//! Clerk authentication: turn a `__client` cookie into short-lived JWTs.
//!
//! The cookie is sent only to Clerk. The Suno API ever sees only the minted JWT.

use base64::Engine;
use serde_json::Value;

use crate::consts::{CLERK_BASE_URL, CLERK_JS_VERSION, CLERK_TOKEN_JS_VERSION, JWT_REFRESH_BUFFER};
use crate::error::{Error, Result};
use crate::http::{Http, HttpRequest, Method};

/// Normalise any accepted token form into a `__client=...` cookie string.
///
/// Accepts a raw JWT (`eyJ...`), a `__client=eyJ...` assignment, or a full
/// cookie header that contains `__client` somewhere within it.
pub(crate) fn normalise_token(token: &str) -> String {
    let token = token.trim();
    if token.starts_with("eyJ") {
        return format!("__client={token}");
    }
    if token.contains("__client=") {
        for part in token.split(';') {
            if let Some(value) = part.trim().strip_prefix("__client=") {
                return format!("__client={value}");
            }
        }
    }
    format!("__client={token}")
}

/// Extract the `exp` claim from a JWT without verifying its signature.
///
/// Returns `0` when the token is malformed, which callers treat as "expired".
pub(crate) fn decode_jwt_exp(token: &str) -> i64 {
    let Some(payload) = token.split('.').nth(1) else {
        return 0;
    };
    let payload = payload.trim_end_matches('=');
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return 0;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return 0;
    };
    value.get("exp").and_then(Value::as_i64).unwrap_or(0)
}

/// Warn when the pasted `__client` cookie is within this many days of expiry.
pub const TOKEN_EXPIRY_WARN_DAYS: i64 = 14;

/// The lifecycle state of the pasted `__client` cookie relative to now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenExpiry {
    /// The cookie could not be decoded, so its deadline is unknown.
    Unknown,
    /// The cookie is valid and comfortably beyond the warning window.
    Fresh,
    /// The cookie expires within the warning window, in `days` (rounded up).
    Expiring { days: i64 },
    /// The cookie has already expired.
    Expired,
}

/// Classify a cookie's `exp` against `now_unix` and a warning `window_secs`.
///
/// `days` is rounded up so any time left short of a full day still reports at
/// least `1`, never `0`.
pub fn classify_token_expiry(exp: i64, now_unix: i64, window_secs: i64) -> TokenExpiry {
    if exp <= now_unix {
        return TokenExpiry::Expired;
    }
    let remaining = exp - now_unix;
    if remaining < window_secs {
        const DAY_SECS: i64 = 86_400;
        return TokenExpiry::Expiring {
            days: (remaining + DAY_SECS - 1) / DAY_SECS,
        };
    }
    TokenExpiry::Fresh
}

struct ClientInfo {
    session_id: String,
    user_id: Option<String>,
    display_name: Option<String>,
}

fn parse_client_response(data: &Value) -> Result<ClientInfo> {
    let response = data
        .get("response")
        .filter(|value| !value.is_null())
        .ok_or_else(|| Error::Auth("invalid Clerk response; the cookie may be expired".into()))?;

    let session_id = response
        .get("last_active_session_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Auth("no active session; the cookie may be expired".into()))?
        .to_string();

    let mut user_id = None;
    let mut display_name = None;
    if let Some(sessions) = response.get("sessions").and_then(Value::as_array) {
        for session in sessions {
            if session.get("id").and_then(Value::as_str) == Some(session_id.as_str()) {
                let user = session.get("user").cloned().unwrap_or(Value::Null);
                user_id = user.get("id").and_then(Value::as_str).map(str::to_string);
                display_name = derive_display_name(&user);
                break;
            }
        }
    }
    Ok(ClientInfo {
        session_id,
        user_id,
        display_name,
    })
}

/// Pick a human display name from a Clerk user, preferring a real handle over
/// an email-derived one, mirroring how the Suno web client labels accounts.
fn derive_display_name(user: &Value) -> Option<String> {
    let field = |key: &str| {
        user.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let first = field("first_name");
    let last = field("last_name");
    let username = field("username");

    if !username.is_empty() && !username.contains('@') {
        Some(username)
    } else if !first.is_empty() && !first.contains('@') {
        Some(if last.is_empty() {
            first
        } else {
            format!("{first} {last}")
        })
    } else if !username.is_empty() && username.contains('@') {
        let local: String = username
            .split('@')
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(100)
            .collect();
        (!local.is_empty()).then_some(local)
    } else {
        None
    }
}

fn parse_token_response(data: &Value) -> Result<String> {
    data.get("jwt")
        .and_then(Value::as_str)
        .filter(|jwt| !jwt.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Auth("no JWT in the Clerk token response".into()))
}

/// Manages the Clerk cookie and the JWT lifecycle for one account.
pub struct ClerkAuth {
    cookie: String,
    jwt: Option<String>,
    jwt_exp: i64,
    session_id: Option<String>,
    user_id: Option<String>,
    display_name: Option<String>,
}

impl ClerkAuth {
    /// Create an authenticator from any accepted token form.
    pub fn new(token: &str) -> Self {
        Self {
            cookie: normalise_token(token),
            jwt: None,
            jwt_exp: 0,
            session_id: None,
            user_id: None,
            display_name: None,
        }
    }

    /// The Suno user ID, available after [`authenticate`](Self::authenticate).
    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    /// The account display name, or `"Suno"` when none is known.
    pub fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or("Suno")
    }

    /// Decode the `exp` claim of the stored `__client` cookie, if it decodes.
    pub fn cookie_exp(&self) -> Option<i64> {
        let normalised = normalise_token(&self.cookie);
        let token = normalised.strip_prefix("__client=")?;
        match decode_jwt_exp(token) {
            0 => None,
            exp => Some(exp),
        }
    }

    /// Classify how close the stored cookie is to its own expiry.
    pub fn token_expiry(&self, now_unix: i64, window_secs: i64) -> TokenExpiry {
        self.cookie_exp()
            .map(|exp| classify_token_expiry(exp, now_unix, window_secs))
            .unwrap_or(TokenExpiry::Unknown)
    }

    /// Fetch the Clerk session and a first JWT, returning the user ID.
    pub async fn authenticate(&mut self, http: &impl Http) -> Result<String> {
        self.fetch_session(http).await?;
        self.refresh_jwt(http).await?;
        self.user_id.clone().ok_or_else(|| {
            Error::Auth("could not determine the user ID from the Clerk session".into())
        })
    }

    /// Return a valid JWT, refreshing it when missing or near expiry.
    pub async fn ensure_jwt(&mut self, http: &impl Http) -> Result<String> {
        if self.jwt.is_none() || now_unix() >= self.jwt_exp - JWT_REFRESH_BUFFER {
            self.refresh_jwt(http).await?;
        }
        self.jwt
            .clone()
            .ok_or_else(|| Error::Auth("failed to obtain a JWT".into()))
    }

    /// Drop the cached JWT so the next [`ensure_jwt`](Self::ensure_jwt) refreshes.
    pub fn invalidate_jwt(&mut self) {
        self.jwt = None;
    }

    async fn fetch_session(&mut self, http: &impl Http) -> Result<()> {
        let cookie = self.cookie.clone();
        let url = format!("{CLERK_BASE_URL}/v1/client?_clerk_js_version={CLERK_JS_VERSION}");
        let data = clerk_request_json(http, &cookie, Method::Get, url).await?;
        let info = parse_client_response(&data)?;
        self.session_id = Some(info.session_id);
        self.user_id = info.user_id;
        self.display_name = info.display_name;
        Ok(())
    }

    async fn refresh_jwt(&mut self, http: &impl Http) -> Result<()> {
        if self.session_id.is_none() {
            self.fetch_session(http).await?;
        }
        let session_id = self
            .session_id
            .clone()
            .ok_or_else(|| Error::Auth("no Clerk session".into()))?;
        let cookie = self.cookie.clone();
        let url = format!(
            "{CLERK_BASE_URL}/v1/client/sessions/{session_id}/tokens?_clerk_js_version={CLERK_TOKEN_JS_VERSION}"
        );
        let data = clerk_request_json(http, &cookie, Method::Post, url).await?;
        let jwt = parse_token_response(&data)?;
        self.jwt_exp = decode_jwt_exp(&jwt);
        self.jwt = Some(jwt);
        Ok(())
    }
}

async fn clerk_request_json(
    http: &impl Http,
    cookie: &str,
    method: Method,
    url: String,
) -> Result<Value> {
    let request = HttpRequest {
        method,
        url,
        headers: vec![("Cookie".to_string(), cookie.to_string())],
    };
    let response = http
        .send(request)
        .await
        .map_err(|err| Error::Connection(format!("could not connect to Clerk: {err}")))?;
    if response.status != 200 {
        return Err(Error::Auth(format!(
            "Clerk request failed with status {}",
            response.status
        )));
    }
    serde_json::from_slice(&response.body)
        .map_err(|err| Error::Connection(format!("invalid Clerk response: {err}")))
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockHttp, Rule};

    fn jwt_with_exp(exp: i64) -> String {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{{\"exp\":{exp}}}"));
        format!("eyJhbGciOiJIUzI1NiJ9.{payload}.signature")
    }

    #[test]
    fn normalise_accepts_raw_jwt() {
        assert_eq!(normalise_token("  eyJabc  "), "__client=eyJabc");
    }

    #[test]
    fn normalise_extracts_from_cookie_header() {
        assert_eq!(
            normalise_token("foo=1; __client=eyJabc; bar=2"),
            "__client=eyJabc"
        );
    }

    #[test]
    fn normalise_wraps_unknown_value() {
        assert_eq!(normalise_token("rawvalue"), "__client=rawvalue");
    }

    #[test]
    fn decode_exp_reads_claim() {
        assert_eq!(decode_jwt_exp(&jwt_with_exp(1_893_456_000)), 1_893_456_000);
    }

    #[test]
    fn decode_exp_handles_garbage() {
        assert_eq!(decode_jwt_exp("not-a-jwt"), 0);
        assert_eq!(decode_jwt_exp(""), 0);
    }

    #[test]
    fn classify_marks_fresh_beyond_window() {
        let window = TOKEN_EXPIRY_WARN_DAYS * 86_400;
        let exp = 1_000_000 + window + 1;
        assert_eq!(
            classify_token_expiry(exp, 1_000_000, window),
            TokenExpiry::Fresh
        );
    }

    #[test]
    fn classify_boundary_is_fresh_just_inside_is_expiring() {
        let window = TOKEN_EXPIRY_WARN_DAYS * 86_400;
        let now = 1_000_000;
        assert_eq!(
            classify_token_expiry(now + window, now, window),
            TokenExpiry::Fresh
        );
        assert_eq!(
            classify_token_expiry(now + window - 1, now, window),
            TokenExpiry::Expiring {
                days: TOKEN_EXPIRY_WARN_DAYS
            }
        );
    }

    #[test]
    fn classify_ceils_partial_days() {
        let window = TOKEN_EXPIRY_WARN_DAYS * 86_400;
        let now = 1_000_000;
        assert_eq!(
            classify_token_expiry(now + 43_200, now, window),
            TokenExpiry::Expiring { days: 1 }
        );
    }

    #[test]
    fn classify_marks_expired_at_or_before_now() {
        let window = TOKEN_EXPIRY_WARN_DAYS * 86_400;
        assert_eq!(
            classify_token_expiry(1_000, 1_000, window),
            TokenExpiry::Expired
        );
        assert_eq!(
            classify_token_expiry(999, 1_000, window),
            TokenExpiry::Expired
        );
    }

    #[test]
    fn token_expiry_round_trips_through_cookie() {
        let window = TOKEN_EXPIRY_WARN_DAYS * 86_400;
        let now = 1_000_000;
        let exp = now + 5 * 86_400;
        let auth = ClerkAuth::new(&jwt_with_exp(exp));
        assert_eq!(auth.cookie_exp(), Some(exp));
        assert_eq!(
            auth.token_expiry(now, window),
            TokenExpiry::Expiring { days: 5 }
        );
    }

    #[test]
    fn token_expiry_is_unknown_for_undecodable_cookie() {
        let window = TOKEN_EXPIRY_WARN_DAYS * 86_400;
        let garbage = ClerkAuth::new("rawvalue");
        assert_eq!(garbage.cookie_exp(), None);
        assert_eq!(
            garbage.token_expiry(1_000_000, window),
            TokenExpiry::Unknown
        );
        // A JWT carrying exp = 0 decodes to nothing usable, so also Unknown.
        let zero = ClerkAuth::new(&jwt_with_exp(0));
        assert_eq!(zero.token_expiry(1_000_000, window), TokenExpiry::Unknown);
    }

    #[test]
    fn display_name_prefers_username() {
        let user = serde_json::json!({"username": "teh-hippo", "first_name": "Ignored"});
        assert_eq!(derive_display_name(&user).as_deref(), Some("teh-hippo"));
    }

    #[test]
    fn display_name_uses_first_last_when_no_username() {
        let user = serde_json::json!({"first_name": "Ada", "last_name": "Lovelace"});
        assert_eq!(derive_display_name(&user).as_deref(), Some("Ada Lovelace"));
    }

    #[test]
    fn display_name_falls_back_to_email_local_part() {
        let user = serde_json::json!({"username": "yshvq8dp9v@privaterelay.appleid.com"});
        assert_eq!(derive_display_name(&user).as_deref(), Some("yshvq8dp9v"));
    }

    #[test]
    fn parse_client_requires_a_session() {
        let data = serde_json::json!({"response": {"sessions": []}});
        assert!(parse_client_response(&data).is_err());
    }

    #[test]
    fn authenticate_fetches_user_and_jwt() {
        let client_body = serde_json::json!({
            "response": {
                "last_active_session_id": "sess_1",
                "sessions": [
                    {"id": "sess_1", "user": {"id": "user_1", "username": "teh-hippo"}}
                ]
            }
        })
        .to_string();
        let token_body = serde_json::json!({"jwt": jwt_with_exp(1_893_456_000)}).to_string();

        // The token URL also contains "/v1/client", so the specific rule wins by order.
        let http = MockHttp::new(vec![
            Rule::new("/v1/client/sessions/", 200, token_body),
            Rule::new("/v1/client", 200, client_body),
        ]);

        let mut auth = ClerkAuth::new("eyJtoken");
        let user_id = pollster::block_on(auth.authenticate(&http)).unwrap();
        assert_eq!(user_id, "user_1");
        assert_eq!(auth.display_name(), "teh-hippo");

        let jwt = pollster::block_on(auth.ensure_jwt(&http)).unwrap();
        assert!(jwt.starts_with("eyJ"));
    }
}
