use super::*;

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
