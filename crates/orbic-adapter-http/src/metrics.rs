//! `/metrics` counters, in the Prometheus text format (0.0.4).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use http::StatusCode;

/// The `Content-Type` of `/metrics`.
pub(crate) const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Default)]
pub(crate) struct Metrics {
    /// Answered requests by status class (1xx..5xx).
    classes: [AtomicU64; 5],
    in_flight: AtomicU64,
    micros: AtomicU64,
    count: AtomicU64,
}

/// One request being handled; counts as in flight until dropped.
pub(crate) struct Timer<'a> {
    metrics: &'a Metrics,
    start: Instant,
}

impl Metrics {
    pub(crate) fn start(&self) -> Timer<'_> {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        Timer {
            metrics: self,
            start: Instant::now(),
        }
    }

    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("# HELP orbic_http_requests_total Requests answered, by status class.\n");
        out.push_str("# TYPE orbic_http_requests_total counter\n");
        for (i, n) in self.classes.iter().enumerate() {
            let _ = writeln!(
                out,
                "orbic_http_requests_total{{class=\"{}xx\"}} {}",
                i + 1,
                n.load(Ordering::Relaxed)
            );
        }
        out.push_str("# HELP orbic_http_requests_in_flight Requests being handled.\n");
        out.push_str("# TYPE orbic_http_requests_in_flight gauge\n");
        let _ = writeln!(
            out,
            "orbic_http_requests_in_flight {}",
            self.in_flight.load(Ordering::Relaxed)
        );
        out.push_str("# HELP orbic_http_request_duration_seconds Time to answer requests.\n");
        out.push_str("# TYPE orbic_http_request_duration_seconds summary\n");
        let micros = self.micros.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "orbic_http_request_duration_seconds_sum {}.{:06}",
            micros / 1_000_000,
            micros % 1_000_000
        );
        let _ = writeln!(
            out,
            "orbic_http_request_duration_seconds_count {}",
            self.count.load(Ordering::Relaxed)
        );
        out
    }
}

impl Timer<'_> {
    /// Records the answer.
    pub(crate) fn done(self, status: StatusCode) {
        let class = usize::from(status.as_u16() / 100).clamp(1, 5) - 1;
        self.metrics.classes[class].fetch_add(1, Ordering::Relaxed);
        let micros = u64::try_from(self.start.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.metrics.micros.fetch_add(micros, Ordering::Relaxed);
        self.metrics.count.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        self.metrics.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}
