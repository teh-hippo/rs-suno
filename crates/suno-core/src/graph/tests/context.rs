use super::*;

#[test]
fn context_for_roots_a_remix_at_its_stored_ancestor() {
    let mut store = LineageStore::new();
    store.update(&chain_clips(), &chain_resolution(), "now");

    let child = &chain_clips()[0]; // "c", a cover of "b"
    let ctx = store.context_for(child);
    assert_eq!(ctx.root_id, "a");
    assert_eq!(ctx.root_title, "Root");
    assert_eq!(ctx.parent_id, "b");
    assert_eq!(ctx.edge_type, Some(EdgeType::Cover));
    assert_eq!(ctx.status, ResolveStatus::Resolved);
    // The remix folders under its resolved root's album.
    assert_eq!(ctx.album("Cover"), "Root");
}

#[test]
fn context_for_a_root_uses_its_own_title_and_has_no_parent() {
    let mut store = LineageStore::new();
    store.update(&chain_clips(), &chain_resolution(), "now");

    let root = &chain_clips()[2]; // "a"
    let ctx = store.context_for(root);
    assert_eq!(ctx.root_id, "a");
    assert_eq!(ctx.root_title, "Root");
    assert_eq!(ctx.parent_id, "");
    assert_eq!(ctx.edge_type, None);
    assert_eq!(ctx.album("Root"), "Root");
}

#[test]
fn context_for_tags_the_root_year_across_a_calendar_boundary() {
    // A December root with a January revision: both tag the root's year, so
    // the album groups under one year even across the boundary.
    let clips = vec![
        Clip {
            id: "child".into(),
            title: "Revision".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            created_at: "2024-01-02T08:00:00Z".into(),
            cover_clip_id: "root".into(),
            edited_clip_id: "root".into(),
            ..Default::default()
        },
        Clip {
            id: "root".into(),
            title: "Origin".into(),
            clip_type: "gen".into(),
            created_at: "2023-12-30T23:00:00Z".into(),
            ..Default::default()
        },
    ];
    let mut roots = HashMap::new();
    for id in ["child", "root"] {
        roots.insert(
            id.to_owned(),
            RootInfo {
                root_id: "root".into(),
                root_title: "Origin".into(),
                status: ResolveStatus::Resolved,
            },
        );
    }
    let resolution = Resolution {
        roots,
        gap_filled: Vec::new(),
        bridges: Vec::new(),
    };
    let mut store = LineageStore::new();
    store.update(&clips, &resolution, "now");

    let child_ctx = store.context_for(&clips[0]);
    assert_eq!(child_ctx.root_id, "root");
    assert_eq!(child_ctx.root_date, "2023-12-30T23:00:00Z");
    // The January child tags the December root's year, not its own 2024.
    assert_eq!(child_ctx.year(&clips[0].created_at), "2023");

    // The root tags its own year (the same year).
    let root_ctx = store.context_for(&clips[1]);
    assert_eq!(root_ctx.year(&clips[1].created_at), "2023");
}

#[test]
fn context_for_an_unknown_clip_is_self_rooted() {
    let store = LineageStore::new();
    let orphan = Clip {
        id: "z".into(),
        title: "Lonely".into(),
        ..Default::default()
    };
    let ctx = store.context_for(&orphan);
    assert_eq!(ctx.root_id, "z");
    assert_eq!(ctx.root_title, "Lonely");
    assert_eq!(ctx.parent_id, "");
    assert_eq!(ctx.status, ResolveStatus::Resolved);
}

#[test]
fn context_for_retains_a_purged_ancestor_album() {
    // The trashed ancestor arrives only via gap_filled, yet a later run
    // whose resolver failed (modelled here by simply not re-updating) must
    // still root the child at the archived ancestor with its stored title
    // (HARDENING H3).
    let child = Clip {
        id: "c".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "t".into(),
        edited_clip_id: "t".into(),
        ..Default::default()
    };
    let trashed = Clip {
        id: "t".into(),
        title: "Trashed Original".into(),
        clip_type: "gen".into(),
        is_trashed: true,
        ..Default::default()
    };
    let mut roots = HashMap::new();
    roots.insert(
        "c".to_owned(),
        RootInfo {
            root_id: "t".into(),
            root_title: "Trashed Original".into(),
            status: ResolveStatus::Resolved,
        },
    );
    let resolution = Resolution {
        roots,
        gap_filled: vec![trashed],
        bridges: Vec::new(),
    };
    let mut store = LineageStore::new();
    store.update(std::slice::from_ref(&child), &resolution, "now");

    let ctx = store.context_for(&child);
    assert_eq!(ctx.root_id, "t");
    assert_eq!(ctx.root_title, "Trashed Original");
    assert_eq!(ctx.album("Cover"), "Trashed Original");
}
