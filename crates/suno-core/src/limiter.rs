//! Adaptive AIMD rate limiter: auto-discovers Suno's request rate.
//!
//! The listing [`SunoClient`](crate::SunoClient) paces its requests to a rate it
//! discovers rather than a hand-tuned constant. Every request is paced to the
//! current rate; a `429` halves the rate (multiplicative decrease) and records
//! the rate that tripped it as a ceiling; a run of clean successes ramps the
//! rate back up (additive-ish increase), capped below that ceiling. The maths is
//! pure and clock-free: [`pace`](AdaptiveLimiter::pace) only returns the delay,
//! which the caller waits out through the [`Clock`](crate::Clock) port, so the
//! engine still sleeps nowhere itself.
//!
//! Mirrors the reference `AdaptiveLimiter` from `mvanhorn/printing-press-library`.

use std::time::Duration;

/// Never pace slower than this: one request every two seconds is the hard floor.
pub(crate) const RATE_FLOOR: f64 = 0.5;
/// Ramp the rate up after this many consecutive clean successes.
pub(crate) const RAMP_AFTER: u32 = 10;
/// Multiplicative decrease applied to the rate on a `429`.
const DECREASE_FACTOR: f64 = 0.5;
/// Geometric increase applied to the rate after [`RAMP_AFTER`] successes.
const INCREASE_FACTOR: f64 = 1.25;
/// Keep a ramped rate below this fraction of the ceiling that last tripped a
/// `429`, so the rate settles just under the discovered limit.
const CEILING_MARGIN: f64 = 0.9;

/// Wait this long after a `429` that carries no usable `Retry-After`.
pub(crate) const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(5);
/// Hard cap on any honoured `Retry-After`, so a buggy or hostile upstream cannot
/// pin a walk for minutes.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// An AIMD limiter over a requests-per-second rate.
///
/// Constructed at an initial rate, floored at [`RATE_FLOOR`]. It carries no
/// notion of wall-clock time: state advances only through
/// [`on_success`](Self::on_success), [`on_rate_limit`](Self::on_rate_limit), and
/// [`pace`](Self::pace), which reports the delay to wait before the next
/// request. Integrated at the single per-request layer, it is independent of how
/// a listing is paged, so it composes with a page or cursor walk alike.
pub(crate) struct AdaptiveLimiter {
    rate: f64,
    floor: f64,
    ceiling: Option<f64>,
    successes: u32,
    primed: bool,
}

impl AdaptiveLimiter {
    /// A limiter starting at `initial_rate` requests per second.
    ///
    /// The floor is [`RATE_FLOOR`], or `initial_rate` when that is already below
    /// the floor, so a deliberately slow start is never overridden upward.
    pub(crate) fn new(initial_rate: f64) -> Self {
        let floor = RATE_FLOOR.min(initial_rate);
        Self {
            rate: initial_rate.max(floor),
            floor,
            ceiling: None,
            successes: 0,
            primed: false,
        }
    }

    /// The delay to wait before the next request at the current rate.
    ///
    /// The first call after construction returns [`Duration::ZERO`] so a cold
    /// start is not penalised, mirroring the reference's zero-value last-request
    /// time; every later call returns the full inter-request delay `1 / rate`.
    pub(crate) fn pace(&mut self) -> Duration {
        if !self.primed {
            self.primed = true;
            return Duration::ZERO;
        }
        Duration::from_secs_f64(1.0 / self.rate)
    }

    /// The current rate in requests per second.
    #[cfg(test)]
    pub(crate) fn rate(&self) -> f64 {
        self.rate
    }

    /// Record a clean success, ramping the rate up once [`RAMP_AFTER`]
    /// consecutive successes have accrued. A ramp is capped at
    /// [`CEILING_MARGIN`] of the last ceiling that tripped a `429`.
    pub(crate) fn on_success(&mut self) {
        self.successes += 1;
        if self.successes < RAMP_AFTER {
            return;
        }
        let mut ramped = self.rate * INCREASE_FACTOR;
        if let Some(ceiling) = self.ceiling {
            ramped = ramped.min(ceiling * CEILING_MARGIN);
        }
        self.rate = ramped.max(self.floor);
        self.successes = 0;
    }

    /// Record a `429`: halve the rate (floored), and remember the rate that
    /// tripped it as the ceiling to ramp back under.
    pub(crate) fn on_rate_limit(&mut self) {
        self.ceiling = Some(self.rate);
        self.rate = (self.rate * DECREASE_FACTOR).max(self.floor);
        self.successes = 0;
    }
}

/// The delay to wait after a `429` before retrying: the honoured `Retry-After`
/// (capped at [`MAX_RETRY_AFTER`]) or [`DEFAULT_RETRY_AFTER`] when absent.
///
/// Complements [`AdaptiveLimiter::on_rate_limit`]: the limiter lowers the future
/// rate, while this bounds the wait before the failed request is retried.
pub(crate) fn retry_after_delay(retry_after: Option<Duration>) -> Duration {
    retry_after
        .unwrap_or(DEFAULT_RETRY_AFTER)
        .min(MAX_RETRY_AFTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_request_is_free_then_paces_to_the_rate() {
        let mut limiter = AdaptiveLimiter::new(2.0);
        assert_eq!(limiter.pace(), Duration::ZERO);
        assert_eq!(limiter.pace(), Duration::from_millis(500));
    }

    #[test]
    fn a_rate_limit_halves_the_rate_and_records_a_ceiling() {
        let mut limiter = AdaptiveLimiter::new(4.0);
        limiter.on_rate_limit();
        assert_eq!(limiter.rate(), 2.0);
        let _ = limiter.pace();
        assert_eq!(limiter.pace(), Duration::from_millis(500));
    }

    #[test]
    fn the_rate_never_drops_below_the_floor() {
        let mut limiter = AdaptiveLimiter::new(1.0);
        for _ in 0..10 {
            limiter.on_rate_limit();
        }
        assert_eq!(limiter.rate(), RATE_FLOOR);
        let _ = limiter.pace();
        assert_eq!(limiter.pace(), Duration::from_secs(2));
    }

    #[test]
    fn ramps_up_only_after_ten_consecutive_successes() {
        let mut limiter = AdaptiveLimiter::new(2.0);
        for _ in 0..(RAMP_AFTER - 1) {
            limiter.on_success();
        }
        assert_eq!(limiter.rate(), 2.0);
        limiter.on_success();
        assert!((limiter.rate() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn a_success_streak_resets_after_a_rate_limit() {
        let mut limiter = AdaptiveLimiter::new(2.0);
        for _ in 0..(RAMP_AFTER - 1) {
            limiter.on_success();
        }
        limiter.on_rate_limit();
        assert_eq!(limiter.rate(), 1.0);
        for _ in 0..(RAMP_AFTER - 1) {
            limiter.on_success();
        }
        assert_eq!(limiter.rate(), 1.0);
    }

    #[test]
    fn a_ramp_is_capped_below_the_last_ceiling() {
        let mut limiter = AdaptiveLimiter::new(4.0);
        limiter.on_rate_limit();
        assert_eq!(limiter.rate(), 2.0);
        // The ceiling is the 4.0 that tripped the 429, so the rate settles just
        // under 0.9 * 4.0 = 3.6 no matter how long the success streak runs.
        for _ in 0..(RAMP_AFTER * 20) {
            limiter.on_success();
        }
        assert!((limiter.rate() - 3.6).abs() < 1e-9);
    }

    #[test]
    fn retry_after_defaults_when_absent_and_caps_when_long() {
        assert_eq!(retry_after_delay(None), DEFAULT_RETRY_AFTER);
        assert_eq!(
            retry_after_delay(Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
        assert_eq!(
            retry_after_delay(Some(Duration::from_secs(600))),
            MAX_RETRY_AFTER
        );
    }
}
