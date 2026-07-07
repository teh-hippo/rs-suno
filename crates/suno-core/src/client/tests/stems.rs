use super::*;

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
