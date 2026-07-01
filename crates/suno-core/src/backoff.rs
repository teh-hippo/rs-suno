//! Shared retry backoff: exponential delay with a `Retry-After` floor.
//!
//! Both the download executor and the listing [`SunoClient`](crate::SunoClient)
//! ride through Suno's rate limiter with this policy, so the maths lives in one
//! place. The delay is waited out through the [`Clock`](crate::Clock) port, so
//! the engine still sleeps nowhere itself.

use std::time::Duration;

use crate::http::HttpResponse;

/// First backoff step; doubles each retry, capped at [`BACKOFF_CAP`].
pub(crate) const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Hard ceiling on any single backoff, matching the reference integration.
pub(crate) const BACKOFF_CAP: Duration = Duration::from_secs(300);

/// Exponential backoff with a `Retry-After` floor, capped at [`BACKOFF_CAP`].
pub(crate) fn backoff_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let base = BACKOFF_BASE.checked_mul(factor).unwrap_or(BACKOFF_CAP);
    let delay = retry_after.map_or(base, |hint| hint.max(base));
    delay.min(BACKOFF_CAP)
}

/// The `Retry-After` delay in whole seconds, if present and valid.
///
/// Suno sits behind Cloudflare and rarely sends this header, so it is treated as
/// an optional floor over the exponential backoff, never a requirement.
pub(crate) fn retry_after(response: &HttpResponse) -> Option<Duration> {
    let seconds: u64 = response.header("retry-after")?.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_honours_retry_after_and_cap() {
        assert_eq!(backoff_delay(0, None), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, None), Duration::from_secs(4));
        assert_eq!(
            backoff_delay(0, Some(Duration::from_secs(9))),
            Duration::from_secs(9)
        );
        assert_eq!(backoff_delay(40, None), BACKOFF_CAP);
    }

    #[test]
    fn retry_after_parses_or_ignores() {
        let resp = HttpResponse {
            status: 429,
            headers: vec![("Retry-After".to_owned(), "5".to_owned())],
            body: Vec::new(),
        };
        assert_eq!(retry_after(&resp), Some(Duration::from_secs(5)));

        let bare = HttpResponse {
            status: 429,
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert_eq!(retry_after(&bare), None);
    }
}
