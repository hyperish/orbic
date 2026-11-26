//! Adapted from utoipa 6.0.0 Paths::add_path_operation in src/openapi/path.rs
//! (baa4d04c78fdcc86622e1a2775039fc1621157c0, MIT, see LICENSE-MIT).
//! An Orbic route has exactly one method. Move the operation and template
//! into their final entry, retaining sibling methods registered on that path.
use hyve::api::Method;
use utoipa::openapi::{Paths, path::Operation};

pub(super) fn add(paths: &mut Paths, path: String, method: Method, op: Operation) {
    let item = paths.paths.entry(path).or_default();
    let slot = match method {
        Method::Get => &mut item.get,
        Method::Post => &mut item.post,
        Method::Patch => &mut item.patch,
        Method::Delete => &mut item.delete,
    };
    *slot = Some(op);
}

#[cfg(test)]
mod tests {
    use super::*;
    use utoipa::openapi::path::{HttpMethod, OperationBuilder};

    fn build(local: bool) -> Paths {
        let mut paths = Paths::new();
        for i in 0..64 {
            for (method, original) in [
                (Method::Get, HttpMethod::Get),
                (Method::Post, HttpMethod::Post),
                (Method::Patch, HttpMethod::Patch),
                (Method::Delete, HttpMethod::Delete),
                (Method::Get, HttpMethod::Get),
            ] {
                let path = format!("/records/{i}/{{id}}");
                let op = OperationBuilder::new()
                    .summary(Some("project record"))
                    .operation_id(Some(format!("route-{i}-{method:?}")))
                    .description(Some("record".repeat(24)))
                    .build();
                if local {
                    add(&mut paths, path, method, op);
                } else {
                    paths.add_path_operation(path, vec![original], op);
                }
            }
        }
        paths
    }

    #[test]
    fn methods_replace_only_their_own_slot() {
        assert!(build(true) == build(false));
    }

    #[test]
    #[ignore = "timing comparison, run with --release --ignored --nocapture"]
    fn benchmark_mixed_method_paths() {
        let mut samples = [Vec::new(), Vec::new()];
        for trial in 0..21 {
            for i in [trial % 2, 1 - trial % 2] {
                let start = std::time::Instant::now();
                for _ in 0..24 {
                    std::hint::black_box(build(i == 0));
                }
                samples[i].push(start.elapsed().as_micros() / 24);
            }
        }
        for (label, mut times) in ["single-method", "utoipa"].into_iter().zip(samples) {
            times.sort_unstable();
            println!(
                "{label}: {} µs/document ({}–{})",
                times[10], times[0], times[20]
            );
        }
    }
}
