//! Time source for expiry, windows and retention, replaceable in tests.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The time the stores read: the system clock, or a [`ManualClock`].
#[derive(Clone, Default)]
pub struct Clock {
    manual: Option<Arc<Mutex<Instant>>>,
}

impl Clock {
    /// The system's monotonic clock.
    pub fn system() -> Self {
        Clock::default()
    }

    /// The current instant.
    pub fn now(&self) -> Instant {
        match &self.manual {
            None => Instant::now(),
            Some(t) => *t.lock().unwrap_or_else(PoisonError::into_inner),
        }
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.manual.is_some() {
            "Clock(manual)"
        } else {
            "Clock(system)"
        })
    }
}

/// A clock that moves only when told to, for tests of TTLs and windows.
#[derive(Clone, Debug)]
pub struct ManualClock(Arc<Mutex<Instant>>);

impl ManualClock {
    /// Starts at the current instant.
    pub fn new() -> Self {
        ManualClock(Arc::new(Mutex::new(Instant::now())))
    }

    /// Moves time forward.
    pub fn advance(&self, by: Duration) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) += by;
    }

    /// A [`Clock`] reading this time.
    pub fn clock(&self) -> Clock {
        Clock {
            manual: Some(self.0.clone()),
        }
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}
