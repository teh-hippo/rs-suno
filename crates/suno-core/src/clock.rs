//! The clock port: the engine's only source of time.
//!
//! The download executor waits in two places: between polls for a server-side
//! WAV render, and as backoff after a rate-limit or transient failure. Both go
//! through this trait so the wait is injected, never taken from the wall clock.
//! `now_unix` provides the current Unix timestamp for JWT expiry checks.
//! The CLI adapter sleeps with the async runtime; tests use a double that
//! returns immediately and records the requested delays, keeping every test
//! deterministic with no real sleeping.

use std::future::Future;
use std::time::Duration;

/// The time and delay port.
///
/// `Sync` so a `&SunoClient<impl Clock>` held across an `.await` stays `Send`.
pub trait Clock: Sync {
    /// Wait for `duration`, then resolve.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send;

    /// Current time as seconds since the Unix epoch.
    fn now_unix(&self) -> i64;
}
