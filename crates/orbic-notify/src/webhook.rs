use base64::{Engine, engine::general_purpose::STANDARD};
use hyve::{HyveError, HyveResult, Timestamp};
use orbic_core::{Body, DeliverySignature, Extract, Parts, Rail};
use orbic_db::{Db, Sql, Tx, bind, projection};
use ring::{digest, hmac};
use std::future::Future;

/// An unverified inbound delivery with the exact bytes the sender signed.
/// Use as a POST handler argument and pass it to [`Webhooks::accept`].
pub struct Delivery {
    meta: DeliverySignature,
    body: Vec<u8>,
}
impl Delivery {
    /// Reconstructs a delivery when mounting the verifier in another HTTP adapter.
    pub fn new(id: &str, timestamp: &str, signatures: &str, body: Vec<u8>) -> Self {
        Self {
            meta: DeliverySignature {
                id: id.into(),
                timestamp: timestamp.into(),
                signatures: signatures.into(),
            },
            body,
        }
    }
}
impl Extract for Delivery {
    fn extract(parts: &mut Parts) -> HyveResult<Self> {
        if parts.rail != Rail::Standard {
            return Err(denied());
        }
        let meta = parts.meta.delivery_signature.take().ok_or_else(denied)?;
        let body = match parts.take_body() {
            Body::Bytes(bytes) => bytes,
            Body::Empty => Vec::new(),
            Body::Value(_) => return Err(denied()),
        };
        Ok(Self { meta, body })
    }
}
/// Outcome for an authenticated delivery.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The application callback and delivery ID were committed.
    Applied,
    /// This source, ID and body were already committed.
    Duplicate,
}
fn denied() -> HyveError {
    HyveError::domain("unauthenticated", "invalid webhook credentials")
}
projection!(Previous,"orbic_webhook_deliveries", digest: Vec<u8>);

/// Standard Webhooks HMAC-SHA256 authentication, replay window and durable
/// idempotency. Signing keys can overlap while a provider rotates them.
#[derive(Clone)]
pub struct Webhooks {
    db: Db,
    source: String,
    keys: Vec<hmac::Key>,
}
impl Webhooks {
    /// Configures an app-selected source namespace and up to four base64
    /// `whsec_` secrets, then installs the delivery ledger.
    pub async fn new(db: Db, source: &str, secrets: &[&str]) -> HyveResult<Self> {
        if source.is_empty() || source.len() > 128 || secrets.is_empty() || secrets.len() > 4 {
            return Err(HyveError::invalid_request(
                "invalid webhook source or key count",
            ));
        }
        let mut keys = Vec::new();
        for secret in secrets {
            let key = secret
                .strip_prefix("whsec_")
                .and_then(|s| STANDARD.decode(s).ok())
                .filter(|k| k.len() >= 16)
                .ok_or_else(|| HyveError::invalid_request("invalid webhook signing key"))?;
            keys.push(hmac::Key::new(hmac::HMAC_SHA256, &key));
        }
        db.execute_script("CREATE TABLE IF NOT EXISTS orbic_webhook_deliveries(source TEXT NOT NULL,delivery_id TEXT NOT NULL,digest BYTEA NOT NULL,PRIMARY KEY(source,delivery_id))").await?;
        Ok(Self {
            db,
            source: source.into(),
            keys,
        })
    }
    /// Authenticates before decoding, accepts timestamps within five minutes
    /// and executes the application's changes in the same transaction as the
    /// replay record. Retry after a failed callback will run the callback again.
    pub async fn accept<F, Fut>(&self, delivery: Delivery, apply: F) -> HyveResult<DeliveryOutcome>
    where
        F: FnOnce(Tx, Vec<u8>) -> Fut,
        Fut: Future<Output = HyveResult<()>>,
    {
        let Delivery { meta, body } = delivery;
        if body.len() > 1024 * 1024
            || meta.id.is_empty()
            || meta.id.len() > 128
            || !meta.id.bytes().all(|b| b.is_ascii_graphic())
            || meta.timestamp.len() > 20
            || !meta.timestamp.bytes().all(|b| b.is_ascii_digit())
            || meta.signatures.len() > 4096
        {
            return Err(denied());
        }
        let seconds = meta.timestamp.parse::<i64>().map_err(|_| denied())?;
        if seconds.abs_diff(Timestamp::now().as_unix_micros() / 1_000_000) > 300 {
            return Err(denied());
        }
        let mut message = format!("{}.{}.", meta.id, meta.timestamp).into_bytes();
        message.extend_from_slice(&body);
        let valid = meta
            .signatures
            .split_ascii_whitespace()
            .filter_map(|s| s.strip_prefix("v1,"))
            .filter_map(|s| STANDARD.decode(s).ok())
            .any(|tag| {
                tag.len() == 32
                    && self
                        .keys
                        .iter()
                        .any(|key| hmac::verify(key, &message, &tag).is_ok())
            });
        if !valid {
            return Err(denied());
        }
        let hash = digest::digest(&digest::SHA256, &body).as_ref().to_vec();
        self.db.tx(|tx| async move {
            let changed=Sql::<()>::new("INSERT INTO orbic_webhook_deliveries(source,delivery_id,digest) VALUES($1,$2,$3) ON CONFLICT(source,delivery_id) DO NOTHING",
                &["sqlite","postgres"],&[],vec![bind(&self.source),bind(&meta.id),bind(&hash)]).execute(&tx).await?;
            if changed==0 {
                let prior=Sql::<Previous>::new("SELECT digest FROM orbic_webhook_deliveries WHERE source=$1 AND delivery_id=$2",
                    &["sqlite","postgres"],&[],vec![bind(&self.source),bind(&meta.id)]).fetch_one(&tx).await?;
                if prior.digest!=hash {return Err(HyveError::domain("idempotency_key_mismatch","webhook ID has different content"));}
                return Ok(DeliveryOutcome::Duplicate);
            }
            apply(tx,body).await?;
            Ok(DeliveryOutcome::Applied)
        }).await
    }
}
