//! The clock adapter: the engine's [`Clock`] port realised with the async
//! runtime's timer.

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use suno_core::Clock;

/// A clock that sleeps on the tokio runtime.
pub struct TokioClock;

impl Clock for TokioClock {
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(duration)
    }

    fn now_unix(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}
