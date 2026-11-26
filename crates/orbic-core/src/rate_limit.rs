//! `RateLimit`: a guard counting requests per principal or client address
//! (`docs/01-dx-and-dsl.md` §1, `RateLimit`의 키).

use std::time::Duration;

use hyve_api::{BoxFuture, ContextNeed};
use hyve_core::{HyveError, HyveResult};
use orbic_cache::Cache;

use crate::guard::{Guard, GuardCtx};
use crate::request::{Principal, RateLimitInfo};

/// Allows `limit` requests per window to each principal (or, before
/// authentication, each client address) across the routes of the component
/// it wraps; more fail with `429 rate_limited` until the window resets.
/// Responses of requests that reach it carry the remaining count. Counters
/// live in the app's `Cache`.
#[derive(Debug)]
pub struct RateLimit {
    limit: u32,
    window: Duration,
}

impl RateLimit {
    /// `limit` requests per `window`.
    ///
    /// # Panics
    ///
    /// If `limit` or `window` is zero.
    pub fn new(limit: u32, window: Duration) -> Self {
        assert!(limit > 0, "RateLimit: the limit must be at least 1");
        assert!(
            !window.is_zero(),
            "RateLimit: the window must be longer than zero"
        );
        RateLimit { limit, window }
    }

    /// `limit` requests per second.
    pub fn per_second(limit: u32) -> Self {
        Self::new(limit, Duration::from_secs(1))
    }

    /// `limit` requests per minute.
    pub fn per_minute(limit: u32) -> Self {
        Self::new(limit, Duration::from_secs(60))
    }

    /// `limit` requests per hour.
    pub fn per_hour(limit: u32) -> Self {
        Self::new(limit, Duration::from_secs(3600))
    }
}

impl Guard for RateLimit {
    fn needs(&self) -> Vec<ContextNeed> {
        vec![ContextNeed::of::<Cache>()]
    }

    fn describe(&self) -> crate::GuardShape {
        crate::GuardShape {
            authenticates: false,
            errors: vec!["rate_limited"],
        }
    }

    fn check<'a>(&'a self, cx: &'a mut GuardCtx<'_>) -> BoxFuture<'a, HyveResult<()>> {
        Box::pin(async move {
            let cache = cx.scope.require::<Cache>()?;
            let subject = match (cx.scope.get::<Principal>(), cx.meta.client_addr) {
                (Some(p), _) => format!("principal:{}", p.as_str()),
                (None, Some(addr)) => format!("addr:{addr}"),
                (None, None) => "addr:unknown".to_owned(),
            };
            let d = cache
                .count_request(cx.guard_key, &subject, self.limit, self.window)
                .await?;
            let reset_secs = d.reset.as_secs() + u64::from(d.reset.subsec_nanos() > 0);
            let info = RateLimitInfo {
                limit: d.limit,
                remaining: d.remaining,
                reset_secs,
            };
            // With nested limits, report the tightest.
            if cx
                .response
                .rate_limit
                .is_none_or(|prev| info.remaining <= prev.remaining)
            {
                cx.response.rate_limit = Some(info);
            }
            if d.allowed {
                Ok(())
            } else {
                cx.response.retry_after = Some(reset_secs);
                Err(HyveError::domain(
                    "rate_limited",
                    format!("too many requests; retry in {reset_secs} s"),
                )
                .retryable(true))
            }
        })
    }
}
