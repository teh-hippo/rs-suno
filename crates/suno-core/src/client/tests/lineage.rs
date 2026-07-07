use super::*;

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
    // identity keys; the dual-identity mapper must yield a non-empty
    // display_name/handle (regression pin for #220).
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
