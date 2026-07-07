//! The lineage-store test suite: end-to-end scenarios that populate a store
//! via [`LineageStore::update`] and assert its query, cache, and serde shape.

use std::collections::HashMap;

use super::node::{EdgeStatus, NodeStatus};
use super::store::normalise_slug;
use super::*;
use crate::album_art::{AlbumArt, PlaylistState};
use crate::identity::Owner;
use crate::lineage::{EdgeRole, EdgeType, LineageContext, Resolution, ResolveStatus, RootInfo};
use crate::model::Clip;

fn chain_clips() -> Vec<Clip> {
    vec![
        Clip {
            id: "c".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            created_at: "t2".into(),
            cover_clip_id: "b".into(),
            edited_clip_id: "b".into(),
            ..Default::default()
        },
        Clip {
            id: "b".into(),
            title: "Remaster".into(),
            clip_type: "upsample".into(),
            task: "upsample".into(),
            created_at: "t1".into(),
            upsample_clip_id: "a".into(),
            edited_clip_id: "a".into(),
            ..Default::default()
        },
        Clip {
            id: "a".into(),
            title: "Root".into(),
            clip_type: "gen".into(),
            created_at: "t0".into(),
            ..Default::default()
        },
    ]
}

/// The matching resolution: every clip roots at `a`, all resolved.
fn chain_resolution() -> Resolution {
    let mut roots = HashMap::new();
    for id in ["a", "b", "c"] {
        roots.insert(
            id.to_owned(),
            RootInfo {
                root_id: "a".into(),
                root_title: "Root".into(),
                status: ResolveStatus::Resolved,
            },
        );
    }
    Resolution {
        roots,
        gap_filled: Vec::new(),
        bridges: Vec::new(),
    }
}

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
        clip_attribution_type: "".into(),
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
fn album_overrides_are_runtime_only_and_never_persist() {
    // Overrides come from config each run, so they must not serialise into
    // the durable graph or survive a round-trip (they would then outlive the
    // config entry that set them).
    let mut store = LineageStore::new();
    store.update(&chain_clips(), &chain_resolution(), "now");
    store.set_album_overrides(
        [("a".to_owned(), "Preferred".to_owned())]
            .into_iter()
            .collect(),
    );

    let value: serde_json::Value = serde_json::to_value(&store).unwrap();
    assert!(value.get("album_overrides").is_none());

    let json = serde_json::to_string(&store).unwrap();
    let back: LineageStore = serde_json::from_str(&json).unwrap();
    assert!(back.album_overrides.is_empty());
    assert_eq!(back.album_for_id("c"), "Root");
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

#[test]
fn colliding_root_titles_flags_only_shared_distinct_roots() {
    // Two distinct roots share the title "Break Through"; a third root is
    // unique; a child of a shared root does not add a spurious distinct root.
    let clips = vec![
        Clip {
            id: "r1".into(),
            title: "Break Through".into(),
            clip_type: "gen".into(),
            ..Default::default()
        },
        Clip {
            id: "r2".into(),
            title: "Break Through".into(),
            clip_type: "gen".into(),
            ..Default::default()
        },
        Clip {
            id: "r3".into(),
            title: "Solo".into(),
            clip_type: "gen".into(),
            ..Default::default()
        },
        Clip {
            id: "c1".into(),
            title: "Break Through".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "r1".into(),
            edited_clip_id: "r1".into(),
            ..Default::default()
        },
    ];
    let mut roots = HashMap::new();
    for (id, root) in [("r1", "r1"), ("r2", "r2"), ("r3", "r3"), ("c1", "r1")] {
        let title = if root == "r3" {
            "Solo"
        } else {
            "Break Through"
        };
        roots.insert(
            id.to_owned(),
            RootInfo {
                root_id: root.into(),
                root_title: title.into(),
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

    let colliding = store.colliding_root_titles();
    assert!(colliding.contains("Break Through"));
    assert!(!colliding.contains("Solo"));
    assert_eq!(colliding.len(), 1);
}

/// Build the two-distinct-root store used by the disambiguation tests: `r1`
/// and `r2` are separate `gen` roots titled `t1`/`t2`.
fn two_root_store(t1: &str, t2: &str) -> LineageStore {
    let clips = vec![
        Clip {
            id: "r1".into(),
            title: t1.into(),
            clip_type: "gen".into(),
            ..Default::default()
        },
        Clip {
            id: "r2".into(),
            title: t2.into(),
            clip_type: "gen".into(),
            ..Default::default()
        },
    ];
    let mut roots = HashMap::new();
    roots.insert(
        "r1".to_owned(),
        RootInfo {
            root_id: "r1".into(),
            root_title: t1.into(),
            status: ResolveStatus::Resolved,
        },
    );
    roots.insert(
        "r2".to_owned(),
        RootInfo {
            root_id: "r2".into(),
            root_title: t2.into(),
            status: ResolveStatus::Resolved,
        },
    );
    let mut store = LineageStore::new();
    store.update(
        &clips,
        &Resolution {
            roots,
            gap_filled: Vec::new(),
            bridges: Vec::new(),
        },
        "now",
    );
    store
}

#[test]
fn album_override_flows_into_context_tag_hash_and_index() {
    // Override the lineage root's album name; every album-bearing surface
    // (the resolved context, the ALBUM tag, the change hash, and the
    // id-only index) must reflect the preferred name from one source.
    let clips = chain_clips();
    let mut store = LineageStore::new();
    store.update(&clips, &chain_resolution(), "now");

    let cover = &clips[0]; // "Cover", rooted at "a" ("Root")
    let before_hash = crate::hash::meta_hash(cover, &store.context_for(cover));

    store.set_album_overrides(
        [("a".to_owned(), "Preferred Name".to_owned())]
            .into_iter()
            .collect(),
    );

    // Every clip in the lineage now folders under the preferred album.
    for id in ["a", "b", "c"] {
        let clip = clips.iter().find(|c| c.id == id).unwrap();
        let ctx = store.context_for(clip);
        assert_eq!(ctx.album(&clip.title), "Preferred Name");
        assert_eq!(store.album_for_id(id), "Preferred Name");
    }

    // The ALBUM tag follows the override.
    let ctx = store.context_for(cover);
    let meta = crate::tag::TrackMetadata::from_clip(cover, &ctx);
    assert_eq!(meta.album, "Preferred Name");

    // The change hash shifts, so reconcile retags the file in place.
    let after_hash = crate::hash::meta_hash(cover, &ctx);
    assert_ne!(before_hash, after_hash);
}

#[test]
fn empty_album_override_is_ignored() {
    // A blank value must never blank an album; the derived title stands.
    let clips = chain_clips();
    let mut store = LineageStore::new();
    store.update(&clips, &chain_resolution(), "now");
    store.set_album_overrides([("a".to_owned(), "   ".to_owned())].into_iter().collect());
    assert_eq!(store.album_for_id("c"), "Root");
}

#[test]
fn album_override_creates_a_collision_that_disambiguates() {
    // Two uniquely titled roots collide once one is renamed onto the other.
    let mut store = two_root_store("Alpha", "Beta");
    assert!(store.colliding_root_titles().is_empty());

    store.set_album_overrides(
        [("r2".to_owned(), "Alpha".to_owned())]
            .into_iter()
            .collect(),
    );
    let colliding = store.colliding_root_titles();
    assert!(colliding.contains("Alpha"));
    assert_eq!(colliding.len(), 1);
}

#[test]
fn album_override_resolves_a_natural_collision() {
    // Two roots share a title; renaming one apart settles the collision.
    let mut store = two_root_store("Break Through", "Break Through");
    assert!(store.colliding_root_titles().contains("Break Through"));

    store.set_album_overrides(
        [("r2".to_owned(), "Second Wind".to_owned())]
            .into_iter()
            .collect(),
    );
    assert!(store.colliding_root_titles().is_empty());
}

/// Insert a cache-only root: an entry in the resolution cache whose root_id
/// has NO backing node (an external or not-yet-archived root). Such a root
/// still folders under a configured override, so it must be visible to
/// collision detection.
fn insert_cache_only_root(store: &mut LineageStore, root_id: &str) {
    store.resolution_cache.insert(
        root_id.to_owned(),
        CacheEntry {
            root_id: root_id.to_owned(),
            status: ResolveStatus::External,
            algorithm_version: 1,
            computed_at: "now".to_owned(),
        },
    );
    // Direct cache mutation bypasses `update`, so mirror what a real run does
    // after loading: refresh the derived eligible-root set.
    store.refresh_eligible_roots();
}

#[test]
fn override_on_node_less_root_collides_with_a_real_root() {
    // A node-less (cache-only) root overridden onto a real root's title must
    // be flagged as colliding, so both albums get the [root_id8] suffix and
    // two distinct roots never share one folder.
    let mut store = LineageStore::new();
    store.update(
        std::slice::from_ref(&Clip {
            id: "realroot".into(),
            title: "Shared".into(),
            clip_type: "gen".into(),
            ..Default::default()
        }),
        &Resolution {
            roots: [(
                "realroot".to_owned(),
                RootInfo {
                    root_id: "realroot".into(),
                    root_title: "Shared".into(),
                    status: ResolveStatus::Resolved,
                },
            )]
            .into_iter()
            .collect(),
            gap_filled: Vec::new(),
            bridges: Vec::new(),
        },
        "now",
    );
    insert_cache_only_root(&mut store, "extroot");
    store.set_album_overrides(
        [("extroot".to_owned(), "Shared".to_owned())]
            .into_iter()
            .collect(),
    );

    let colliding = store.colliding_root_titles();
    assert!(
        colliding.contains("Shared"),
        "a node-less overridden root must still be seen by collision detection"
    );
}

#[test]
fn two_node_less_roots_overridden_to_same_name_collide() {
    let mut store = LineageStore::new();
    insert_cache_only_root(&mut store, "extone");
    insert_cache_only_root(&mut store, "exttwo");
    store.set_album_overrides(
        [
            ("extone".to_owned(), "Shared".to_owned()),
            ("exttwo".to_owned(), "Shared".to_owned()),
        ]
        .into_iter()
        .collect(),
    );
    assert!(store.colliding_root_titles().contains("Shared"));
}

#[test]
fn colliding_node_less_overrides_keep_album_art_paths_distinct() {
    // End-to-end guard: two node-less roots overridden to one name must not
    // collapse their album-art (folder.jpg) onto a single shared path. The
    // colliding set drives naming to append [root_id8], giving each root its
    // own album folder and so its own folder.jpg.
    let mut store = LineageStore::new();
    insert_cache_only_root(&mut store, "aaaaaaaa-root-one");
    insert_cache_only_root(&mut store, "bbbbbbbb-root-two");
    store.set_album_overrides(
        [
            ("aaaaaaaa-root-one".to_owned(), "Shared".to_owned()),
            ("bbbbbbbb-root-two".to_owned(), "Shared".to_owned()),
        ]
        .into_iter()
        .collect(),
    );
    let colliding = store.colliding_root_titles();

    let clip_of = |id: &str| Clip {
        id: id.to_owned(),
        title: "Track".to_owned(),
        display_name: "alice".to_owned(),
        image_large_url: "https://art.example/large.jpg".to_owned(),
        ..Default::default()
    };
    let ctx_of = |root_id: &str| LineageContext {
        root_id: root_id.to_owned(),
        root_title: "Shared".to_owned(),
        root_date: String::new(),
        parent_id: String::new(),
        edge_type: None,
        status: ResolveStatus::Resolved,
    };
    let clip_a = clip_of("clipaaaa-1111");
    let clip_b = clip_of("clipbbbb-2222");
    let ctx_a = ctx_of("aaaaaaaa-root-one");
    let ctx_b = ctx_of("bbbbbbbb-root-two");
    let requests = [
        crate::naming::NamingRequest {
            clip: &clip_a,
            lineage: &ctx_a,
        },
        crate::naming::NamingRequest {
            clip: &clip_b,
            lineage: &ctx_b,
        },
    ];
    let names = crate::naming::render_clip_names(
        &requests,
        &crate::naming::NamingConfig::default(),
        &colliding,
    );

    let desired_of = |clip: &Clip, ctx: &LineageContext, name: &crate::naming::RenderedName| {
        crate::reconcile::Desired {
            clip: clip.clone(),
            lineage: ctx.clone(),
            path: format!(
                "{}.flac",
                crate::desired::rel_to_string(&name.relative_path)
            ),
            format: crate::AudioFormat::Flac,
            meta_hash: String::new(),
            art_hash: String::new(),
            modes: vec![crate::vocab::SourceMode::Mirror],
            trashed: false,
            private: false,
            artifacts: Vec::new(),
            stems: None,
        }
    };
    let desired = vec![
        desired_of(&clip_a, &ctx_a, &names[0]),
        desired_of(&clip_b, &ctx_b, &names[1]),
    ];

    let albums = crate::reconcile::album_desired(
        &desired,
        false,
        false,
        crate::vocab::WebpEncodeSettings::default(),
    );
    assert_eq!(albums.len(), 2, "each distinct root is its own album");
    let jpg_paths: Vec<String> = albums
        .iter()
        .filter_map(|a| a.folder_jpg.as_ref().map(|art| art.path.clone()))
        .collect();
    assert_eq!(jpg_paths.len(), 2, "both albums have a folder.jpg");
    assert_ne!(
        jpg_paths[0], jpg_paths[1],
        "colliding roots must not share one folder.jpg path"
    );
}

#[test]
fn override_on_uncached_selected_root_is_ignored_and_keeps_albums_distinct() {
    // Residual-bug guard. On a resolution-FAILED run (no store.update), a
    // newly-listed clip is uncached, so context_for falls back to a
    // self-root (root_id = clip.id) that colliding_root_titles cannot see.
    // An override on that id must NOT apply this run, or the clip would
    // render/tag under a stored root's title with no [root_id8] suffix and
    // collapse two distinct albums onto one folder (and one folder.jpg).
    let mut store = LineageStore::new();
    store.update(
        std::slice::from_ref(&Clip {
            id: "realroot".into(),
            title: "Shared".into(),
            clip_type: "gen".into(),
            ..Default::default()
        }),
        &Resolution {
            roots: [(
                "realroot".to_owned(),
                RootInfo {
                    root_id: "realroot".into(),
                    root_title: "Shared".into(),
                    status: ResolveStatus::Resolved,
                },
            )]
            .into_iter()
            .collect(),
            gap_filled: Vec::new(),
            bridges: Vec::new(),
        },
        "now",
    );
    // A newly-listed clip that failed to resolve this run: it is NOT in the
    // cache, and config overrides its id onto the stored root's title.
    let new_clip = Clip {
        id: "newnewnew-9999".into(),
        title: "Solo Track".into(),
        display_name: "alice".into(),
        image_large_url: "https://art.example/large.jpg".into(),
        ..Default::default()
    };
    store.set_album_overrides(
        [("newnewnew-9999".to_owned(), "Shared".to_owned())]
            .into_iter()
            .collect(),
    );

    // The uncached clip folders under its OWN title, not the override.
    let new_ctx = store.context_for(&new_clip);
    assert_eq!(new_ctx.root_id, "newnewnew-9999");
    assert_eq!(new_ctx.album(&new_clip.title), "Solo Track");

    // Collision detection is unchanged: the stored root stands alone.
    assert!(store.colliding_root_titles().is_empty());

    // Album-art paths for the two distinct roots stay distinct.
    let real_clip = Clip {
        id: "realroot".into(),
        title: "Shared".into(),
        display_name: "alice".into(),
        image_large_url: "https://art.example/large.jpg".into(),
        ..Default::default()
    };
    let real_ctx = store.context_for(&real_clip);
    let colliding = store.colliding_root_titles();
    let requests = [
        crate::naming::NamingRequest {
            clip: &real_clip,
            lineage: &real_ctx,
        },
        crate::naming::NamingRequest {
            clip: &new_clip,
            lineage: &new_ctx,
        },
    ];
    let names = crate::naming::render_clip_names(
        &requests,
        &crate::naming::NamingConfig::default(),
        &colliding,
    );
    let desired_of = |clip: &Clip, ctx: &LineageContext, name: &crate::naming::RenderedName| {
        crate::reconcile::Desired {
            clip: clip.clone(),
            lineage: ctx.clone(),
            path: format!(
                "{}.flac",
                crate::desired::rel_to_string(&name.relative_path)
            ),
            format: crate::AudioFormat::Flac,
            meta_hash: String::new(),
            art_hash: String::new(),
            modes: vec![crate::vocab::SourceMode::Mirror],
            trashed: false,
            private: false,
            artifacts: Vec::new(),
            stems: None,
        }
    };
    let desired = vec![
        desired_of(&real_clip, &real_ctx, &names[0]),
        desired_of(&new_clip, &new_ctx, &names[1]),
    ];
    let albums = crate::reconcile::album_desired(
        &desired,
        false,
        false,
        crate::vocab::WebpEncodeSettings::default(),
    );
    let jpg_paths: Vec<String> = albums
        .iter()
        .filter_map(|a| a.folder_jpg.as_ref().map(|art| art.path.clone()))
        .collect();
    assert_eq!(jpg_paths.len(), 2, "both albums have a folder.jpg");
    assert_ne!(
        jpg_paths[0], jpg_paths[1],
        "an uncached override must not collapse two albums onto one path"
    );
}

#[test]
fn override_on_gap_filled_root_applies_to_children_and_collides() {
    // Round-3 regression. A gap-filled/archived root is a cache VALUE for its
    // children but never a cache KEY (see
    // resolve_roots_returns_gap_filled_ancestors_for_archival). An override
    // keyed by such a root must still apply to its children AND participate
    // in collision detection, so gating override-application on the cache
    // VALUE set (not the key set) is essential.
    let child = Clip {
        id: "childclip".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "gaproot".into(),
        edited_clip_id: "gaproot".into(),
        ..Default::default()
    };
    let other_root = Clip {
        id: "otherroot".into(),
        title: "Preferred".into(),
        clip_type: "gen".into(),
        ..Default::default()
    };
    let gap_ancestor = Clip {
        id: "gaproot".into(),
        title: "Working Title".into(),
        clip_type: "gen".into(),
        ..Default::default()
    };
    let mut roots = HashMap::new();
    roots.insert(
        "childclip".to_owned(),
        RootInfo {
            root_id: "gaproot".into(),
            root_title: "Working Title".into(),
            status: ResolveStatus::Resolved,
        },
    );
    roots.insert(
        "otherroot".to_owned(),
        RootInfo {
            root_id: "otherroot".into(),
            root_title: "Preferred".into(),
            status: ResolveStatus::Resolved,
        },
    );
    let mut store = LineageStore::new();
    store.update(
        &[child.clone(), other_root],
        &Resolution {
            roots,
            gap_filled: vec![gap_ancestor],
            bridges: Vec::new(),
        },
        "now",
    );
    // "gaproot" is a node and a cache value, but NOT a cache key.
    assert!(store.node("gaproot").is_some());
    assert!(!store.resolution_cache.contains_key("gaproot"));

    store.set_album_overrides(
        [("gaproot".to_owned(), "Preferred".to_owned())]
            .into_iter()
            .collect(),
    );

    // The override on the gap-filled root reaches its child (would be
    // ignored under a cache-KEY gate).
    assert_eq!(store.context_for(&child).album(&child.title), "Preferred");
    assert_eq!(store.album_for_id("childclip"), "Preferred");

    // And it participates in collision detection: two distinct roots now
    // resolve to "Preferred", so it is flagged.
    assert!(store.colliding_root_titles().contains("Preferred"));
}

#[test]
fn eligible_root_set_is_exactly_the_cache_value_domain() {
    // Tie-together guard: the set effective_root_title gates overrides on is
    // literally the set colliding_root_titles groups over (both read
    // eligible_root_ids), and refresh_eligible_roots computes it as the
    // non-empty root_id VALUES of the cache. If these drift, an override
    // could apply where a collision is invisible (or vice versa).
    let child = Clip {
        id: "childclip".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "gaproot".into(),
        edited_clip_id: "gaproot".into(),
        ..Default::default()
    };
    let mut roots = HashMap::new();
    roots.insert(
        "childclip".to_owned(),
        RootInfo {
            root_id: "gaproot".into(),
            root_title: "Working Title".into(),
            status: ResolveStatus::Resolved,
        },
    );
    let mut store = LineageStore::new();
    store.update(
        std::slice::from_ref(&child),
        &Resolution {
            roots,
            gap_filled: vec![Clip {
                id: "gaproot".into(),
                title: "Working Title".into(),
                clip_type: "gen".into(),
                ..Default::default()
            }],
            bridges: Vec::new(),
        },
        "now",
    );

    let expected: std::collections::HashSet<String> = store
        .resolution_cache
        .values()
        .map(|entry| entry.root_id.clone())
        .filter(|root_id| !root_id.is_empty())
        .collect();
    assert_eq!(*store.eligible_root_ids_for_test(), expected);
    // The gap-filled root is in the domain (a value), not because it is a key.
    assert!(store.eligible_root_ids_for_test().contains("gaproot"));
    assert!(!store.resolution_cache.contains_key("gaproot"));
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
