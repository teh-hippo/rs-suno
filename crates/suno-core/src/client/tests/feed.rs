use super::*;

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
