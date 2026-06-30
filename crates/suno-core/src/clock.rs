//! The clock port: the executor's only source of delay.
//!
//! The download executor waits in two places: between polls for a server-side
//! WAV render, and as backoff after a rate-limit or transient failure. Both go
//! through this trait so the wait is injected, never taken from the wall clock.
//! The CLI adapter sleeps with the async runtime; tests use a double that
//! returns immediately and records the requested delays, keeping every test
//! deterministic with no real sleeping.

use std::future::Future;
use std::time::Duration;

/// The delay port the executor waits through.
pub trait Clock {
    /// Wait for `duration`, then resolve.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}
