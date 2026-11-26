use super::*;
use http_body_util::Full;
use hyper::body::{Frame, SizeHint};
use std::{
    collections::VecDeque,
    hint::black_box,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

struct Frames(VecDeque<Result<Frame<Bytes>, io::Error>>);
impl Body for Frames {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        Poll::Ready(self.0.pop_front())
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}
fn chunk(data: &'static [u8]) -> Result<Frame<Bytes>, io::Error> {
    Ok(Frame::data(Bytes::from_static(data)))
}

#[tokio::test]
async fn decoded_length_single_buffer_fragmented_and_failed_bodies() {
    let sample = Bytes::copy_from_slice(b"hello");
    let pointer = sample.as_ptr();
    assert_eq!(
        collect(Full::new(sample), 5).await.unwrap().as_ptr(),
        pointer
    );
    assert_eq!(
        collect(Full::new(Bytes::from_static(b"large")), 4)
            .await
            .unwrap_err()
            .code(),
        crate::too_large(4).code()
    );
    let frames = || {
        Frames(
            [
                chunk(b"a"),
                chunk(b""),
                chunk(b"bc"),
                Ok(Frame::trailers(http::HeaderMap::new())),
                chunk(b"d"),
            ]
            .into(),
        )
    };
    let reference = Limited::new(frames(), 4)
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(collect(frames(), 4).await.unwrap(), reference);
    assert_eq!(
        collect(frames(), 3).await.unwrap_err().code(),
        crate::too_large(3).code()
    );
    let broken = Frames([chunk(b"a"), Err(io::Error::other("transport closed"))].into());
    assert_eq!(
        collect(broken, 100).await.unwrap_err().code(),
        "invalid_request"
    );
    assert!(
        collect(Frames(VecDeque::new()), 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "local release comparison with http-body-util's collected buffer"]
async fn compare_bounded_aggregation() {
    let bytes = Bytes::from(vec![0x53; 4096]);
    let mut per_body = [Vec::new(), Vec::new()];
    for round in 0..21 {
        for mode in [round % 2, 1 - round % 2] {
            let start = Instant::now();
            for _ in 0..20_000 {
                let body = Full::new(black_box(bytes.clone()));
                let result = if mode == 0 {
                    collect(body, 65536).await.unwrap()
                } else {
                    Limited::new(body, 65536)
                        .collect()
                        .await
                        .unwrap()
                        .to_bytes()
                };
                black_box(result);
            }
            per_body[mode].push(start.elapsed().as_nanos() / 20_000);
        }
    }
    for (label, mut samples) in ["bounded", "collect/to_bytes"].into_iter().zip(per_body) {
        samples.sort_unstable();
        println!(
            "{label} ns per 4 KiB body median {} ({}..{})",
            samples[10], samples[0], samples[20]
        );
    }
}
