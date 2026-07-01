//! The clock adapter: the engine's [`Clock`] port realised with the async
//! runtime's timer.

use std::future::Future;
use std::time::Duration;

use suno_core::Clock;

/// A clock that sleeps on the tokio runtime.
pub struct TokioClock;

impl Clock for TokioClock {
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(duration)
    }
}
