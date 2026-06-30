//! The Suno API client: lists the library behind the [`Http`](crate::Http) port.

use serde_json::Value;

use crate::auth::ClerkAuth;
use crate::consts::{MAX_PAGES, SUNO_API_BASE_URL};
use crate::error::{Error, Result};
use crate::http::{Http, HttpRequest, Method};
use crate::model::Clip;

const EXCLUDED_TASKS: [&str; 2] = ["infill", "fixed_infill"];
const EXCLUDED_TYPES: [&str; 1] = ["rendered_context_window"];

/// A client for the Suno library API, owning the account's [`ClerkAuth`].
pub struct SunoClient {
    auth: ClerkAuth,
}

impl SunoClient {
    /// Create a client from a fresh or already-authenticated [`ClerkAuth`].
    pub fn new(auth: ClerkAuth) -> Self {
        Self { auth }
    }

    /// Borrow the underlying authenticator.
    pub fn auth(&self) -> &ClerkAuth {
        &self.auth
    }

    /// List clips across the whole library, or only liked clips.
    ///
    /// Stops early once `limit` clips are collected. Paging is hard-capped at
    /// [`MAX_PAGES`] so a runaway `has_more` can never loop forever.
    pub async fn list_clips(
        &mut self,
        http: &impl Http,
        liked: bool,
        limit: Option<usize>,
    ) -> Result<Vec<Clip>> {
        let mut clips = Vec::new();
        let suffix = if liked { "&is_liked=true" } else { "" };
        for page in 0..MAX_PAGES {
            let path = format!("/api/feed/v2/?page={page}{suffix}");
            let body = self.api_get(http, &path).await?;
            let (page_clips, has_more) = parse_feed(&body)?;
            clips.extend(page_clips);
            if !has_more || limit.is_some_and(|n| clips.len() >= n) {
                break;
            }
        }
        if let Some(n) = limit {
            clips.truncate(n);
        }
        Ok(clips)
    }

    /// Fetch one clip by ID.
    ///
    /// Tries the dedicated `/api/clip/{id}` endpoint first, then falls back to
    /// scanning the library feed, since that endpoint's exact shape is not yet
    /// confirmed against the live API.
    pub async fn get_clip(&mut self, http: &impl Http, id: &str) -> Result<Clip> {
        if let Some(clip) = self.try_get_clip(http, id).await? {
            return Ok(clip);
        }
        self.find_in_feed(http, id).await
    }

    /// Ask Suno to render a clip to lossless WAV (server-side, asynchronous).
    pub async fn request_wav(&mut self, http: &impl Http, id: &str) -> Result<()> {
        let path = format!("/api/gen/{id}/convert_wav/");
        self.api_request(http, Method::Post, &path).await?;
        Ok(())
    }

    /// Read the rendered WAV URL for a clip, or `None` while it is not ready.
    pub async fn wav_url(&mut self, http: &impl Http, id: &str) -> Result<Option<String>> {
        let path = format!("/api/gen/{id}/wav_file/");
        let body = self.api_get(http, &path).await?;
        let data: Value = serde_json::from_slice(&body)
            .map_err(|err| Error::Api(format!("invalid wav_file JSON: {err}")))?;
        Ok(data
            .get("wav_file_url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_string))
    }

    /// Try the dedicated clip endpoint, returning `None` when it is missing or
    /// returns a body that does not yield the requested clip.
    async fn try_get_clip(&mut self, http: &impl Http, id: &str) -> Result<Option<Clip>> {
        let path = format!("/api/clip/{id}");
        match self.api_get(http, &path).await {
            Ok(body) => Ok(parse_clip(&body).filter(|clip| clip.id == id)),
            Err(Error::Api(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Locate a clip by scanning the library feed.
    async fn find_in_feed(&mut self, http: &impl Http, id: &str) -> Result<Clip> {
        let clips = self.list_clips(http, false, None).await?;
        clips
            .into_iter()
            .find(|clip| clip.id == id)
            .ok_or_else(|| Error::Api(format!("clip {id} not found in the library")))
    }

    /// Perform an authenticated GET, refreshing the JWT once on a 401/403.
    async fn api_get(&mut self, http: &impl Http, path: &str) -> Result<Vec<u8>> {
        self.api_request(http, Method::Get, path).await
    }

    /// Perform an authenticated request, refreshing the JWT once on a 401/403.
    async fn api_request(
        &mut self,
        http: &impl Http,
        method: Method,
        path: &str,
    ) -> Result<Vec<u8>> {
        let url = format!("{SUNO_API_BASE_URL}{path}");
        for attempt in 0..2 {
            let jwt = self.auth.ensure_jwt(http).await?;
            let request = HttpRequest {
                method,
                url: url.clone(),
                headers: vec![("Authorization".to_string(), format!("Bearer {jwt}"))],
            };
            let response = http
                .send(request)
                .await
                .map_err(|err| Error::Connection(err.to_string()))?;
            match response.status {
                200..=299 => return Ok(response.body),
                401 | 403 if attempt == 0 => self.auth.invalidate_jwt(),
                401 | 403 => {
                    return Err(Error::Auth(format!(
                        "Suno API auth failed with status {}",
                        response.status
                    )));
                }
                429 => return Err(Error::RateLimited),
                status => {
                    let preview: String = String::from_utf8_lossy(&response.body)
                        .chars()
                        .take(200)
                        .collect();
                    return Err(Error::Api(format!("Suno API returned {status}: {preview}")));
                }
            }
        }
        Err(Error::Api("Suno API request failed after retries".into()))
    }
}

/// Parse a single-clip response body, accepting either a bare clip object or a
/// `{"clip": {...}}` wrapper. Returns `None` when no clip id is present.
fn parse_clip(body: &[u8]) -> Option<Clip> {
    let data: Value = serde_json::from_slice(body).ok()?;
    let raw = data
        .get("clip")
        .filter(|value| value.is_object())
        .unwrap_or(&data);
    let has_id = raw
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty());
    has_id.then(|| Clip::from_json(raw))
}

/// Parse a feed page body into the kept clips and the `has_more` flag.
fn parse_feed(body: &[u8]) -> Result<(Vec<Clip>, bool)> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid feed JSON: {err}")))?;
    let Some(object) = data.as_object() else {
        return Ok((Vec::new(), false));
    };
    let clips = object
        .get("clips")
        .and_then(Value::as_array)
        .map(|raw| {
            raw.iter()
                .filter(|clip| keep_clip(clip))
                .map(Clip::from_json)
                .collect()
        })
        .unwrap_or_default();
    let has_more = object
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((clips, has_more))
}

/// Keep only finished clips that are not infills or context-window artefacts.
fn keep_clip(raw: &Value) -> bool {
    if raw.get("status").and_then(Value::as_str) != Some("complete") {
        return false;
    }
    let metadata = raw.get("metadata");
    let clip_type = metadata.and_then(|m| m.get("type")).and_then(Value::as_str);
    if clip_type.is_some_and(|t| EXCLUDED_TYPES.contains(&t)) {
        return false;
    }
    let task = metadata.and_then(|m| m.get("task")).and_then(Value::as_str);
    !task.is_some_and(|t| EXCLUDED_TASKS.contains(&t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockHttp, Rule};

    fn feed_body() -> String {
        serde_json::json!({
            "has_more": false,
            "clips": [
                {
                    "id": "a", "title": "Song A", "status": "complete",
                    "audio_url": "https://cdn1.suno.ai/a.mp3",
                    "metadata": {"tags": "rock", "duration": 120.5, "type": "gen"}
                },
                {"id": "b", "title": "Infill", "status": "complete", "metadata": {"task": "infill"}},
                {"id": "c", "title": "Streaming", "status": "streaming", "metadata": {}},
                {
                    "id": "d", "title": "Context", "status": "complete",
                    "metadata": {"type": "rendered_context_window"}
                }
            ]
        })
        .to_string()
    }

    #[test]
    fn parse_feed_filters_and_maps() {
        let (clips, has_more) = parse_feed(feed_body().as_bytes()).unwrap();
        assert!(!has_more);
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].id, "a");
        assert_eq!(clips[0].tags, "rock");
        assert!((clips[0].duration - 120.5).abs() < f64::EPSILON);
    }

    #[test]
    fn audiopipe_url_is_rewritten_to_cdn() {
        let raw =
            serde_json::json!({"id": "x", "audio_url": "https://audiopipe.suno.ai/?item_id=x"});
        assert_eq!(
            Clip::from_json(&raw).audio_url,
            "https://cdn1.suno.ai/x.mp3"
        );
    }

    #[test]
    fn list_clips_authenticates_then_reads_the_feed() {
        let client_body = serde_json::json!({
            "response": {
                "last_active_session_id": "s",
                "sessions": [{"id": "s", "user": {"id": "u", "username": "h"}}]
            }
        })
        .to_string();
        let http = MockHttp::new(vec![
            Rule::new(
                "/v1/client/sessions/",
                200,
                r#"{"jwt": "a.b.c"}"#.to_string(),
            ),
            Rule::new("/v1/client", 200, client_body),
            Rule::new("/api/feed/v2", 200, feed_body()),
        ]);

        let mut auth = ClerkAuth::new("eyJtoken");
        pollster::block_on(auth.authenticate(&http)).unwrap();
        let mut client = SunoClient::new(auth);
        let clips = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].id, "a");
    }

    fn auth_rules() -> Vec<Rule> {
        let client_body = serde_json::json!({
            "response": {
                "last_active_session_id": "s",
                "sessions": [{"id": "s", "user": {"id": "u", "username": "h"}}]
            }
        })
        .to_string();
        vec![
            Rule::new(
                "/v1/client/sessions/",
                200,
                r#"{"jwt": "a.b.c"}"#.to_string(),
            ),
            Rule::new("/v1/client", 200, client_body),
        ]
    }

    fn authed_client(http: &MockHttp) -> SunoClient {
        let mut auth = ClerkAuth::new("eyJtoken");
        pollster::block_on(auth.authenticate(http)).unwrap();
        SunoClient::new(auth)
    }

    #[test]
    fn parse_clip_accepts_bare_and_wrapped_shapes() {
        let bare = serde_json::json!({"id": "z", "title": "Zed"}).to_string();
        assert_eq!(parse_clip(bare.as_bytes()).unwrap().id, "z");

        let wrapped = serde_json::json!({"clip": {"id": "w", "title": "Wai"}}).to_string();
        assert_eq!(parse_clip(wrapped.as_bytes()).unwrap().id, "w");

        let missing = serde_json::json!({"detail": "not found"}).to_string();
        assert!(parse_clip(missing.as_bytes()).is_none());
    }

    #[test]
    fn get_clip_uses_the_dedicated_endpoint() {
        let clip_body = serde_json::json!({
            "id": "z", "title": "Zed", "status": "complete",
            "audio_url": "https://cdn1.suno.ai/z.mp3",
            "metadata": {"tags": "jazz", "duration": 99.0, "type": "gen"}
        })
        .to_string();
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/clip/", 200, clip_body));
        let http = MockHttp::new(rules);
        let mut client = authed_client(&http);

        let clip = pollster::block_on(client.get_clip(&http, "z")).unwrap();
        assert_eq!(clip.id, "z");
        assert_eq!(clip.title, "Zed");
        assert_eq!(clip.tags, "jazz");
    }

    #[test]
    fn get_clip_falls_back_to_the_feed_when_endpoint_missing() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            "/api/clip/",
            404,
            r#"{"detail": "not found"}"#.to_string(),
        ));
        rules.push(Rule::new("/api/feed/v2", 200, feed_body()));
        let http = MockHttp::new(rules);
        let mut client = authed_client(&http);

        let clip = pollster::block_on(client.get_clip(&http, "a")).unwrap();
        assert_eq!(clip.id, "a");
        assert_eq!(clip.tags, "rock");
    }

    #[test]
    fn request_wav_accepts_a_2xx_status() {
        let mut rules = auth_rules();
        rules.push(Rule::new("/convert_wav/", 201, "{}".to_string()));
        let http = MockHttp::new(rules);
        let mut client = authed_client(&http);

        assert!(pollster::block_on(client.request_wav(&http, "z")).is_ok());
    }

    #[test]
    fn wav_url_reads_the_ready_url() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            "/wav_file/",
            200,
            r#"{"wav_file_url": "https://cdn1.suno.ai/z.wav"}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let mut client = authed_client(&http);

        let url = pollster::block_on(client.wav_url(&http, "z")).unwrap();
        assert_eq!(url.as_deref(), Some("https://cdn1.suno.ai/z.wav"));
    }

    #[test]
    fn wav_url_is_none_until_the_render_is_ready() {
        let mut rules = auth_rules();
        rules.push(Rule::new("/wav_file/", 200, "{}".to_string()));
        let http = MockHttp::new(rules);
        let mut client = authed_client(&http);

        let url = pollster::block_on(client.wav_url(&http, "z")).unwrap();
        assert_eq!(url, None);
    }
}
