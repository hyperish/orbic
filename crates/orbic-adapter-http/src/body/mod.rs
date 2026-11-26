//! Bounded request aggregation. The two-chunk lookahead and capped reserve derive from
//! hyper 0.14.32 `src/body/to_bytes.rs`, commit d7bab92149e41612bcb4000e86b2734f657f433b
//! (MIT, see LICENSE). This adapter consumes hyper 1.x frames, reuses decoded lengths,
//! and keeps the streaming limit active even when the body has no known length.
use bytes::Bytes;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::body::Body;
use hyve::{HyveError, HyveResult};
use std::error::Error;

type BodyError = Box<dyn Error + Send + Sync>;

pub(super) async fn collect<B>(body: B, limit: usize) -> HyveResult<Bytes>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Into<BodyError>,
{
    if body.size_hint().lower() > limit as u64 {
        return Err(crate::too_large(limit));
    }
    let mut body = Limited::new(body, limit);
    let Some(first) = next_data(&mut body, limit).await? else {
        return Ok(Bytes::new());
    };
    let Some(second) = next_data(&mut body, limit).await? else {
        return Ok(first);
    };
    let remaining = body.size_hint().lower().min(16 * 1024) as usize;
    // Limited has checked both chunks against the same running byte budget.
    let capacity = (first.len() + second.len())
        .saturating_add(remaining)
        .min(limit);
    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(&first);
    output.extend_from_slice(&second);
    while let Some(bytes) = next_data(&mut body, limit).await? {
        output.extend_from_slice(&bytes);
    }
    Ok(Bytes::from(output))
}

async fn next_data<B>(body: &mut B, limit: usize) -> HyveResult<Option<Bytes>>
where
    B: Body<Data = Bytes, Error = BodyError> + Unpin,
{
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| {
            if error.is::<LengthLimitError>() {
                crate::too_large(limit)
            } else {
                HyveError::invalid_request("the request body could not be read")
            }
        })?;
        if let Ok(bytes) = frame.into_data()
            && !bytes.is_empty()
        {
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
