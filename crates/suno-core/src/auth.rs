//! Clerk authentication: turn a `__client` cookie into short-lived JWTs.
//!
//! The cookie is sent only to Clerk. The Suno API ever sees only the minted JWT.

use std::sync::Mutex;

use base64::Engine;
use futures_util::lock::Mutex as AsyncMutex;
use serde_json::Value;

use crate::consts::{CLERK_BASE_URL, CLERK_JS_VERSION, JWT_REFRESH_BUFFER};
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
    let remaining = exp.saturating_sub(now_unix);
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
    state: Mutex<AuthState>,
    refresh_flight: AsyncMutex<()>,
}

#[derive(Default)]
struct AuthState {
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
            state: Mutex::new(AuthState::default()),
            refresh_flight: AsyncMutex::new(()),
        }
    }

    /// The Suno user ID, available after [`authenticate`](Self::authenticate).
    pub fn user_id(&self) -> Option<String> {
        self.state.lock().unwrap().user_id.clone()
    }

    /// The account display name, or `"Suno"` when none is known.
    pub fn display_name(&self) -> String {
        self.state
            .lock()
            .unwrap()
            .display_name
            .clone()
            .unwrap_or_else(|| "Suno".to_owned())
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
    pub async fn authenticate(&self, http: &impl Http) -> Result<String> {
        let _guard = self.refresh_flight.lock().await;
        self.fetch_session(http).await?;
        self.refresh_jwt(http).await?;
        self.state.lock().unwrap().user_id.clone().ok_or_else(|| {
            Error::Auth("could not determine the user ID from the Clerk session".into())
        })
    }

    /// Return a valid JWT, refreshing it when missing or near expiry.
    pub async fn ensure_jwt(&self, now_unix: i64, http: &impl Http) -> Result<String> {
        if !self.jwt_is_fresh(now_unix) {
            let _guard = self.refresh_flight.lock().await;
            if !self.jwt_is_fresh(now_unix) {
                self.refresh_jwt(http).await?;
            }
        }
        self.state
            .lock()
            .unwrap()
            .jwt
            .clone()
            .ok_or_else(|| Error::Auth("failed to obtain a JWT".into()))
    }

    fn jwt_is_fresh(&self, now_unix: i64) -> bool {
        let state = self.state.lock().unwrap();
        state.jwt.is_some() && now_unix < state.jwt_exp - JWT_REFRESH_BUFFER
    }

    /// Drop the cached JWT so the next [`ensure_jwt`](Self::ensure_jwt) refreshes.
    pub fn invalidate_jwt(&self) {
        self.state.lock().unwrap().jwt = None;
    }

    async fn fetch_session(&self, http: &impl Http) -> Result<()> {
        let cookie = self.cookie.clone();
        let url = format!("{CLERK_BASE_URL}/v1/client?_clerk_js_version={CLERK_JS_VERSION}");
        let data = clerk_request_json(http, &cookie, Method::Get, url).await?;
        let info = parse_client_response(&data)?;
        let mut state = self.state.lock().unwrap();
        state.session_id = Some(info.session_id);
        state.user_id = info.user_id;
        state.display_name = info.display_name;
        Ok(())
    }

    async fn refresh_jwt(&self, http: &impl Http) -> Result<()> {
        let mut session_id = self.state.lock().unwrap().session_id.clone();
        if session_id.is_none() {
            self.fetch_session(http).await?;
            session_id = self.state.lock().unwrap().session_id.clone();
        }
        let session_id = session_id.ok_or_else(|| Error::Auth("no Clerk session".into()))?;
        let cookie = self.cookie.clone();
        let url = format!(
            "{CLERK_BASE_URL}/v1/client/sessions/{session_id}/tokens?_clerk_js_version={CLERK_JS_VERSION}"
        );
        let data = clerk_request_json(http, &cookie, Method::Post, url).await?;
        let jwt = parse_token_response(&data)?;
        let mut state = self.state.lock().unwrap();
        state.jwt_exp = decode_jwt_exp(&jwt);
        state.jwt = Some(jwt);
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
        body: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockHttp, Reply, Rule, ScriptedHttp};

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
    fn classify_saturates_on_extreme_bounds_without_panicking() {
        // `exp - now_unix` would overflow i64 (a debug panic) for these bounds;
        // the saturating subtraction must keep this public fn total. Unreachable
        // via the real positive clock, but the fn is public API.
        assert_eq!(
            classify_token_expiry(i64::MAX, i64::MIN, i64::MAX),
            TokenExpiry::Fresh
        );
        assert_eq!(
            classify_token_expiry(i64::MAX, -1, i64::MAX),
            TokenExpiry::Fresh
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

        let auth = ClerkAuth::new("eyJtoken");
        let user_id = pollster::block_on(auth.authenticate(&http)).unwrap();
        assert_eq!(user_id, "user_1");
        assert_eq!(auth.display_name(), "teh-hippo");

        // Well before expiry — no refresh needed.
        let jwt = pollster::block_on(auth.ensure_jwt(0, &http)).unwrap();
        assert!(jwt.starts_with("eyJ"));
    }

    #[test]
    fn ensure_jwt_does_not_refresh_when_fresh() {
        let exp = 1_000_000i64;
        // No rules: any HTTP call would return an error.
        let http = MockHttp::new(vec![]);
        let auth = ClerkAuth::new("eyJtoken");
        *auth.state.lock().unwrap() = AuthState {
            jwt: Some(jwt_with_exp(exp)),
            jwt_exp: exp,
            session_id: Some("sess_1".into()),
            user_id: Some("user_1".into()),
            display_name: None,
        };
        let jwt = pollster::block_on(auth.ensure_jwt(exp - JWT_REFRESH_BUFFER - 1, &http)).unwrap();
        assert_eq!(decode_jwt_exp(&jwt), exp);
    }

    #[test]
    fn ensure_jwt_refreshes_at_expiry_boundary() {
        let exp = 1_000_000i64;
        let new_exp = exp + 3_600;
        let token_body = serde_json::json!({"jwt": jwt_with_exp(new_exp)}).to_string();
        let http = MockHttp::new(vec![Rule::new("/v1/client/sessions/", 200, token_body)]);
        let auth = ClerkAuth::new("eyJtoken");
        *auth.state.lock().unwrap() = AuthState {
            jwt: Some(jwt_with_exp(exp)),
            jwt_exp: exp,
            session_id: Some("sess_1".into()),
            user_id: Some("user_1".into()),
            display_name: None,
        };
        // At the refresh boundary: a new JWT with new_exp is issued.
        let jwt = pollster::block_on(auth.ensure_jwt(exp - JWT_REFRESH_BUFFER, &http)).unwrap();
        assert_eq!(decode_jwt_exp(&jwt), new_exp);
    }

    #[test]
    fn ensure_jwt_refresh_is_single_flight_under_concurrency() {
        let exp = 1_000_000i64;
        let new_exp = exp + 3_600;
        let token_body = serde_json::json!({"jwt": jwt_with_exp(new_exp)}).to_string();
        let auth = ClerkAuth::new("eyJtoken");
        *auth.state.lock().unwrap() = AuthState {
            jwt: Some(jwt_with_exp(exp)),
            jwt_exp: exp,
            session_id: Some("sess_1".into()),
            user_id: Some("user_1".into()),
            display_name: None,
        };
        let http = ScriptedHttp::new().route("/v1/client/sessions/", Reply::json(&token_body));
        let now = exp - JWT_REFRESH_BUFFER;
        let (first, second) = pollster::block_on(async {
            futures_util::future::join(auth.ensure_jwt(now, &http), auth.ensure_jwt(now, &http))
                .await
        });
        assert!(first.is_ok());
        assert!(second.is_ok());
        assert_eq!(http.count("/v1/client/sessions/"), 1);
    }
}
