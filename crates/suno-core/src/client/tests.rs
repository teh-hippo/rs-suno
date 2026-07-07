//! The Suno API client test suite: end-to-end scenarios that script the
//! [`Http`](crate::Http) port with in-memory doubles and drive `SunoClient`
//! methods, asserting feed paging, retry/pacing, and each endpoint's decode.

use super::*;
use crate::testutil::{MockHttp, RecordingClock, Reply, Rule, ScriptedHttp};
use serde_json::Value;
use std::time::Duration;

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
        Rule::new("/api/feed/v3", 200, feed_body()),
    ]);

    let auth = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth.authenticate(&http)).unwrap();
    let client = SunoClient::new(auth, RecordingClock::new());
    let (clips, complete, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].id, "a");
    assert!(complete);
}

#[test]
fn api_request_uses_clock_now_unix_for_jwt_expiry() {
    use crate::consts::JWT_REFRESH_BUFFER;
    use base64::Engine;
    let exp = 1_000_000i64;
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
    let jwt_str = format!("hdr.{}.sig", payload);
    let token_body = format!(r#"{{"jwt": "{jwt_str}"}}"#);
    let client_body = serde_json::json!({
        "response": {
            "last_active_session_id": "s",
            "sessions": [{"id": "s", "user": {"id": "u", "username": "h"}}]
        }
    })
    .to_string();

    let make_http = || {
        ScriptedHttp::new()
            .route("/v1/client/sessions/", Reply::json(&token_body))
            .route("/v1/client", Reply::json(&client_body))
            .route("/api/feed/v3", Reply::json(&feed_body()))
    };

    // At the refresh boundary: ensure_jwt triggers a second refresh_jwt call.
    let http = make_http();
    let auth = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth.authenticate(&http)).unwrap();
    let client = SunoClient::new(auth, RecordingClock::at(exp - JWT_REFRESH_BUFFER));
    let (clips, _, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert_eq!(clips.len(), 1);
    // authenticate + api_request refresh = 2 token calls.
    assert_eq!(http.count("/v1/client/sessions/"), 2);

    // Just before the boundary: no additional refresh.
    let http2 = make_http();
    let auth2 = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth2.authenticate(&http2)).unwrap();
    let client2 = SunoClient::new(auth2, RecordingClock::at(exp - JWT_REFRESH_BUFFER - 1));
    let (clips2, _, _) = pollster::block_on(client2.list_clips(&http2, false, None)).unwrap();
    assert_eq!(clips2.len(), 1);
    // Only authenticate's token call; no extra refresh.
    assert_eq!(http2.count("/v1/client/sessions/"), 1);
}

#[test]
fn list_clips_reports_incomplete_when_paging_is_capped() {
    let mut rules = auth_rules();
    rules.push(Rule::new(
        "/api/feed/v3",
        200,
        serde_json::json!({
            "has_more": true,
            "next_cursor": "cur1",
            "clips": [{
                "id": "a", "title": "Song A", "status": "complete",
                "audio_url": "https://cdn1.suno.ai/a.mp3",
                "metadata": {"type": "gen"}
            }]
        })
        .to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let (_clips, complete, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert!(!complete);
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

fn authed_client(http: &MockHttp) -> SunoClient<RecordingClock> {
    let auth = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth.authenticate(http)).unwrap();
    SunoClient::new(auth, RecordingClock::new())
}

#[test]
fn get_billing_info_reads_remaining_credits() {
    let mut rules = auth_rules();
    rules.push(Rule::new(
        BILLING_INFO_PATH,
        200,
        r#"{"total_credits_left":500,"monthly_limit":1000,"monthly_usage":500}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let billing = pollster::block_on(client.get_billing_info(&http)).unwrap();
    assert_eq!(billing.total_credits_left, Some(500));
    assert_eq!(billing.monthly_limit, Some(1000));
    assert_eq!(billing.monthly_usage, Some(500));
}

#[test]
fn get_billing_info_tolerates_missing_balance() {
    let mut rules = auth_rules();
    rules.push(Rule::new(
        BILLING_INFO_PATH,
        200,
        r#"{"monthly_usage":12}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let billing = pollster::block_on(client.get_billing_info(&http)).unwrap();
    assert_eq!(billing.total_credits_left, None);
    assert_eq!(billing.monthly_usage, Some(12));
}

#[test]
fn aligned_lyrics_reads_words_and_lines() {
    let mut rules = auth_rules();
    let body = serde_json::json!({
        "aligned_words": [
            {"word": "hi", "success": true, "start_s": 0.5, "end_s": 0.9, "p_align": 0.99}
        ],
        "aligned_lyrics": [
            {"text": "hi", "start_s": 0.5, "end_s": 0.9, "section": "Verse 1",
             "words": [{"text": "hi", "start_s": 0.5, "end_s": 0.9}]}
        ],
        "hoot_cer": 0.2, "is_streamed": false
    })
    .to_string();
    rules.push(Rule::new("/aligned_lyrics/v2/", 200, body));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let aligned = pollster::block_on(client.aligned_lyrics(&http, "clip-1")).unwrap();
    assert_eq!(aligned.words.len(), 1);
    assert_eq!(aligned.lines.len(), 1);
    assert_eq!(aligned.lines[0].section, "Verse 1");
    assert!(!aligned.is_empty());
}

#[test]
fn aligned_lyrics_empty_arrays_map_to_empty() {
    let mut rules = auth_rules();
    rules.push(Rule::new(
        "/aligned_lyrics/v2/",
        200,
        r#"{"aligned_words":[],"aligned_lyrics":[],"hoot_cer":1.0}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let aligned = pollster::block_on(client.aligned_lyrics(&http, "instr")).unwrap();
    assert!(aligned.is_empty());
}

#[test]
fn aligned_lyrics_maps_404_to_empty() {
    let mut rules = auth_rules();
    rules.push(Rule::new(
        "/aligned_lyrics/v2/",
        404,
        "not found".to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let aligned = pollster::block_on(client.aligned_lyrics(&http, "missing")).unwrap();
    assert!(aligned.is_empty());
}

fn scripted_client(http: &ScriptedHttp, clock: RecordingClock) -> SunoClient<RecordingClock> {
    let auth = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth.authenticate(http)).unwrap();
    SunoClient::new(auth, clock)
}

fn one_clip_page(id: &str, next_cursor: Option<&str>) -> String {
    let mut page = serde_json::json!({
        "has_more": next_cursor.is_some(),
        "clips": [{
            "id": id, "title": "Song", "status": "complete",
            "audio_url": format!("https://cdn1.suno.ai/{id}.mp3"),
            "metadata": {"type": "gen"}
        }]
    });
    if let Some(cursor) = next_cursor {
        page["next_cursor"] = serde_json::json!(cursor);
    }
    page.to_string()
}

#[test]
fn list_clips_retries_a_rate_limited_page() {
    let http = ScriptedHttp::new().with_auth().route_seq(
        "/api/feed/v3",
        vec![Reply::status(429), Reply::json(&feed_body())],
    );
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let (clips, complete, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert_eq!(clips.len(), 1);
    assert!(complete);
    // The throttled page was retried once, waiting the default post-429 wait.
    assert_eq!(http.count("/api/feed/v3"), 2);
    assert_eq!(clock.sleeps(), vec![Duration::from_secs(5)]);
}

#[test]
fn list_clips_honours_retry_after_on_a_throttled_page() {
    let http = ScriptedHttp::new().with_auth().route_seq(
        "/api/feed/v3",
        vec![
            Reply::status(429).with_retry_after(7),
            Reply::json(&feed_body()),
        ],
    );
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let (clips, _complete, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert_eq!(clips.len(), 1);
    // The server's Retry-After is honoured directly as the post-429 wait.
    assert_eq!(clock.sleeps(), vec![Duration::from_secs(7)]);
}

#[test]
fn list_clips_re_posts_the_same_cursor_after_a_throttled_page() {
    // A 429 mid-walk must re-POST the *same* cursor, not skip a page.
    let http = ScriptedHttp::new().with_auth().route_seq(
        "/api/feed/v3",
        vec![
            Reply::json(&one_clip_page("a", Some("cur1"))),
            Reply::status(429),
            Reply::json(&one_clip_page("b", None)),
        ],
    );
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let (clips, complete, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert!(complete);
    assert_eq!(clips.len(), 2);
    let bodies = http.bodies();
    let feed_bodies: Vec<&String> = bodies.iter().filter(|b| b.contains("filters")).collect();
    assert_eq!(feed_bodies.len(), 3, "page 1, the 429 retry, then page 2");
    // The retry (body 2) carries the SAME cursor as the throttled call (body 2 == the
    // second feed POST), i.e. the cursor from page 1's next_cursor.
    let retried: Value = serde_json::from_str(feed_bodies[1]).unwrap();
    let after_retry: Value = serde_json::from_str(feed_bodies[2]).unwrap();
    assert_eq!(retried["cursor"], "cur1");
    assert_eq!(after_retry["cursor"], "cur1");
}

#[test]
fn list_clips_threads_the_cursor_across_pages() {
    let http = ScriptedHttp::new().with_auth().route_seq(
        "/api/feed/v3",
        vec![
            Reply::json(&one_clip_page("a", Some("cur1"))),
            Reply::json(&one_clip_page("b", None)),
        ],
    );
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let (clips, complete, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert!(complete);
    assert_eq!(clips.len(), 2);
    let bodies = http.bodies();
    let feed_bodies: Vec<&String> = bodies.iter().filter(|b| b.contains("filters")).collect();
    assert_eq!(feed_bodies.len(), 2);
    let page1: Value = serde_json::from_str(feed_bodies[0]).unwrap();
    let page2: Value = serde_json::from_str(feed_bodies[1]).unwrap();
    // Page 1 omits the cursor; page 2 carries exactly page 1's next_cursor.
    assert!(page1.get("cursor").is_none());
    assert_eq!(page2["cursor"], "cur1");
}

#[test]
fn list_clips_stops_incomplete_when_has_more_but_no_cursor() {
    // has_more == true with no usable next_cursor: a truncated feed. The walk
    // must stop, report incomplete, and never re-POST a null cursor.
    let page = serde_json::json!({
        "has_more": true,
        "clips": [{
            "id": "a", "title": "Song", "status": "complete",
            "audio_url": "https://cdn1.suno.ai/a.mp3", "metadata": {"type": "gen"}
        }]
    })
    .to_string();
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/feed/v3", Reply::json(&page));
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let (clips, complete, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert!(!complete);
    assert_eq!(clips.len(), 1);
    assert_eq!(http.count("/api/feed/v3"), 1, "no re-POST of a null cursor");
}

#[test]
fn list_clips_is_incomplete_when_has_more_is_missing() {
    // A page with no has_more key must not be read as a fully drained feed.
    let page = serde_json::json!({
        "clips": [{
            "id": "a", "title": "Song", "status": "complete",
            "audio_url": "https://cdn1.suno.ai/a.mp3", "metadata": {"type": "gen"}
        }]
    })
    .to_string();
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/feed/v3", Reply::json(&page));
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let (clips, complete, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert!(!complete);
    assert_eq!(clips.len(), 1);
    assert_eq!(http.count("/api/feed/v3"), 1);
}

#[test]
fn list_clips_propagates_an_error_mid_walk_and_never_completes() {
    let http = ScriptedHttp::new().with_auth().route_seq(
        "/api/feed/v3",
        vec![
            Reply::json(&one_clip_page("a", Some("cur1"))),
            Reply::status(500),
        ],
    );
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let result = pollster::block_on(client.list_clips(&http, false, None));
    assert!(matches!(result, Err(Error::Api(_))));
}

#[test]
fn list_clips_is_complete_on_an_empty_drained_feed() {
    // An empty but fully drained feed is authoritative (complete = true);
    // deletion is separately gated by there being a mirror source.
    let page = serde_json::json!({"has_more": false, "clips": []}).to_string();
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/feed/v3", Reply::json(&page));
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let (clips, complete, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert!(complete);
    assert!(clips.is_empty());
}

#[test]
fn list_clips_flags_filter_loss_on_a_drained_feed() {
    // A fully drained feed that still hides a clip behind is_downloadable
    // must report any_filtered=true, so the Library/Liked area is not
    // authoritative and an irreplaceable master is never deleted as
    // "absent" (#248).
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/feed/v3", Reply::json(&feed_body()));
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let (clips, complete, any_filtered) =
        pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert!(complete);
    assert!(any_filtered);
    assert_eq!(clips.len(), 1);
}

#[test]
fn list_clips_ors_filter_loss_across_pages() {
    // The first page loses nothing; the second hides a streaming clip. The
    // flag must accumulate so a late-page filter loss still disarms deletion.
    let page2 = serde_json::json!({
        "has_more": false,
        "clips": [
            {"id": "e", "status": "complete", "metadata": {"type": "gen"}},
            {"id": "f", "status": "streaming", "metadata": {}}
        ]
    })
    .to_string();
    let http = ScriptedHttp::new().with_auth().route_seq(
        "/api/feed/v3",
        vec![
            Reply::json(&one_clip_page("a", Some("cur1"))),
            Reply::json(&page2),
        ],
    );
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let (clips, complete, any_filtered) =
        pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert!(complete);
    assert!(any_filtered);
    // "a" and "e" survive; the streaming "f" is dropped.
    assert_eq!(clips.len(), 2);
}

#[test]
fn list_clips_liked_scope_sends_the_liked_filter() {
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/feed/v3", Reply::json(&feed_body()));
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let _ = pollster::block_on(client.list_clips(&http, true, None)).unwrap();
    let bodies = http.bodies();
    let feed_body = bodies.iter().find(|b| b.contains("filters")).unwrap();
    let value: Value = serde_json::from_str(feed_body).unwrap();
    assert_eq!(value["filters"]["liked"], "True");
    assert_eq!(value["filters"]["trashed"], "False");
}

#[test]
fn list_clips_does_not_pace_an_unthrottled_walk() {
    let http = ScriptedHttp::new().with_auth().route_seq(
        "/api/feed/v3",
        vec![
            Reply::json(&one_clip_page("a", Some("cur1"))),
            Reply::json(&one_clip_page("e", None)),
        ],
    );
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let (clips, complete, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert!(complete);
    assert_eq!(clips.len(), 2);
    assert_eq!(http.count("/api/feed/v3"), 2);
    // Pacing is reactive: with no 429 the whole walk waits nowhere.
    assert!(clock.sleeps().is_empty());
}

#[test]
fn list_clips_slows_its_pace_after_a_throttled_page() {
    let http = ScriptedHttp::new().with_auth().route_seq(
        "/api/feed/v3",
        vec![
            Reply::status(429),
            Reply::json(&one_clip_page("a", Some("cur1"))),
            Reply::json(&one_clip_page("e", None)),
        ],
    );
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let (clips, complete, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert!(complete);
    assert_eq!(clips.len(), 2);
    // The 429 halved the rate, so the default post-429 wait is followed by a
    // doubled inter-page pace (500ms to 1s) for the next page.
    assert_eq!(
        clock.sleeps(),
        vec![Duration::from_secs(5), Duration::from_secs(1)]
    );
}

#[test]
fn list_clips_gives_up_after_max_retries() {
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/feed/v3", Reply::status(429));
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());

    let result = pollster::block_on(client.list_clips(&http, false, None));
    assert!(matches!(result, Err(Error::RateLimited { .. })));
    let budget = crate::consts::API_MAX_RETRIES as usize;
    assert_eq!(clock.sleeps().len(), budget);
    assert_eq!(http.count("/api/feed/v3"), budget + 1);
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
    let client = authed_client(&http);

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
    rules.push(Rule::new("/api/feed/v3", 200, feed_body()));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let clip = pollster::block_on(client.get_clip(&http, "a")).unwrap();
    assert_eq!(clip.id, "a");
    assert_eq!(clip.tags, "rock");
}

#[test]
fn request_wav_accepts_a_2xx_status() {
    let mut rules = auth_rules();
    rules.push(Rule::new("/convert_wav/", 201, "{}".to_string()));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

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
    let client = authed_client(&http);

    let url = pollster::block_on(client.wav_url(&http, "z")).unwrap();
    assert_eq!(url.as_deref(), Some("https://cdn1.suno.ai/z.wav"));
}

#[test]
fn wav_url_is_none_until_the_render_is_ready() {
    let mut rules = auth_rules();
    rules.push(Rule::new("/wav_file/", 200, "{}".to_string()));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let url = pollster::block_on(client.wav_url(&http, "z")).unwrap();
    assert_eq!(url, None);
}

#[test]
fn wav_url_404_maps_to_none() {
    // A 404 means the render is absent or was never requested, not a run
    // failure: map it to None, symmetric with aligned_lyrics, so the fetch
    // flow polls again rather than aborting the whole render.
    let mut rules = auth_rules();
    rules.push(Rule::new(
        "/wav_file/",
        404,
        r#"{"detail": "Not found."}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let url = pollster::block_on(client.wav_url(&http, "z")).unwrap();
    assert_eq!(url, None);
}

#[test]
fn get_clips_by_ids_keeps_infill_and_upload_ancestors() {
    // The gap-fill path must not apply the listing's downloadability filter:
    // an infill ancestor and an upload root both survive, returned by the
    // batch `get_songs_by_ids` call.
    let p1 = serde_json::json!({
        "id": "p1", "title": "Infill Ancestor", "status": "complete",
        "metadata": {"type": "gen", "task": "infill"}
    })
    .to_string();
    let p2 = serde_json::json!({
        "id": "p2", "title": "Uploaded Root", "status": "complete",
        "metadata": {"type": "upload"}
    })
    .to_string();
    let batch = format!(r#"{{"clips":[{p1},{p2}]}}"#);
    let mut rules = auth_rules();
    rules.push(Rule::new("get_songs_by_ids", 200, batch));
    rules.push(Rule::new("/api/clip/p1", 200, p1));
    rules.push(Rule::new("/api/clip/p2", 200, p2));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let clips = pollster::block_on(client.get_clips_by_ids(&http, &["p1", "p2"], 4)).unwrap();
    assert_eq!(
        clips.len(),
        2,
        "infill and upload ancestors must not be filtered"
    );
    assert_eq!(clips[0].id, "p1");
    assert_eq!(clips[1].id, "p2");
}

#[test]
fn get_clips_by_ids_returns_a_trashed_clip() {
    // A trashed ancestor must still be retrievable by id (the v2 `?ids=`
    // capability that `get_songs_by_ids` now restores in one request).
    let trashed = serde_json::json!({
        "id": "t1", "title": "Trashed Ancestor", "status": "complete",
        "is_trashed": true, "metadata": {"type": "gen"}
    })
    .to_string();
    let batch = format!(r#"{{"clips":[{trashed}]}}"#);
    let mut rules = auth_rules();
    rules.push(Rule::new("get_songs_by_ids", 200, batch));
    rules.push(Rule::new("/api/clip/t1", 200, trashed));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let clips = pollster::block_on(client.get_clips_by_ids(&http, &["t1"], 4)).unwrap();
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].id, "t1");
    assert!(clips[0].is_trashed);
}

#[test]
fn get_clips_by_ids_skips_a_not_found_id_and_dedupes() {
    let only = serde_json::json!({
        "id": "only", "title": "Bare", "status": "complete", "metadata": {"type": "gen"}
    })
    .to_string();
    // The batch returns "only" and omits "gone"; "gone" then falls back to a
    // per-id fetch that 404s and is skipped.
    let batch = format!(r#"{{"clips":[{only}]}}"#);
    let http = ScriptedHttp::new()
        .with_auth()
        .route("get_songs_by_ids", Reply::json(&batch))
        .route("/api/clip/gone", Reply::status(404));
    let client = scripted_client(&http, RecordingClock::new());

    let clips =
        pollster::block_on(client.get_clips_by_ids(&http, &["only", "gone", "only"], 4)).unwrap();
    assert_eq!(clips.len(), 1, "the 404 id is skipped");
    assert_eq!(clips[0].id, "only");
    // "only" is deduped and returned by the batch, so it is never per-id
    // fetched; "gone" is attempted once via the per-id fallback.
    assert_eq!(
        http.count("get_songs_by_ids"),
        1,
        "one batch call for both ids"
    );
    assert_eq!(http.count("/api/clip/only"), 0);
    assert_eq!(http.count("/api/clip/gone"), 1);
}

#[test]
fn get_clips_by_ids_matches_serial_results_and_keeps_order_when_concurrent() {
    // With no batch route the batch is unavailable, so both calls fall back
    // to per-id and must return the deduped input order regardless of the
    // concurrency used.
    let a = serde_json::json!({
        "id": "a", "title": "A", "status": "complete", "metadata": {"type": "gen"}
    })
    .to_string();
    let b = serde_json::json!({
        "id": "b", "title": "B", "status": "complete", "metadata": {"type": "gen"}
    })
    .to_string();
    let c = serde_json::json!({
        "id": "c", "title": "C", "status": "complete", "metadata": {"type": "gen"}
    })
    .to_string();
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/clip/a", Reply::json(&a))
        .route("/api/clip/b", Reply::json(&b))
        .route("/api/clip/c", Reply::json(&c));
    let client = scripted_client(&http, RecordingClock::new());
    let ids = ["b", "a", "c", "a"];

    let serial = pollster::block_on(client.get_clips_by_ids(&http, &ids, 1)).unwrap();
    let concurrent = pollster::block_on(client.get_clips_by_ids(&http, &ids, 4)).unwrap();

    let serial_ids: Vec<&str> = serial.iter().map(|clip| clip.id.as_str()).collect();
    let concurrent_ids: Vec<&str> = concurrent.iter().map(|clip| clip.id.as_str()).collect();
    assert_eq!(serial_ids, vec!["b", "a", "c"]);
    assert_eq!(concurrent_ids, serial_ids);
}

/// A minimal complete-clip body for the batch tests below.
fn clip_body(id: &str) -> String {
    format!(r#"{{"id":"{id}","title":"T","status":"complete","metadata":{{"type":"gen"}}}}"#)
}

#[test]
fn get_songs_by_ids_maps_the_batch_body_matched_by_id_in_input_order() {
    // The batch returns the clips out of order; the result must follow the
    // de-duplicated input order, matched by id, never the response position.
    let batch = format!(
        r#"{{"clips":[{},{},{}]}}"#,
        clip_body("c"),
        clip_body("a"),
        clip_body("b")
    );
    let http = ScriptedHttp::new()
        .with_auth()
        .route("get_songs_by_ids", Reply::json(&batch));
    let client = scripted_client(&http, RecordingClock::new());

    let clips = pollster::block_on(client.get_songs_by_ids(&http, &["a", "b", "c", "a"])).unwrap();
    let ids: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b", "c"], "input order, not response order");
    assert_eq!(http.count("get_songs_by_ids"), 1, "one chunk, one request");
}

#[test]
fn get_songs_by_ids_drops_clips_that_were_not_requested() {
    // A defensive body carrying an extra id must not leak into the result.
    let batch = format!(r#"{{"clips":[{},{}]}}"#, clip_body("a"), clip_body("x"));
    let http = ScriptedHttp::new()
        .with_auth()
        .route("get_songs_by_ids", Reply::json(&batch));
    let client = scripted_client(&http, RecordingClock::new());

    let clips = pollster::block_on(client.get_songs_by_ids(&http, &["a"])).unwrap();
    let ids: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
    assert_eq!(ids, vec!["a"], "an unrequested id is dropped");
}

#[test]
fn get_songs_by_ids_chunks_ids_beyond_the_chunk_size() {
    // 21 ids span two chunks (20 + 1), one batch request each, with the
    // input order preserved across the chunk boundary.
    let ids: Vec<String> = (0..21).map(|i| format!("id-{i:02}")).collect();
    let body = |slice: &[String]| {
        let clips: Vec<String> = slice.iter().map(|id| clip_body(id)).collect();
        format!(r#"{{"clips":[{}]}}"#, clips.join(","))
    };
    let http = ScriptedHttp::new().with_auth().route_seq(
        "get_songs_by_ids",
        vec![
            Reply::json(&body(&ids[..20])),
            Reply::json(&body(&ids[20..])),
        ],
    );
    let client = scripted_client(&http, RecordingClock::new());
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();

    let clips = pollster::block_on(client.get_songs_by_ids(&http, &refs)).unwrap();
    let got: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
    assert_eq!(got, refs, "all 21 ids returned in input order");
    assert_eq!(
        http.count("get_songs_by_ids"),
        2,
        "two chunks -> two requests"
    );
    let batch_calls: Vec<String> = http
        .calls()
        .into_iter()
        .filter(|url| url.contains("get_songs_by_ids"))
        .collect();
    assert_eq!(
        batch_calls[0].matches("ids=").count(),
        20,
        "first chunk of 20"
    );
    assert_eq!(
        batch_calls[1].matches("ids=").count(),
        1,
        "second chunk of 1"
    );
}

#[test]
fn get_clips_by_ids_batch_first_does_not_fetch_per_id_when_batch_is_complete() {
    // When the batch returns every requested id, no per-id request is made.
    let batch = format!(r#"{{"clips":[{},{}]}}"#, clip_body("a"), clip_body("b"));
    let http = ScriptedHttp::new()
        .with_auth()
        .route("get_songs_by_ids", Reply::json(&batch))
        .route("/api/clip/a", Reply::json(&clip_body("a")))
        .route("/api/clip/b", Reply::json(&clip_body("b")));
    let client = scripted_client(&http, RecordingClock::new());

    let clips = pollster::block_on(client.get_clips_by_ids(&http, &["a", "b"], 4)).unwrap();
    let ids: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b"]);
    assert_eq!(http.count("get_songs_by_ids"), 1);
    assert_eq!(
        http.count("/api/clip/"),
        0,
        "a complete batch needs no per-id fallback"
    );
}

#[test]
fn get_clips_by_ids_fills_ids_the_batch_omits_via_per_id() {
    // The batch returns only "a"; "b" is filled by a per-id fetch.
    let batch = format!(r#"{{"clips":[{}]}}"#, clip_body("a"));
    let http = ScriptedHttp::new()
        .with_auth()
        .route("get_songs_by_ids", Reply::json(&batch))
        .route("/api/clip/b", Reply::json(&clip_body("b")));
    let client = scripted_client(&http, RecordingClock::new());

    let clips = pollster::block_on(client.get_clips_by_ids(&http, &["a", "b"], 4)).unwrap();
    let ids: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b"], "omitted id is filled, order preserved");
    assert_eq!(http.count("/api/clip/a"), 0, "a came from the batch");
    assert_eq!(http.count("/api/clip/b"), 1, "b was filled per-id");
}

#[test]
fn get_clips_by_ids_falls_back_to_per_id_on_a_malformed_batch_body() {
    // A 200 body that is not `{"clips":[…]}` yields nothing for the chunk, so
    // every requested id is recovered by the per-id fallback.
    let http = ScriptedHttp::new()
        .with_auth()
        .route("get_songs_by_ids", Reply::json("not-json{"))
        .route("/api/clip/a", Reply::json(&clip_body("a")))
        .route("/api/clip/b", Reply::json(&clip_body("b")));
    let client = scripted_client(&http, RecordingClock::new());

    let clips = pollster::block_on(client.get_clips_by_ids(&http, &["a", "b"], 4)).unwrap();
    let ids: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b"]);
    assert_eq!(http.count("/api/clip/a"), 1);
    assert_eq!(http.count("/api/clip/b"), 1);
}

#[test]
fn get_clips_by_ids_propagates_a_batch_rate_limit_without_per_id_fan_out() {
    // A 429 that survives the retry budget propagates: it must never fan out
    // into a burst of per-id requests that would only deepen the throttling.
    let http = ScriptedHttp::new()
        .with_auth()
        .route("get_songs_by_ids", Reply::status(429))
        .route("/api/clip/a", Reply::json(&clip_body("a")))
        .route("/api/clip/b", Reply::json(&clip_body("b")));
    let client = scripted_client(&http, RecordingClock::new());

    let result = pollster::block_on(client.get_clips_by_ids(&http, &["a", "b"], 4));
    assert!(
        matches!(result, Err(Error::RateLimited { .. })),
        "an exhausted 429 propagates"
    );
    assert_eq!(
        http.count("/api/clip/"),
        0,
        "no per-id fan-out on rate-limit exhaustion"
    );
}

#[test]
fn concurrent_reads_share_aggregate_pacing_after_first_rate_limit() {
    // Batch-first: one `get_songs_by_ids` request (here returning nothing)
    // then four concurrent per-id fallbacks. All five share the 1 req/s
    // aggregate pacing, so from the first to the last reserved slot they span
    // ~4s, with a small tolerance for runtime scheduling jitter.
    const EXPECTED_SPAN: Duration = Duration::from_secs(4);
    const TOLERANCE: Duration = Duration::from_millis(50);
    let ids = ["a", "b", "c", "d"];
    let a = serde_json::json!({"id":"a","title":"A","status":"complete","metadata":{"type":"gen"}})
        .to_string();
    let b = serde_json::json!({"id":"b","title":"B","status":"complete","metadata":{"type":"gen"}})
        .to_string();
    let c = serde_json::json!({"id":"c","title":"C","status":"complete","metadata":{"type":"gen"}})
        .to_string();
    let d = serde_json::json!({"id":"d","title":"D","status":"complete","metadata":{"type":"gen"}})
        .to_string();
    let http = ScriptedHttp::new()
        .with_auth()
        .route_seq(
            "/api/feed/v3",
            vec![
                Reply::status(429),
                Reply::json(&one_clip_page("seed", None)),
            ],
        )
        .route("get_songs_by_ids", Reply::json(r#"{"clips":[]}"#))
        .route("/api/clip/a", Reply::json(&a))
        .route("/api/clip/b", Reply::json(&b))
        .route("/api/clip/c", Reply::json(&c))
        .route("/api/clip/d", Reply::json(&d));
    let clock = RecordingClock::new();
    let client = scripted_client(&http, clock.clone());
    pollster::block_on(client.list_clips(&http, false, Some(1))).unwrap();
    let before = clock.sleeps().len();

    let clips = pollster::block_on(client.get_clips_by_ids(&http, &ids, ids.len())).unwrap();
    assert_eq!(clips.len(), ids.len());
    let sleeps = clock.sleeps();
    let paced = &sleeps[before..];
    assert_eq!(
        paced.len(),
        ids.len() + 1,
        "one batch call plus four per-id"
    );
    let min = paced.iter().copied().min().unwrap();
    let max = paced.iter().copied().max().unwrap();
    let span = max.saturating_sub(min);
    // After the first 429, rate halves from 2 -> 1 req/s. Under shared slot
    // pacing, the batch call and the four per-id fallbacks are dispatched one
    // second apart in aggregate, so the first-to-last spacing is about four
    // seconds.
    assert!(span >= EXPECTED_SPAN.saturating_sub(TOLERANCE));
    assert!(span <= EXPECTED_SPAN + TOLERANCE);
}

#[test]
fn get_clip_parent_reads_the_parent_clip() {
    let parent = serde_json::json!({
        "id": "par", "title": "Ancestor", "status": "complete",
        "metadata": {"type": "gen"}
    })
    .to_string();
    let mut rules = auth_rules();
    rules.push(Rule::new("/api/clips/parent?clip_id=child", 200, parent));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let clip = pollster::block_on(client.get_clip_parent(&http, "child")).unwrap();
    assert_eq!(clip.unwrap().id, "par");
}

#[test]
fn get_clip_parent_is_none_for_a_root() {
    let mut rules = auth_rules();
    rules.push(Rule::new(
        "/api/clips/parent",
        404,
        r#"{"detail": "no parent"}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let clip = pollster::block_on(client.get_clip_parent(&http, "root")).unwrap();
    assert!(clip.is_none());
}

#[test]
fn get_clip_parent_is_none_for_a_200_no_id_root() {
    // The live "no parent" contract: HTTP 200 with a bodiless clip that has
    // no id (`{"is_public": false}`), not a 404. parse_clip gates on a
    // non-empty id, so it maps to Ok(None) rather than a bogus edge. Both
    // the bare and `{"clip": ...}`-wrapped encodings must behave the same.
    for body in [
        r#"{"is_public": false}"#,
        r#"{"clip": {"is_public": false}}"#,
    ] {
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/clips/parent", 200, body.to_string()));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let clip = pollster::block_on(client.get_clip_parent(&http, "root")).unwrap();
        assert!(clip.is_none(), "200-no-id body {body:?} must map to None");
    }
}

#[test]
fn get_clip_parent_reads_the_reduced_user_prefixed_shape() {
    // The parent endpoint returns a reduced shape with user_-prefixed
    // identity keys; after the dual-identity mapper fix the parent Clip
    // carries a non-empty display_name/handle (regression pin for #220).
    let parent = serde_json::json!({
        "id": "00000000-0000-4000-8000-000000000020",
        "title": "Track 2",
        "is_public": false,
        "user_display_name": "Example Artist 4",
        "user_handle": "example-artist-1",
        "user_avatar_image_url": "https://cdn1.suno.ai/avatar.jpg"
    })
    .to_string();
    let mut rules = auth_rules();
    rules.push(Rule::new("/api/clips/parent?clip_id=child", 200, parent));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let clip = pollster::block_on(client.get_clip_parent(&http, "child"))
        .unwrap()
        .expect("a parent clip with an id");
    assert_eq!(clip.id, "00000000-0000-4000-8000-000000000020");
    assert_eq!(clip.display_name, "Example Artist 4");
    assert_eq!(clip.handle, "example-artist-1");
    assert_eq!(clip.avatar_image_url, "https://cdn1.suno.ai/avatar.jpg");
}

#[test]
fn get_clip_parent_propagates_server_errors_instead_of_reporting_no_parent() {
    // A transient 5xx must never be mistaken for "this clip is a root":
    // folding it into Ok(None) would fabricate a wrong external root and let
    // a blip rewrite lineage (HARDENING H3). Only a real 404 means no parent.
    for status in [500u16, 503] {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            "/api/clips/parent",
            status,
            r#"{"detail": "server error"}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let result = pollster::block_on(client.get_clip_parent(&http, "child"));
        assert!(
            matches!(result, Err(Error::Api(_))),
            "status {status} must propagate as an error, not Ok(None)"
        );
    }
}

#[test]
fn get_playlists_maps_entries_and_skips_missing_ids() {
    let page1 = serde_json::json!({
        "playlists": [
            {"id": "pl1", "name": "Road Trip", "num_total_results": 12},
            {"id": "", "name": "No Id", "num_total_results": 3},
            {"name": "Also No Id"}
        ]
    })
    .to_string();
    let mut rules = auth_rules();
    // Page 1 returns entries; page 2 is empty, ending pagination.
    rules.push(Rule::new("/api/playlist/me?page=1", 200, page1));
    rules.push(Rule::new(
        "/api/playlist/me?page=2",
        200,
        r#"{"playlists": []}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let playlists = pollster::block_on(client.get_playlists(&http)).unwrap();
    assert_eq!(playlists.len(), 1, "entries without an id are dropped");
    assert_eq!(
        playlists[0],
        Playlist {
            id: "pl1".to_owned(),
            name: "Road Trip".to_owned(),
            num_clips: 12,
        }
    );
}

#[test]
fn get_playlists_defaults_a_missing_name_to_untitled() {
    let page1 = serde_json::json!({
        "playlists": [{"id": "pl9", "num_total_results": 1}]
    })
    .to_string();
    let mut rules = auth_rules();
    rules.push(Rule::new("/api/playlist/me?page=1", 200, page1));
    rules.push(Rule::new(
        "/api/playlist/me?page=2",
        200,
        r#"{"playlists": []}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let playlists = pollster::block_on(client.get_playlists(&http)).unwrap();
    assert_eq!(playlists[0].name, "Untitled");
}

#[test]
fn get_playlist_clips_preserves_order_and_unwraps_clip() {
    // Members arrive wrapped under `clip`, in playlist order, already
    // non-trashed. Order is preserved and no downloadability filter is applied.
    let body = serde_json::json!({
        "num_total_results": 2,
        "playlist_clips": [
            {"clip": {
                "id": "second", "title": "Second", "status": "complete",
                "metadata": {"duration": 60.0, "type": "gen"}
            }},
            {"clip": {
                "id": "first", "title": "First", "status": "complete",
                "metadata": {"duration": 30.0, "task": "infill", "type": "gen"}
            }}
        ]
    })
    .to_string();
    let mut rules = auth_rules();
    rules.push(Rule::new("/api/playlist/pl1/", 200, body));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let (clips, complete) = pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
    assert_eq!(clips.len(), 2, "an infill member is not filtered out");
    assert_eq!(clips[0].id, "second");
    assert_eq!(clips[1].id, "first");
    assert!(
        complete,
        "returned == num_total_results is fully enumerated"
    );
}

#[test]
fn get_playlist_clips_short_page_is_not_complete() {
    // A page with fewer entries than num_total_results is not authoritative.
    let body = serde_json::json!({
        "num_total_results": 5,
        "playlist_clips": [
            {"clip": {
                "id": "only", "title": "Only", "status": "complete",
                "metadata": {"duration": 60.0, "type": "gen"}
            }}
        ]
    })
    .to_string();
    let mut rules = auth_rules();
    rules.push(Rule::new("/api/playlist/pl1/", 200, body));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let (clips, complete) = pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
    assert_eq!(clips.len(), 1);
    assert!(!complete, "a short page is not fully enumerated");
}

#[test]
fn get_playlist_clips_is_empty_for_a_playlist_with_no_members() {
    let mut rules = auth_rules();
    rules.push(Rule::new(
        "/api/playlist/empty/",
        200,
        r#"{"num_total_results": 0, "playlist_clips": []}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let (clips, complete) = pollster::block_on(client.get_playlist_clips(&http, "empty")).unwrap();
    assert!(clips.is_empty());
    assert!(
        complete,
        "an empty playlist reporting zero total is complete"
    );
}

#[test]
fn get_playlist_clips_missing_total_is_not_complete() {
    // A body without num_total_results cannot be verified as whole, so it is
    // never authoritative -- an empty or malformed page must not let a Mirror
    // area delete from it (D5).
    let mut rules = auth_rules();
    rules.push(Rule::new(
        "/api/playlist/pl1/",
        200,
        r#"{"playlist_clips": []}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let (clips, complete) = pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
    assert!(clips.is_empty());
    assert!(!complete, "a missing total is never fully enumerated");
}

#[test]
fn get_playlist_clips_dropped_member_disarms_authority() {
    // A member whose clip carries no usable id is dropped by the empty-id
    // filter, so clips.len() < raw_len even when raw_len == num_total_results.
    // Both a missing `id` key and an empty-string `id` must disarm deletion
    // authority rather than silently arming a Mirror area on a short set.
    let missing_id = serde_json::json!({
        "num_total_results": 2,
        "playlist_clips": [
            {"clip": {
                "id": "a", "title": "A", "status": "complete",
                "metadata": {"duration": 60.0, "type": "gen"}
            }},
            {"clip": {
                "title": "No Id", "status": "complete",
                "metadata": {"duration": 30.0, "type": "gen"}
            }}
        ]
    })
    .to_string();
    let empty_id = serde_json::json!({
        "num_total_results": 2,
        "playlist_clips": [
            {"clip": {
                "id": "a", "title": "A", "status": "complete",
                "metadata": {"duration": 60.0, "type": "gen"}
            }},
            {"clip": {
                "id": "", "title": "Empty Id", "status": "complete",
                "metadata": {"duration": 30.0, "type": "gen"}
            }}
        ]
    })
    .to_string();
    for body in [missing_id, empty_id] {
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/playlist/pl1/", 200, body));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let (clips, complete) =
            pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
        assert_eq!(clips.len(), 1, "the member with no id is dropped");
        assert!(
            !complete,
            "a dropped member disarms authority even when raw_len == total"
        );
    }
}

#[test]
fn get_playlist_clips_over_count_is_not_complete() {
    // total=2 but three raw members (one with an empty id): clips.len()==2
    // matches the total, yet raw_len==3 does not. The two-conjunct gate must
    // reject this; a mis-simplification to `clips.len() == total` would wrongly
    // arm authority here.
    let body = serde_json::json!({
        "num_total_results": 2,
        "playlist_clips": [
            {"clip": {
                "id": "a", "title": "A", "status": "complete",
                "metadata": {"duration": 60.0, "type": "gen"}
            }},
            {"clip": {
                "id": "b", "title": "B", "status": "complete",
                "metadata": {"duration": 30.0, "type": "gen"}
            }},
            {"clip": {
                "id": "", "title": "Empty Id", "status": "complete",
                "metadata": {"duration": 45.0, "type": "gen"}
            }}
        ]
    })
    .to_string();
    let mut rules = auth_rules();
    rules.push(Rule::new("/api/playlist/pl1/", 200, body));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let (clips, complete) = pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
    assert_eq!(clips.len(), 2, "the empty-id member is dropped");
    assert!(
        !complete,
        "raw_len (3) diverging from the total (2) is not authoritative"
    );
}

#[test]
fn get_playlist_clips_ignores_song_count() {
    // The detail reports song_count=0 while num_total_results=1 for the same
    // playlist; completeness must trust num_total_results, so a single-member
    // page reads as complete instead of being compared against song_count.
    let body = serde_json::json!({
        "num_total_results": 1,
        "song_count": 0,
        "playlist_clips": [
            {"clip": {
                "id": "only", "title": "Only", "status": "complete",
                "metadata": {"duration": 60.0, "type": "gen"}
            }}
        ]
    })
    .to_string();
    let mut rules = auth_rules();
    rules.push(Rule::new("/api/playlist/pl1/", 200, body));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let (clips, complete) = pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
    assert_eq!(clips.len(), 1);
    assert!(
        complete,
        "completeness uses num_total_results, not song_count"
    );
}

#[test]
fn get_playlists_num_clips_ignores_song_count() {
    // song_count is unreliable across endpoints (15 in the listing, 0 in the
    // detail), so num_clips must come from num_total_results, never song_count.
    let page1 = serde_json::json!({
        "playlists": [
            {"id": "pl1", "name": "Road Trip", "num_total_results": 15, "song_count": 0}
        ]
    })
    .to_string();
    let mut rules = auth_rules();
    rules.push(Rule::new("/api/playlist/me?page=1", 200, page1));
    rules.push(Rule::new(
        "/api/playlist/me?page=2",
        200,
        r#"{"playlists": []}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let playlists = pollster::block_on(client.get_playlists(&http)).unwrap();
    assert_eq!(
        playlists[0].num_clips, 15,
        "num_clips reads num_total_results, not song_count"
    );
}

#[test]
fn get_playlists_dedupes_a_page_ignoring_server() {
    // A server that ignores `page` returns the same non-empty body for every
    // page, so the empty-page terminator never fires and MAX_PAGES bounds the
    // loop. Dedupe-by-id keeps the result to the true unique set instead of
    // MAX_PAGES copies.
    let same_body = serde_json::json!({
        "playlists": [
            {"id": "pl1", "name": "Road Trip", "num_total_results": 12},
            {"id": "pl2", "name": "Chill", "num_total_results": 7}
        ]
    })
    .to_string();
    let mut rules = auth_rules();
    rules.push(Rule::new("/api/playlist/me", 200, same_body));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let playlists = pollster::block_on(client.get_playlists(&http)).unwrap();
    assert_eq!(
        playlists.len(),
        2,
        "duplicates from a page-ignoring server are collapsed"
    );
    assert_eq!(playlists[0].id, "pl1");
    assert_eq!(playlists[1].id, "pl2");
}

#[test]
fn get_playlist_clips_preserves_array_order_over_created_at() {
    // relative_index ascends with array order while the wrapper created_at
    // values are non-monotonic. Members must stay in array order: the parser
    // never sorts by created_at (or any timestamp).
    let body = serde_json::json!({
        "num_total_results": 3,
        "playlist_clips": [
            {"clip": {
                "id": "a", "title": "A", "status": "complete",
                "metadata": {"duration": 60.0, "type": "gen"}
            }, "relative_index": 1.0, "created_at": "2026-06-08T00:00:00.000Z"},
            {"clip": {
                "id": "b", "title": "B", "status": "complete",
                "metadata": {"duration": 30.0, "type": "gen"}
            }, "relative_index": 2.0, "created_at": "2026-01-11T00:00:00.000Z"},
            {"clip": {
                "id": "c", "title": "C", "status": "complete",
                "metadata": {"duration": 45.0, "type": "gen"}
            }, "relative_index": 3.0, "created_at": "2026-05-15T00:00:00.000Z"}
        ]
    })
    .to_string();
    let mut rules = auth_rules();
    rules.push(Rule::new("/api/playlist/pl1/", 200, body));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let (clips, complete) = pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
    assert_eq!(
        clips.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["a", "b", "c"],
        "array order is preserved despite non-monotonic created_at"
    );
    assert!(complete, "three intact members equal the declared total");
}

/// A stems page body: each stem is a full clip object whose title carries
/// the label in a trailing parenthetical, as the live endpoint returns.
fn stem_page(stems: &[(&str, &str, &str)]) -> String {
    let entries: Vec<Value> = stems
        .iter()
        .map(|(id, label, url)| {
            serde_json::json!({
                "id": id,
                "title": format!("My Song ({label})"),
                "status": "complete",
                "audio_url": url,
            })
        })
        .collect();
    serde_json::json!({ "stems": entries }).to_string()
}

/// The page-count body for `GET /api/clip/{id}/stems/pages`.
fn stem_pages(pages: u32) -> String {
    serde_json::json!({ "pages": pages }).to_string()
}

#[test]
fn list_stems_drains_all_declared_pages_and_is_authoritative() {
    // Two 0-indexed pages, both drained: the stems concatenate in order and
    // the listing is authoritative (it declared its pages and held stems).
    let http = ScriptedHttp::new()
        .with_auth()
        .route("stems/pages", Reply::json(&stem_pages(2)))
        .route(
            "stems?page=0",
            Reply::json(&stem_page(&[
                ("s1", "Vocals", "https://cdn1.suno.ai/s1.mp3"),
                ("s2", "Drums", "https://cdn1.suno.ai/s2.mp3"),
            ])),
        )
        .route(
            "stems?page=1",
            Reply::json(&stem_page(&[("s3", "Bass", "https://cdn1.suno.ai/s3.mp3")])),
        );
    let client = scripted_client(&http, RecordingClock::new());

    let (stems, complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();
    assert_eq!(stems.len(), 3);
    assert_eq!(stems[0].id, "s1");
    assert_eq!(stems[0].label, "Vocals");
    assert_eq!(stems[0].url, "https://cdn1.suno.ai/s1.mp3");
    assert_eq!(stems[2].label, "Bass");
    assert!(
        complete,
        "a fully drained listing that returned stems is authoritative"
    );
}

#[test]
fn list_stems_zero_pages_is_indeterminate_never_empty() {
    // A clip with no stems answers `{"pages": 0}`. That must NOT be read as an
    // authoritative empty set, or it could delete local stems.
    let http = ScriptedHttp::new()
        .with_auth()
        .route("stems/pages", Reply::json(&stem_pages(0)));
    let client = scripted_client(&http, RecordingClock::new());

    let (stems, complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();
    assert!(stems.is_empty());
    assert!(
        !complete,
        "an empty listing is indeterminate, so existing stems are kept"
    );
}

#[test]
fn list_stems_missing_page_count_is_indeterminate() {
    // A `400`/`404` on the page-count endpoint (Suno's "no stems" answer) is
    // indeterminate, never an authoritative empty set.
    for status in [400u16, 404] {
        let http = ScriptedHttp::new()
            .with_auth()
            .route("stems/pages", Reply::status(status));
        let client = scripted_client(&http, RecordingClock::new());
        let (stems, complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();
        assert!(stems.is_empty(), "status {status}");
        assert!(!complete, "status {status} is indeterminate, not empty");
    }
}

#[test]
fn stem_page_count_5xx_with_invalid_page_body_is_not_no_stems() {
    // A `5xx` whose body happens to contain "Invalid page" must NOT be
    // classified as "no stems": body-text matching would misclassify it.
    // Only a genuine `400` status triggers the no-stems path.
    let http = ScriptedHttp::new()
        .with_auth()
        .route("stems/pages", Reply::with_body(500, "Invalid page"));
    let client = scripted_client(&http, RecordingClock::new());

    let result = pollster::block_on(client.list_stems(&http, "clip1"));
    assert!(
        result.is_err(),
        "a 5xx is a transient error, never 'no stems'"
    );
}

#[test]
fn list_stems_page_error_mid_enumeration_propagates() {
    // A transient 5xx on a page mid-drain is indeterminate, not an end: it
    // surfaces as an error rather than a (partial) authoritative set, so the
    // caller keeps existing stems.
    let http = ScriptedHttp::new()
        .with_auth()
        .route("stems/pages", Reply::json(&stem_pages(2)))
        .route(
            "stems?page=0",
            Reply::json(&stem_page(&[(
                "s1",
                "Vocals",
                "https://cdn1.suno.ai/s1.mp3",
            )])),
        )
        .route("stems?page=1", Reply::status(500));
    let client = scripted_client(&http, RecordingClock::new());

    let result = pollster::block_on(client.list_stems(&http, "clip1"));
    assert!(result.is_err(), "a 5xx page is not a clean drain");
}

#[test]
fn list_stems_over_max_pages_is_truncated_never_authoritative() {
    // A clip that declares more pages than the `MAX_PAGES` cap can only be
    // drained partially, so even though the fetched pages hold stems the
    // listing is TRUNCATED and must not be authoritative: its un-fetched
    // stems on pages beyond the cap would otherwise be delete-reconciled.
    let http = ScriptedHttp::new()
        .with_auth()
        .route("stems/pages", Reply::json(&stem_pages(MAX_PAGES + 1)))
        .route(
            "stems?page=",
            Reply::json(&stem_page(&[(
                "s1",
                "Vocals",
                "https://cdn1.suno.ai/s1.mp3",
            )])),
        );
    let client = scripted_client(&http, RecordingClock::new());

    let (stems, complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();
    assert!(!stems.is_empty(), "the fetched pages still yield stems");
    assert!(
        !complete,
        "a listing declaring more than MAX_PAGES is truncated, never authoritative"
    );
}

#[test]
fn list_stems_labels_the_inferred_populated_page_from_the_stem_group() {
    // The populated `/stems` shape was never captured for this account, so
    // it is inferred: each stem is a full clip whose structured
    // `metadata.stem_type_group_name` (underscore form) is the label, even
    // when the title carries no parenthetical. This pins the normaliser and
    // the group-over-title preference against the inferred fixture.
    let page = serde_json::json!({
        "stems": [{
            "id": "stem-bv",
            "title": "Track 30",
            "status": "complete",
            "audio_url": "https://cdn1.suno.ai/stem-bv.mp3",
            "metadata": {
                "stem_from_id": "source-074",
                "stem_task": "twelve",
                "stem_type_id": 91.0,
                "stem_type_group_name": "Backing_Vocals"
            }
        }]
    })
    .to_string();
    let http = ScriptedHttp::new()
        .with_auth()
        .route("stems/pages", Reply::json(&stem_pages(1)))
        .route("stems?page=0", Reply::json(&page));
    let client = scripted_client(&http, RecordingClock::new());

    let (stems, complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();
    assert_eq!(stems.len(), 1);
    assert_eq!(stems[0].id, "stem-bv");
    assert_eq!(
        stems[0].label, "Backing Vocals",
        "the underscore group name is normalised, not the empty title parenthetical"
    );
    assert_eq!(stems[0].url, "https://cdn1.suno.ai/stem-bv.mp3");
    assert!(
        complete,
        "a drained listing that returned a stem is authoritative"
    );
}

#[test]
fn post_allow_list_permits_only_feed_and_wav_render() {
    assert!(post_path_allowed(FEED_V3_PATH));
    assert!(post_path_allowed("/api/gen/abc123/convert_wav/"));
    // No generation endpoint is on the list.
    assert!(!post_path_allowed("/api/gen/abc123/stem_task"));
    assert!(!post_path_allowed("/api/gen/abc123/separate"));
    // Path traversal or extra segments can't smuggle a match.
    assert!(!post_path_allowed("/api/gen/a/../evil/convert_wav/"));
    assert!(!post_path_allowed("/api/gen/a/b/convert_wav/"));
    // The stems endpoints are GET-only and never on the POST allow-list.
    assert!(!post_path_allowed("/api/clip/x/stems/pages"));
    assert!(!post_path_allowed("/api/clip/x/stems?page=0"));
}

#[test]
fn api_request_refuses_a_post_off_the_allow_list() {
    // The single POST chokepoint rejects an off-list POST before the wire, so
    // a credit-spending endpoint can never be reached by accident.
    let http = MockHttp::new(auth_rules());
    let client = authed_client(&http);
    let err = pollster::block_on(client.api_request(
        &http,
        Method::Post,
        "/api/gen/x/stem_task",
        b"{}".to_vec(),
    ))
    .unwrap_err();
    assert!(matches!(err, Error::Refused(_)));
}
