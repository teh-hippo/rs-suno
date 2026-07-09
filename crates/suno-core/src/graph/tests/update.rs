use super::*;

fn edge<'a>(store: &'a LineageStore, child: &str, parent: &str) -> &'a StoredEdge {
    store
        .edges
        .iter()
        .find(|e| e.child_id == child && e.parent_id == parent)
        .expect("edge should exist")
}

#[test]
fn new_store_is_empty_and_versioned() {
    let store = LineageStore::new();
    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
    assert_eq!(store.schema_version, 1);
}

#[test]
fn update_populates_nodes_edges_and_cache() {
    let mut store = LineageStore::new();
    store.update(&chain_clips(), &chain_resolution(), "now");

    // A node per clip, dated and typed from the clip.
    assert_eq!(store.len(), 3);
    let cover = store.node("c").unwrap();
    assert_eq!(cover.title, "Cover");
    assert_eq!(cover.clip_type, "gen");
    assert_eq!(cover.task, "cover");
    assert_eq!(cover.created_at, "t2");
    assert_eq!(cover.status, NodeStatus::Observed);
    assert!(!cover.is_trashed);
    assert_eq!(cover.first_seen_at, "now");
    assert_eq!(cover.last_seen_at, "now");

    // One primary edge per non-root clip; the root emits none.
    assert_eq!(store.edges.len(), 2);
    let cb = edge(&store, "c", "b");
    assert_eq!(cb.edge_type, "cover");
    assert_eq!(cb.role, EdgeRole::Primary);
    assert_eq!(cb.ordinal, 0);
    assert_eq!(cb.source_field, "cover_clip_id");
    assert_eq!(cb.status, EdgeStatus::Active);
    let ba = edge(&store, "b", "a");
    assert_eq!(ba.edge_type, "remaster");
    assert!(!store.edges.iter().any(|e| e.child_id == "a"));

    // The cache roots every clip at `a`, resolved.
    for id in ["a", "b", "c"] {
        let cached = store.get_root(id).unwrap();
        assert_eq!(cached.root_id, "a");
        assert_eq!(cached.status, ResolveStatus::Resolved);
        assert_eq!(cached.algorithm_version, 1);
    }
}

#[test]
fn update_persists_edges_for_gap_filled_ancestors() {
    // A gap-filled intermediate carries its own parent pointer; update()
    // must record ITS edge (not only the input clips'), so the stored graph
    // stays connected and a later run resolves through it without a fetch.
    let child = Clip {
        id: "child".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "mid".into(),
        edited_clip_id: "mid".into(),
        ..Default::default()
    };
    let mid = Clip {
        id: "mid".into(),
        title: "Mid".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "root".into(),
        edited_clip_id: "root".into(),
        ..Default::default()
    };
    let mut roots = HashMap::new();
    roots.insert(
        "child".to_owned(),
        RootInfo {
            root_id: "root".into(),
            root_title: "Original".into(),
            status: ResolveStatus::Resolved,
        },
    );
    let resolution = Resolution {
        roots,
        gap_filled: vec![mid],
        bridges: Vec::new(),
    };
    let mut store = LineageStore::new();
    store.update(std::slice::from_ref(&child), &resolution, "now");

    // The gap-filled ancestor's own edge is persisted.
    let mid_edge = edge(&store, "mid", "root");
    assert_eq!(mid_edge.role, EdgeRole::Primary);
    assert_eq!(mid_edge.ordinal, 0);
    // Both hops are now reachable from the archive for a later resolve.
    let archived = store.archived_parents();
    assert_eq!(archived.get("child").map(String::as_str), Some("mid"));
    assert_eq!(archived.get("mid").map(String::as_str), Some("root"));
}

#[test]
fn update_persists_bridges_as_edges() {
    // A parent-endpoint bridge has no clip of its own, so it is persisted
    // directly as a primary edge to keep that hop durable.
    let child = Clip {
        id: "child".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "gone".into(),
        edited_clip_id: "gone".into(),
        ..Default::default()
    };
    let mut roots = HashMap::new();
    roots.insert(
        "child".to_owned(),
        RootInfo {
            root_id: "found".into(),
            root_title: String::new(),
            status: ResolveStatus::External,
        },
    );
    let resolution = Resolution {
        roots,
        gap_filled: Vec::new(),
        bridges: vec![("gone".to_owned(), "found".to_owned())],
    };
    let mut store = LineageStore::new();
    store.update(std::slice::from_ref(&child), &resolution, "now");

    let bridged = edge(&store, "gone", "found");
    assert_eq!(bridged.source_field, "parent_endpoint");
    assert_eq!(bridged.role, EdgeRole::Primary);
    assert_eq!(bridged.ordinal, 0);
    assert_eq!(
        store.archived_parents().get("gone").map(String::as_str),
        Some("found")
    );
}

#[test]
fn archived_parents_maps_children_to_primary_parents_only() {
    let mut store = LineageStore::new();
    store.update(&chain_clips(), &chain_resolution(), "now");
    let archived = store.archived_parents();
    assert_eq!(archived.get("c").map(String::as_str), Some("b"));
    assert_eq!(archived.get("b").map(String::as_str), Some("a"));
    assert!(
        !archived.contains_key("a"),
        "a root has no primary parent edge"
    );
}

#[test]
fn update_persists_attribution_edges_without_polluting_resolution() {
    // A clip whose clip_roots point at a different node than its structural
    // parent: the attribution edge is stored (role Secondary, open slug),
    // but it must NOT be read by archived_parents (which seeds resolution)
    // nor appear in the resolution cache's root set.
    let child = Clip {
        id: "child".into(),
        title: "Remix".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "struct-parent".into(),
        edited_clip_id: "struct-parent".into(),
        handle: "me".into(),
        clip_attribution_type: "remix".into(),
        clip_roots: vec![crate::model::ClipRoot {
            id: "attr-root".into(),
            handle: "me".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut roots = HashMap::new();
    roots.insert(
        "child".to_owned(),
        RootInfo {
            root_id: "struct-parent".into(),
            root_title: "Structural Root".into(),
            status: ResolveStatus::Resolved,
        },
    );
    let resolution = Resolution {
        roots,
        gap_filled: Vec::new(),
        bridges: Vec::new(),
    };
    let mut store = LineageStore::new();
    store.update(std::slice::from_ref(&child), &resolution, "now");

    // The attribution edge is stored as a Secondary with the open slug.
    let attr = edge(&store, "child", "attr-root");
    assert_eq!(attr.edge_type, "remix");
    assert_eq!(attr.role, EdgeRole::Secondary);
    assert_eq!(attr.ordinal, 0);
    assert_eq!(attr.source_field, "clip_roots");

    // The structural edge is separate and unaffected.
    let structural = edge(&store, "child", "struct-parent");
    assert_eq!(structural.role, EdgeRole::Primary);

    // Deletion/resolution safety: the attribution edge never seeds a walk.
    let archived = store.archived_parents();
    assert_eq!(
        archived.get("child").map(String::as_str),
        Some("struct-parent"),
        "archived_parents reads only the structural primary, never clip_roots"
    );
    assert_eq!(
        store.get_root("child").unwrap().root_id,
        "struct-parent",
        "the resolution cache roots at the structural parent, not the attribution root"
    );
}

#[test]
fn update_defaults_a_blank_attribution_type_to_attribution() {
    // clip_roots present with a blank clip_attribution_type still records an
    // edge, slugged "attribution" so it always carries a type.
    let child = Clip {
        id: "child".into(),
        title: "Remix".into(),
        handle: "me".into(),
        clip_attribution_type: String::new(),
        clip_roots: vec![crate::model::ClipRoot {
            id: "attr-root".into(),
            handle: "me".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut roots = HashMap::new();
    roots.insert(
        "child".to_owned(),
        RootInfo {
            root_id: "child".into(),
            root_title: "Remix".into(),
            status: ResolveStatus::Resolved,
        },
    );
    let resolution = Resolution {
        roots,
        gap_filled: Vec::new(),
        bridges: Vec::new(),
    };
    let mut store = LineageStore::new();
    store.update(std::slice::from_ref(&child), &resolution, "now");
    assert_eq!(edge(&store, "child", "attr-root").edge_type, "attribution");
}

#[test]
fn update_is_idempotent_bar_last_seen() {
    let clips = chain_clips();
    let resolution = chain_resolution();
    let mut store = LineageStore::new();
    store.update(&clips, &resolution, "first");
    let node_ids: Vec<String> = store.iter().map(|(id, _)| id.clone()).collect();
    let edge_count = store.edges.len();

    store.update(&clips, &resolution, "second");

    // No new nodes, edges, or cache rows: the second run only refreshes.
    assert_eq!(
        store.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        node_ids
    );
    assert_eq!(store.edges.len(), edge_count, "edges must not duplicate");
    assert_eq!(store.resolution_cache.len(), 3);

    // first_seen_at sticks; last_seen_at advances.
    let cover = store.node("c").unwrap();
    assert_eq!(cover.first_seen_at, "first");
    assert_eq!(cover.last_seen_at, "second");
    let cb = edge(&store, "c", "b");
    assert_eq!(cb.first_seen_at, "first");
    assert_eq!(cb.last_seen_at, "second");
    // Root ids are stable across the re-run.
    assert_eq!(store.get_root("c").unwrap().root_id, "a");
}

#[test]
fn update_after_roundtrip_rebuilds_edge_index_without_duplicates() {
    let clips = chain_clips();
    let resolution = chain_resolution();

    let mut store = LineageStore::new();
    store.update(&clips, &resolution, "first");

    let json = serde_json::to_string(&store).unwrap();
    let mut store: LineageStore = serde_json::from_str(&json).unwrap();

    store.update(&clips, &resolution, "second");

    assert_eq!(store.edges.len(), 2);
    let cb = edge(&store, "c", "b");
    assert_eq!(cb.first_seen_at, "first");
    assert_eq!(cb.last_seen_at, "second");
    let ba = edge(&store, "b", "a");
    assert_eq!(ba.first_seen_at, "first");
    assert_eq!(ba.last_seen_at, "second");
}

#[test]
fn cache_is_monotonic_and_never_downgrades_a_resolved_root() {
    let mut store = LineageStore::new();
    store.update(&chain_clips(), &chain_resolution(), "first");
    assert_eq!(store.get_root("c").unwrap().status, ResolveStatus::Resolved);

    // A later run where `c` fails to resolve (a transient gap-fill miss)
    // and a brand-new clip `d` that only reaches an external boundary.
    let child = Clip {
        id: "c".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "b".into(),
        edited_clip_id: "b".into(),
        ..Default::default()
    };
    let mut roots = HashMap::new();
    roots.insert(
        "c".to_owned(),
        RootInfo {
            root_id: "elsewhere".into(),
            root_title: String::new(),
            status: ResolveStatus::External,
        },
    );
    roots.insert(
        "d".to_owned(),
        RootInfo {
            root_id: "boundary".into(),
            root_title: String::new(),
            status: ResolveStatus::External,
        },
    );
    let resolution = Resolution {
        roots,
        gap_filled: Vec::new(),
        bridges: Vec::new(),
    };
    store.update(&[child], &resolution, "second");

    // The resolved root of `c` is kept, not downgraded.
    let cached = store.get_root("c").unwrap();
    assert_eq!(cached.root_id, "a");
    assert_eq!(cached.status, ResolveStatus::Resolved);
    assert_eq!(cached.computed_at, "first");
    // A never-resolved clip records its last-known non-resolved status.
    let d = store.get_root("d").unwrap();
    assert_eq!(d.root_id, "boundary");
    assert_eq!(d.status, ResolveStatus::External);
}

#[test]
fn gap_filled_trashed_ancestor_is_a_durable_node() {
    // The trashed ancestor is not among `clips`; it arrives only via the
    // resolution's gap_filled set, yet must be archived as a node so its
    // lineage survives Suno's purge (HARDENING H4 / L2).
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
    store_update_and_assert_trashed(child, resolution);
}

fn store_update_and_assert_trashed(child: Clip, resolution: Resolution) {
    let mut store = LineageStore::new();
    store.update(&[child], &resolution, "now");

    let node = store
        .node("t")
        .expect("trashed ancestor should be archived");
    assert!(node.is_trashed);
    assert_eq!(node.title, "Trashed Original");
    // The child roots at the trashed ancestor.
    assert_eq!(store.get_root("c").unwrap().root_id, "t");
}
