//! Warns when an account's pasted `__client` cookie is nearing its own expiry.
//!
//! The cookie is itself a long-lived JWT; decoding its `exp` lets us nudge the
//! user to re-paste a fresh one before it dies. The message carries only the
//! account label and a day count, never the token.

use suno_core::{ClerkAuth, TOKEN_EXPIRY_WARN_DAYS, TokenExpiry};

use crate::cli::run;

const DAY_SECS: i64 = 86_400;

/// Render an expiry warning for `label`, or `None` when nothing needs saying.
///
/// `Fresh` and `Unknown` stay silent; `Expiring`/`Expired` return a one-line
/// message that leads with the real remedy and never suggests `auth refresh`,
/// which only re-mints a JWT from the same cookie and cannot move the deadline.
pub fn token_expiry_message(label: &str, expiry: TokenExpiry) -> Option<String> {
    let remedy = format!(
        "Get a fresh __client cookie from your browser and update it: \
set SUNO_TOKEN (or --token), or update the token for [accounts.{label}] in your config."
    );
    match expiry {
        TokenExpiry::Fresh | TokenExpiry::Unknown => None,
        TokenExpiry::Expiring { days } => Some(format!(
            "warning: the token for account '{label}' expires in {days} day(s). {remedy}"
        )),
        TokenExpiry::Expired => Some(format!(
            "warning: the token for account '{label}' has expired. {remedy}"
        )),
    }
}

/// Print an expiry warning to stderr when the token is stale and output is not
/// silenced, using the CLI clock to keep `suno-core` clock-free.
pub fn warn_token_expiry(label: &str, auth: &ClerkAuth, verbosity: i8) {
    if verbosity < -1 {
        return;
    }
    let now = run::now_secs() as i64;
    let window = TOKEN_EXPIRY_WARN_DAYS * DAY_SECS;
    if let Some(message) = token_expiry_message(label, auth.token_expiry(now, window)) {
        eprintln!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_and_unknown_stay_silent() {
        assert!(token_expiry_message("alice", TokenExpiry::Fresh).is_none());
        assert!(token_expiry_message("alice", TokenExpiry::Unknown).is_none());
    }

    #[test]
    fn expiring_carries_label_and_day_count() {
        let message = token_expiry_message("alice", TokenExpiry::Expiring { days: 3 }).unwrap();
        assert!(message.contains("alice"));
        assert!(message.contains('3'));
        assert!(!message.contains("auth refresh"));
    }

    #[test]
    fn expired_carries_label() {
        let message = token_expiry_message("bob", TokenExpiry::Expired).unwrap();
        assert!(message.contains("bob"));
        assert!(message.contains("expired"));
    }

    #[test]
    fn message_never_leaks_a_token() {
        for expiry in [TokenExpiry::Expiring { days: 1 }, TokenExpiry::Expired] {
            let message = token_expiry_message("acct", expiry).unwrap();
            assert!(
                !message.contains("eyJ"),
                "message leaked a JWT/cookie: {message}"
            );
        }
    }
}
