//! The fixed RESP3 reply produced by the cache claim script. Adapted from fred 10.1.0
//! `modules/response.rs` (tuple and Bytes conversion), 29d4790e8522a3b0c67531081fc0488585dbc665.
//! MIT, see LICENSE-MIT. Read the discriminator first and retain the archive's byte owner.
use bytes::Bytes;
use fred::{
    error::{Error, ErrorKind},
    prelude::{FromValue, Value},
};
use std::time::Duration;

#[derive(Debug)]
pub(super) enum Claim {
    Wait(String),
    Load,
    Read {
        revision: String,
        schema: Bytes,
        data: Bytes,
        ttl: Option<Duration>,
    },
}
impl FromValue for Claim {
    fn from_value(value: Value) -> Result<Self, Error> {
        let Value::Array(values) = value else {
            return Err(Error::new(ErrorKind::Parse, "expected a cache reply array"));
        };
        let [state, revision, schema, data, remaining]: [Value; 5] = values
            .try_into()
            .map_err(|_| Error::new(ErrorKind::Parse, "expected five cache reply fields"))?;
        match state {
            Value::Integer(0) => Ok(Self::Wait(revision.convert()?)),
            Value::Integer(2) => Ok(Self::Load),
            Value::Integer(code @ (1 | 3)) => {
                let ttl = if code == 1 {
                    Some(Duration::from_millis(
                        remaining.convert::<i64>()?.max(0) as u64
                    ))
                } else {
                    None
                };
                Ok(Self::Read {
                    revision: revision.convert()?,
                    schema: schema.convert()?,
                    data: data.convert()?,
                    ttl,
                })
            }
            _ => Err(Error::new(ErrorKind::Parse, "unknown cache claim reply")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type GenericReply = (i64, String, String, Bytes, i64);
    fn response(data: Bytes) -> Value {
        Value::Array(vec![
            1.into(),
            "revision".into(),
            "model-shape".into(),
            data.into(),
            350.into(),
        ])
    }
    #[test]
    fn claim_reuses_the_resp_buffer_and_checks_the_reply_shape() {
        let body = Bytes::from(vec![42; 2048]);
        let response = response(body.clone());
        let expected = GenericReply::from_value(response.clone()).unwrap();
        let Claim::Read {
            revision,
            schema,
            data,
            ttl,
        } = Claim::from_value(response).unwrap()
        else {
            panic!("read");
        };
        assert_eq!(data.as_ptr(), body.as_ptr());
        assert_eq!(
            (revision.as_str(), schema.as_ref(), &data),
            (expected.1.as_str(), expected.2.as_bytes(), &expected.3)
        );
        assert_eq!(ttl, Some(Duration::from_millis(expected.4 as u64)));
        assert!(Claim::from_value(Value::Array(vec![0.into()])).is_err());
    }

    #[test]
    #[ignore = "local release comparison against the fred five-field tuple"]
    fn compare_claim_decode() {
        let record = response(Bytes::from(vec![42; 4096]));
        let mut periods = [Vec::new(), Vec::new()];
        for round in 0..21 {
            for mode in [round % 2, 1 - round % 2] {
                let now = std::time::Instant::now();
                for _ in 0..20_000 {
                    let value = std::hint::black_box(record.clone());
                    if mode == 0 {
                        std::hint::black_box(Claim::from_value(value).unwrap());
                    } else {
                        std::hint::black_box(GenericReply::from_value(value).unwrap());
                    }
                }
                periods[mode].push(now.elapsed().as_nanos() / 20_000);
            }
        }
        for (label, mut times) in ["cache reply", "fred tuple"].into_iter().zip(periods) {
            times.sort_unstable();
            println!(
                "{label} ns per claim median {} ({}..{})",
                times[10], times[0], times[20]
            );
        }
    }
}
