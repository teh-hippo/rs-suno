use super::*;

#[test]
fn normalise_slug_lowercases_joins_and_defaults() {
    assert_eq!(normalise_slug("remix"), "remix");
    assert_eq!(normalise_slug("Remix Cover"), "remix_cover");
    assert_eq!(
        normalise_slug("  Remix   Reuse Style "),
        "remix_reuse_style"
    );
    assert_eq!(normalise_slug(""), "attribution");
    assert_eq!(normalise_slug("   "), "attribution");
}

#[test]
fn album_for_id_matches_context_for_and_handles_unknown() {
    let mut store = LineageStore::new();
    store.update(&chain_clips(), &chain_resolution(), "now");

    // A child folds under its differently-titled root, agreeing with the
    // live-clip rule via context_for.
    assert_eq!(store.album_for_id("c"), "Root");
    let cover = &chain_clips()[0];
    assert_eq!(
        store.album_for_id("c"),
        store.context_for(cover).album(&cover.title)
    );
    // The root folders under its own title.
    assert_eq!(store.album_for_id("a"), "Root");
    // An id absent from the store folds to an empty own title.
    assert_eq!(store.album_for_id("missing"), "");
}

#[test]
fn serde_roundtrip_preserves_a_relational_shape() {
    let mut store = LineageStore::new();
    store.update(&chain_clips(), &chain_resolution(), "now");

    let json = serde_json::to_string(&store).unwrap();
    let back: LineageStore = serde_json::from_str(&json).unwrap();
    assert_eq!(store, back);

    let value: serde_json::Value = serde_json::to_value(&store).unwrap();
    assert_eq!(value.get("schema_version").unwrap(), 1);
    assert!(value.get("nodes").unwrap().is_object());
    assert!(value.get("edges").unwrap().is_array());
    assert!(value.get("resolution_cache").unwrap().is_object());
    assert!(value.get("edge_index").is_none());

    // Relational, not adjacency: a node carries no edges/parent of its own,
    // and an edge is a flat row keyed by child and parent.
    let node = value.get("nodes").unwrap().get("c").unwrap();
    assert!(node.get("edges").is_none());
    assert!(node.get("parent_id").is_none());
    let first_edge = value.get("edges").unwrap().get(0).unwrap();
    assert!(first_edge.get("child_id").is_some());
    assert!(first_edge.get("parent_id").is_some());
}

#[test]
fn partial_json_loads_with_defaults() {
    // An older/partial file missing whole collections and per-row fields
    // still loads: container and row defaults fill the gaps.
    let json = r#"{"nodes":{"x":{"title":"Kept"}},"edges":[{"child_id":"x","parent_id":"y"}]}"#;
    let store: LineageStore = serde_json::from_str(json).unwrap();
    assert_eq!(store.schema_version, 1);
    let node = store.node("x").unwrap();
    assert_eq!(node.title, "Kept");
    assert_eq!(node.status, NodeStatus::Observed);
    assert_eq!(store.edges[0].status, EdgeStatus::Active);
    assert!(store.resolution_cache.is_empty());
    // The album-art collection is additive: a store written before folder
    // art existed loads with no albums and no folder art.
    assert!(store.albums.is_empty());
    assert!(!store.albums.contains_key("x"));
    // The playlist collection is likewise additive: absent in an older
    // store, it defaults empty (HARDENING B2: no stored playlist means no
    // reconcile ever treats one as stale).
    assert!(store.playlists.is_empty());
    assert!(!store.playlists.contains_key("x"));
}

#[test]
fn on_disk_slugs_are_byte_identical_to_the_legacy_string_literals() {
    // The typed enums must serialize to the SAME slug strings as the old
    // hand-written literals. Any change here would corrupt existing stores.
    let mut store = LineageStore::new();
    store.update(&chain_clips(), &chain_resolution(), "now");

    let value = serde_json::to_value(&store).unwrap();
    let edges = value.get("edges").unwrap().as_array().unwrap();

    // There are two edges: c->b (cover/primary) and b->a (remaster/primary).
    let primary_edge = edges
        .iter()
        .find(|e| e.get("child_id").unwrap() == "c")
        .unwrap();
    assert_eq!(primary_edge.get("role").unwrap(), "primary");
    assert_eq!(primary_edge.get("status").unwrap(), "active");
    assert_eq!(primary_edge.get("edge_type").unwrap(), "cover");

    // Node status slug.
    let node = value.get("nodes").unwrap().get("c").unwrap();
    assert_eq!(node.get("status").unwrap(), "observed");

    // Cache entry status slug.
    let cache = value.get("resolution_cache").unwrap();
    assert_eq!(cache.get("a").unwrap().get("status").unwrap(), "resolved");

    // A non-resolved status also serialises to the correct slug.
    let mut store2 = LineageStore::new();
    let child = Clip {
        id: "x".into(),
        ..Default::default()
    };
    let mut roots = HashMap::new();
    roots.insert(
        "x".to_owned(),
        RootInfo {
            root_id: "ext".into(),
            root_title: String::new(),
            status: ResolveStatus::External,
        },
    );
    store2.update(
        std::slice::from_ref(&child),
        &Resolution {
            roots,
            gap_filled: Vec::new(),
            bridges: Vec::new(),
        },
        "now",
    );
    let v2 = serde_json::to_value(&store2).unwrap();
    assert_eq!(
        v2.get("resolution_cache")
            .unwrap()
            .get("x")
            .unwrap()
            .get("status")
            .unwrap(),
        "external"
    );
}

#[test]
fn serde_roundtrip_is_byte_identical() {
    // The typed enums must not change the wire format: a store serialised
    // and then re-serialised must produce the same bytes.
    let mut store = LineageStore::new();
    store.update(&chain_clips(), &chain_resolution(), "now");

    let first = serde_json::to_string(&store).unwrap();
    let back: LineageStore = serde_json::from_str(&first).unwrap();
    let second = serde_json::to_string(&back).unwrap();
    assert_eq!(first, second, "round-trip must be byte-identical");
}

#[test]
fn existing_string_form_json_deserialises_correctly() {
    // Existing on-disk stores use the plain slug strings; the typed enums
    // must still parse them correctly.
    let json = r#"{
        "nodes": {"a": {"title": "Root", "status": "observed"}},
        "edges": [{"child_id": "b", "parent_id": "a", "role": "primary", "status": "active", "edge_type": "cover"}],
        "resolution_cache": {"b": {"root_id": "a", "status": "resolved"}}
    }"#;
    let store: LineageStore = serde_json::from_str(json).unwrap();
    assert_eq!(store.node("a").unwrap().status, NodeStatus::Observed);
    assert_eq!(store.edges[0].role, EdgeRole::Primary);
    assert_eq!(store.edges[0].status, EdgeStatus::Active);
    assert_eq!(store.get_root("b").unwrap().status, ResolveStatus::Resolved);
    // archived_parents uses typed comparison: the loaded edge must be returned.
    let archived = store.archived_parents();
    assert_eq!(archived.get("b").map(String::as_str), Some("a"));
}

#[test]
fn full_store_json_is_stable_across_the_split() {
    // Golden anchor spanning ALL THREE domains at once: the lineage graph
    // (nodes + edges + resolution_cache), the album/playlist art state, and
    // the identity owner pin. If any field, serde attribute, key name,
    // nesting, or emitted key order ever drifts, this fails, guarding the
    // on-disk `.suno-lineage.json` format the domain split must not change.
    let mut store = LineageStore::new();
    store.update(&chain_clips(), &chain_resolution(), "now");
    store.albums.insert(
        "a".to_owned(),
        AlbumArt {
            folder_jpg: Some(crate::ArtifactState {
                path: "alice/Root/folder.jpg".to_owned(),
                hash: "jpg-h".to_owned(),
            }),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    store.playlists.insert(
        "liked".to_owned(),
        PlaylistState {
            name: "Liked".to_owned(),
            path: "Liked.m3u8".to_owned(),
            hash: "pl-h".to_owned(),
        },
    );
    store.pin_owner(Owner {
        user_id: "user_a".to_owned(),
        display_name: "Alice".to_owned(),
    });

    // (1) Exact key set, nesting, and values across every domain.
    let value = serde_json::to_value(&store).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "schema_version": 1,
            "nodes": {
                "a": {"title": "Root", "created_at": "t0", "clip_type": "gen", "task": "", "is_remix": false, "is_trashed": false, "status": "observed", "first_seen_at": "now", "last_seen_at": "now"},
                "b": {"title": "Remaster", "created_at": "t1", "clip_type": "upsample", "task": "upsample", "is_remix": false, "is_trashed": false, "status": "observed", "first_seen_at": "now", "last_seen_at": "now"},
                "c": {"title": "Cover", "created_at": "t2", "clip_type": "gen", "task": "cover", "is_remix": false, "is_trashed": false, "status": "observed", "first_seen_at": "now", "last_seen_at": "now"}
            },
            "edges": [
                {"child_id": "b", "parent_id": "a", "edge_type": "remaster", "role": "primary", "source_field": "upsample_clip_id", "ordinal": 0, "status": "active", "first_seen_at": "now", "last_seen_at": "now"},
                {"child_id": "c", "parent_id": "b", "edge_type": "cover", "role": "primary", "source_field": "cover_clip_id", "ordinal": 0, "status": "active", "first_seen_at": "now", "last_seen_at": "now"}
            ],
            "resolution_cache": {
                "a": {"root_id": "a", "status": "resolved", "algorithm_version": 1, "computed_at": "now"},
                "b": {"root_id": "a", "status": "resolved", "algorithm_version": 1, "computed_at": "now"},
                "c": {"root_id": "a", "status": "resolved", "algorithm_version": 1, "computed_at": "now"}
            },
            "albums": {"a": {"folder_jpg": {"path": "alice/Root/folder.jpg", "hash": "jpg-h"}}},
            "playlists": {"liked": {"name": "Liked", "path": "Liked.m3u8", "hash": "pl-h"}},
            "owner": {"user_id": "user_a", "display_name": "Alice"}
        })
    );

    // (2) Exact serialised bytes, including emitted key order (struct field
    // declaration order; map keys sorted). Any reordering fails here.
    let expected = r#"{"schema_version":1,"nodes":{"a":{"title":"Root","created_at":"t0","clip_type":"gen","task":"","is_remix":false,"is_trashed":false,"status":"observed","first_seen_at":"now","last_seen_at":"now"},"b":{"title":"Remaster","created_at":"t1","clip_type":"upsample","task":"upsample","is_remix":false,"is_trashed":false,"status":"observed","first_seen_at":"now","last_seen_at":"now"},"c":{"title":"Cover","created_at":"t2","clip_type":"gen","task":"cover","is_remix":false,"is_trashed":false,"status":"observed","first_seen_at":"now","last_seen_at":"now"}},"edges":[{"child_id":"b","parent_id":"a","edge_type":"remaster","role":"primary","source_field":"upsample_clip_id","ordinal":0,"status":"active","first_seen_at":"now","last_seen_at":"now"},{"child_id":"c","parent_id":"b","edge_type":"cover","role":"primary","source_field":"cover_clip_id","ordinal":0,"status":"active","first_seen_at":"now","last_seen_at":"now"}],"resolution_cache":{"a":{"root_id":"a","status":"resolved","algorithm_version":1,"computed_at":"now"},"b":{"root_id":"a","status":"resolved","algorithm_version":1,"computed_at":"now"},"c":{"root_id":"a","status":"resolved","algorithm_version":1,"computed_at":"now"}},"albums":{"a":{"folder_jpg":{"path":"alice/Root/folder.jpg","hash":"jpg-h"}}},"playlists":{"liked":{"name":"Liked","path":"Liked.m3u8","hash":"pl-h"}},"owner":{"user_id":"user_a","display_name":"Alice"}}"#;
    assert_eq!(serde_json::to_string(&store).unwrap(), expected);

    // (3) The three `#[serde(skip)]` runtime overlays never reach disk.
    assert!(value.get("album_overrides").is_none());
    assert!(value.get("eligible_root_ids").is_none());
    assert!(value.get("edge_index").is_none());

    // And it round-trips back to an equal store.
    let back: LineageStore = serde_json::from_str(expected).unwrap();
    assert_eq!(store, back);
}
