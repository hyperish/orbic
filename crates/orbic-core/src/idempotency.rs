//! The `Idempotency-Key` gate between the guards and extraction
//! (`docs/03-adapters-and-pipeline.md` §1).
//!
//! It applies to routes bound to `POST`, on the standard and fast rails, for
//! requests with a key and an authenticated [`Principal`]. Without a
//! principal the response could not be scoped to its owner, so nothing is
//! stored or reused. In-process calls carry no body bytes and no transport
//! retries, so they bypass it.

use std::sync::Arc;

use hyve_core::{ErrorKind, HyveError, HyveResult, Scope};
use hyve_serde::AlignedBytes;
use orbic_cache::{Begin, Cache, IdempotencyKey, Reservation};

use crate::request::{Body, Payload, Principal, Rail, Request, ResponseMeta};

/// Longest accepted key, in bytes.
pub(crate) const MAX_KEY_LEN: usize = 255;

/// Seconds a client waits before retrying a key that is still running.
const IN_PROGRESS_RETRY_SECS: u64 = 1;

/// A finished outcome kept for reuse.
enum Stored {
    Empty,
    Json(String),
    Binary(AlignedBytes),
    Error(HyveError),
}

pub(crate) enum Gate {
    /// No gate for this request.
    Pass,
    /// Run the handler, then [`finish`].
    Run(Reservation),
}

/// Claims the request's key, or answers it from a previous outcome.
pub(crate) fn begin(
    scope: &Scope,
    req: &Request,
    meta: &mut ResponseMeta,
) -> HyveResult<Result<Gate, Payload>> {
    let (Some(key), Some(principal)) = (&req.meta.idempotency_key, scope.get::<Principal>()) else {
        return Ok(Ok(Gate::Pass));
    };
    if req.rail == Rail::InProcess {
        return Ok(Ok(Gate::Pass));
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
    match cache.idempotency().begin(id, digest(req.rail, &req.body)) {
        Begin::Started(r) => Ok(Ok(Gate::Run(r))),
        Begin::Replay(stored) => match stored.downcast_ref::<Stored>() {
            Some(Stored::Empty) => Ok(Err(Payload::Empty)),
            Some(Stored::Json(s)) => Ok(Err(Payload::Json(s.clone()))),
            Some(Stored::Binary(b)) => Ok(Err(Payload::Binary(b.clone()))),
            Some(Stored::Error(e)) => Err(e.clone()),
            None => Err(HyveError::internal(
                "idempotency store holds a foreign value",
            )),
        },
        Begin::InProgress => {
            meta.retry_after = Some(IN_PROGRESS_RETRY_SECS);
            Err(HyveError::domain(
                "request_in_progress",
                "a request with this `Idempotency-Key` is still being processed",
            ))
        }
        Begin::Mismatch => Err(HyveError::domain(
            "idempotency_key_mismatch",
            "this `Idempotency-Key` was used with a different request body or media type",
        )),
    }
}

/// Stores successes and `Domain` errors; other errors release the key so the
/// request can be retried.
pub(crate) fn finish(reservation: Reservation, outcome: &HyveResult<Payload>) {
    let stored = match outcome {
        Ok(Payload::Empty) => Stored::Empty,
        Ok(Payload::Json(s)) => Stored::Json(s.clone()),
        Ok(Payload::Binary(b)) => Stored::Binary(b.clone()),
        Ok(Payload::Value(_)) => return,
        Err(e) if e.kind() == ErrorKind::Domain => Stored::Error(e.clone()),
        Err(_) => return,
    };
    reservation.complete(Arc::new(stored));
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
