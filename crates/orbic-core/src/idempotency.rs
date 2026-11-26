//! The `Idempotency-Key` gate between the guards and extraction
//! (`docs/03-adapters-and-pipeline.md` §1).
//!
//! It applies to routes bound to `POST`, on the standard and fast rails, for
//! requests with a key and an authenticated [`Principal`]. Without a
//! principal the response could not be scoped to its owner, so nothing is
//! stored or reused. In-process calls carry no body bytes and no transport
//! retries, so they bypass it.

use hyve_api::ErrorWire;

use hyve_core::{ErrorKind, HyveError, HyveResult, Scope};
use hyve_serde::{AlignedBytes, Binary};
use orbic_cache::{
    Cache, IdempotencyKey, RequestBegin as Begin, RequestReservation as Reservation,
};

use crate::request::{Body, Payload, Principal, Rail, Request, ResponseMeta};

/// Longest accepted key, in bytes.
pub(crate) const MAX_KEY_LEN: usize = 255;

/// Seconds a client waits before retrying a key that is still running.
const IN_PROGRESS_RETRY_SECS: u64 = 1;

pub(crate) enum Gate {
    /// No gate for this request.
    Pass,
    /// Run the handler, then [`finish`].
    Run(Reservation),
}

/// Claims the request's key, or answers it from a previous outcome.
pub(crate) fn prepare(
    scope: &Scope,
    req: &Request,
) -> HyveResult<Option<(Cache, IdempotencyKey, u128)>> {
    let (Some(key), Some(principal)) = (&req.meta.idempotency_key, scope.get::<Principal>()) else {
        return Ok(None);
    };
    if req.rail == Rail::InProcess {
        return Ok(None);
    }
    if key.is_empty() || key.len() > MAX_KEY_LEN || key.chars().any(char::is_control) {
        return Err(HyveError::domain(
            "invalid_request",
            format!("`Idempotency-Key` must be 1 to {MAX_KEY_LEN} bytes of printable text"),
        ));
    }
    let cache = scope.require::<Cache>()?;
    let id = IdempotencyKey {
        principal: principal.as_str().to_owned(),
        method: "POST",
        path: req.path.clone(),
        key: key.clone(),
    };
    Ok(Some(((*cache).clone(), id, digest(req.rail, &req.body))))
}

pub(crate) async fn begin(
    prepared: Option<(Cache, IdempotencyKey, u128)>,
    meta: &mut ResponseMeta,
) -> HyveResult<Result<Gate, Payload>> {
    let Some((cache, id, digest)) = prepared else {
        return Ok(Ok(Gate::Pass));
    };
    match cache.begin_request(id, digest).await? {
        Begin::Started(r) => Ok(Ok(Gate::Run(r))),
        Begin::Replay(bytes) => match bytes.split_first() {
            Some((0, [])) => Ok(Err(Payload::Empty)),
            Some((1, data)) => Ok(Err(Payload::Json(
                std::str::from_utf8(data)
                    .map_err(|_| HyveError::internal("invalid cached JSON reply"))?
                    .to_owned(),
            ))),
            Some((2, data)) => Ok(Err(Payload::Binary(AlignedBytes::copy_from(data)))),
            Some((3, data)) => Err(ErrorWire::from_binary(data)
                .map_err(|_| HyveError::internal("invalid cached domain error"))?
                .into()),
            _ => Err(HyveError::internal("unrecognized cached reply")),
        },
        Begin::InProgress => {
            meta.retry_after = Some(IN_PROGRESS_RETRY_SECS);
            Err(HyveError::domain(
                "request_in_progress",
                "a request with this `Idempotency-Key` is still being processed",
            )
            .retryable(true))
        }
        Begin::Mismatch => Err(HyveError::domain(
            "idempotency_key_mismatch",
            "this `Idempotency-Key` was used with a different request body or media type",
        )),
    }
}

/// Stores successes and `Domain` errors; other errors release the key so the
/// request can be retried.
pub(crate) fn encode(outcome: &HyveResult<Payload>) -> HyveResult<Option<Vec<u8>>> {
    let error;
    let (kind, body): (u8, &[u8]) = match outcome {
        Ok(Payload::Empty) => (0, &[]),
        Ok(Payload::Json(s)) => (1, s.as_bytes()),
        Ok(Payload::Binary(b)) => (2, b),
        Err(e) if e.kind() == ErrorKind::Domain => {
            error = ErrorWire::from(e).to_binary()?;
            (3, &error)
        }
        _ => return Ok(None),
    };
    let mut bytes = Vec::with_capacity(1 + body.len());
    bytes.push(kind);
    bytes.extend_from_slice(body);
    Ok(Some(bytes))
}

/// 128-bit FNV-1a over the rail and the body bytes.
fn digest(rail: Rail, body: &Body) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let tag: &[u8] = match rail {
        Rail::Standard => b"json",
        Rail::Fast => b"binary",
        Rail::InProcess => b"value",
    };
    let bytes: &[u8] = match body {
        Body::Bytes(b) => b,
        _ => &[],
    };
    let mut h = OFFSET;
    for &b in tag.iter().chain(&[0]).chain(bytes) {
        h ^= u128::from(b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_cover_rail_and_body() {
        let b = |s: &[u8]| Body::Bytes(s.to_vec());
        assert_eq!(
            digest(Rail::Standard, &b(b"{}")),
            digest(Rail::Standard, &b(b"{}"))
        );
        assert_ne!(
            digest(Rail::Standard, &b(b"{}")),
            digest(Rail::Fast, &b(b"{}"))
        );
        assert_ne!(
            digest(Rail::Standard, &b(b"{}")),
            digest(Rail::Standard, &b(b"{ }"))
        );
        assert_eq!(
            digest(Rail::Standard, &Body::Empty),
            digest(Rail::Standard, &b(b""))
        );
    }
}
