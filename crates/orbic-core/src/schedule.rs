//! Component-owned UTC timers. The facade starts workers when serving the app.
use chrono::{DateTime, Utc};
use hyve_core::{HyveError, HyveResult, Scope};
use std::{future::Future, pin::Pin, str::FromStr, sync::Arc, time::Duration};

/// A validated fixed interval or UTC cron expression.
#[derive(Clone, Debug)]
pub struct Schedule {
    spec: String,
    timing: Timing,
}

#[derive(Clone, Debug)]
enum Timing {
    Interval(i64),
    Cron(Box<cron::Schedule>),
}

type TaskFn = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = HyveResult<()>> + Send>> + Send + Sync>;

pub(crate) struct Declaration {
    name: String,
    timing: HyveResult<Schedule>,
    run: TaskFn,
}

/// A task registered in a component, including its dependency scope.
#[derive(Clone)]
pub struct ScheduledTask {
    /// Durable schedule identity, scoped to the mounted component.
    pub name: String,
    /// Validated UTC schedule.
    pub timing: Schedule,
    /// Scope containing the dependencies provided by the component.
    pub scope: Scope,
    run: TaskFn,
}

impl ScheduledTask {
    /// Invokes the task once. The caller establishes its per-run scope.
    pub async fn run(&self) -> HyveResult<()> {
        (self.run)().await
    }
}

impl Declaration {
    pub(crate) fn assemble(self, prefix: &str, scope: &Scope) -> HyveResult<ScheduledTask> {
        Ok(ScheduledTask {
            name: format!("{prefix}:{}", self.name),
            timing: self.timing?,
            scope: scope.clone(),
            run: self.run,
        })
    }
}

impl crate::BackendComponent {
    /// Runs a component function periodically, beginning after one interval.
    pub fn every<F, Fut>(self, interval: &str, handler: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HyveResult<()>> + Send + 'static,
    {
        self.scheduled(
            std::any::type_name::<F>(),
            Schedule::every(interval),
            handler,
        )
    }

    /// Registers a named durable timer. Its name survives function renames.
    pub fn scheduled<F, Fut>(mut self, name: &str, timing: HyveResult<Schedule>, handler: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HyveResult<()>> + Send + 'static,
    {
        self.timers.push(Declaration {
            name: name.into(),
            timing: if name.is_empty() {
                Err(HyveError::invalid_request("empty schedule name"))
            } else {
                timing
            },
            run: Arc::new(move || Box::pin(handler())),
        });
        self
    }
}

impl Schedule {
    /// Canonical timing expression used to detect conflicting registrations.
    pub fn expression(&self) -> &str {
        &self.spec
    }

    /// Parses Hyve duration syntax such as `1h30m`.
    pub fn every(interval: &str) -> HyveResult<Self> {
        let duration = hyve_parser::parse_duration(interval)
            .map_err(|d| HyveError::invalid_request(d.summary()))?;
        if duration == Duration::ZERO {
            return Err(HyveError::invalid_request(
                "schedule interval must be positive",
            ));
        }
        let ms = i64::try_from(duration.as_nanos().div_ceil(1_000_000))
            .map_err(|_| HyveError::invalid_request("schedule interval too large"))?;
        Ok(Self {
            spec: format!("every/{ms}"),
            timing: Timing::Interval(ms),
        })
    }

    /// Parses five UTC cron fields, or six with explicit seconds.
    pub fn cron(expression: &str) -> HyveResult<Self> {
        let fields: Vec<_> = expression.split_whitespace().collect();
        let spec = match fields.len() {
            5 => format!("0 {}", fields.join(" ")),
            6 => fields.join(" "),
            _ => {
                return Err(HyveError::invalid_request(
                    "expected five or six UTC cron fields",
                ));
            }
        };
        let schedule = cron::Schedule::from_str(&spec)
            .map_err(|e| HyveError::invalid_request(e.to_string()))?;
        Ok(Self {
            spec: format!("cron/{spec}"),
            timing: Timing::Cron(Box::new(schedule)),
        })
    }

    /// Next firing time in Unix milliseconds after the supplied database time.
    pub fn next(&self, now: i64) -> HyveResult<i64> {
        match &self.timing {
            Timing::Interval(ms) => now
                .checked_add(*ms)
                .ok_or_else(|| HyveError::invalid_request("schedule time is out of range")),
            Timing::Cron(schedule) => DateTime::<Utc>::from_timestamp_millis(now)
                .and_then(|date| schedule.after(&date).next())
                .map(|time| time.timestamp_millis())
                .ok_or_else(|| {
                    HyveError::invalid_request("cron schedule has no future occurrence")
                }),
        }
    }
}
