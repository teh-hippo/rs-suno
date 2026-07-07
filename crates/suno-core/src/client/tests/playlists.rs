use super::*;

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
