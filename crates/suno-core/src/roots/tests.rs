//! The lineage-root resolver test suite: drives resolve_roots through the
//! scripted Http port, asserting seed walking, parent-endpoint fallback,
//! and the resolution status of each root.

use super::*;
use crate::auth::ClerkAuth;
use crate::testutil::{RecordingClock, Reply, ScriptedHttp};

// A clean six-clip chain modelled on the real `chain1` grounding data:
// upsample -> cover -> upsample -> cover -> edit -> root. For every hop the
// op pointer and `edited_clip_id` agree, as they do in the live shape.
fn chain1_clips() -> Vec<Clip> {
    vec![
        Clip {
            id: "40068b49".into(),
            title: "Zac and the Sea Eagles (Lullaby Version)".into(),
            clip_type: "upsample".into(),
            task: "upsample".into(),
            is_remix: true,
            upsample_clip_id: "52962dae".into(),
            edited_clip_id: "52962dae".into(),
            ..Default::default()
        },
        Clip {
            id: "52962dae".into(),
            title: "Zac and the Sea Eagles (Edit) (Remastered)".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            is_remix: true,
            cover_clip_id: "536e1b92".into(),
            edited_clip_id: "536e1b92".into(),
            ..Default::default()
        },
        Clip {
            id: "536e1b92".into(),
            title: "Zac and the Sea Eagles (Edit) (Remastered)".into(),
            clip_type: "upsample".into(),
            task: "upsample".into(),
            is_remix: true,
            upsample_clip_id: "b9f27ee1".into(),
            edited_clip_id: "b9f27ee1".into(),
            ..Default::default()
        },
        Clip {
            id: "b9f27ee1".into(),
            title: "Zac and the Sea Eagles (Edit)".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            is_remix: true,
            cover_clip_id: "c1997d52".into(),
            edited_clip_id: "c1997d52".into(),
            ..Default::default()
        },
        Clip {
            id: "c1997d52".into(),
            title: "Zac and the Sea Eagles (Rework)".into(),
            clip_type: "edit_v3_export".into(),
            edited_clip_id: "dfb59a04".into(),
            ..Default::default()
        },
        Clip {
            id: "dfb59a04".into(),
            title: "Zac and the Sea Eagles".into(),
            clip_type: "gen".into(),
            ..Default::default()
        },
    ]
}

fn authed_client(http: &ScriptedHttp) -> SunoClient<RecordingClock> {
    let auth = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth.authenticate(http)).unwrap();
    SunoClient::new(auth, RecordingClock::new())
}

fn clip_root(id: &str, handle: &str) -> crate::model::ClipRoot {
    crate::model::ClipRoot {
        id: id.to_owned(),
        handle: handle.to_owned(),
        ..Default::default()
    }
}

#[test]
fn resolve_roots_walks_a_connected_chain_with_no_http() {
    let http = ScriptedHttp::new();
    let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
    let clips = chain1_clips();

    let roots = pollster::block_on(resolve_roots(
        &clips,
        &HashMap::new(),
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap()
    .roots;

    assert!(
        http.calls().is_empty(),
        "a fully-connected chain must never touch the network"
    );
    assert_eq!(roots.len(), clips.len());
    for clip in &clips {
        let info = &roots[&clip.id];
        assert_eq!(info.status, ResolveStatus::Resolved);
        assert_eq!(info.root_id, "dfb59a04");
        assert_eq!(info.root_title, "Zac and the Sea Eagles");
    }
}

#[test]
fn resolve_roots_gap_fills_a_missing_ancestor_by_id() {
    let cover = Clip {
        id: "child".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "root".into(),
        edited_clip_id: "root".into(),
        ..Default::default()
    };
    let root_clip = serde_json::json!({
        "id": "root", "title": "Original", "status": "complete",
        "metadata": {"type": "gen"}
    })
    .to_string();
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/clip/root", Reply::json(&root_clip));
    let client = authed_client(&http);

    let roots = pollster::block_on(resolve_roots(
        &[cover],
        &HashMap::new(),
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap()
    .roots;

    let info = &roots["child"];
    assert_eq!(info.status, ResolveStatus::Resolved);
    assert_eq!(info.root_id, "root");
    assert_eq!(info.root_title, "Original");
    assert_eq!(http.count("/api/clip/root"), 1);
    assert_eq!(
        http.count("/api/clips/parent"),
        0,
        "the parent endpoint must not be used when the per-id fetch succeeds"
    );
}

#[test]
fn resolve_roots_hops_through_a_purged_ancestor_via_the_archive() {
    // A cover whose parent (an intermediate remix) is absent from this run's
    // clips AND unfetchable from the network (Suno purged it), but whose
    // parent link was persisted on an earlier run. The archived edge lets
    // the walk hop through the purged intermediate to the true root, with no
    // network call, instead of self-rooting into a duplicate album.
    let child = Clip {
        id: "child".into(),
        title: "Neue Deutsche Harte".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "mid".into(),
        edited_clip_id: "mid".into(),
        ..Default::default()
    };
    let root = Clip {
        id: "root".into(),
        title: "Original".into(),
        clip_type: "gen".into(),
        ..Default::default()
    };
    // "mid" is neither a live clip nor routed on the network double.
    let archived: HashMap<String, String> = [("mid".to_owned(), "root".to_owned())]
        .into_iter()
        .collect();
    let http = ScriptedHttp::new().with_auth();
    let client = authed_client(&http);

    let resolution = pollster::block_on(resolve_roots(
        &[child, root],
        &archived,
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap();

    let info = &resolution.roots["child"];
    assert_eq!(info.status, ResolveStatus::Resolved);
    assert_eq!(
        info.root_id, "root",
        "hopped through the purged intermediate"
    );
    assert_eq!(info.root_title, "Original");
    assert_eq!(
        http.count("/api/clip/mid"),
        0,
        "the purged intermediate is never fetched: the archived edge bridges it"
    );
    assert!(
        resolution.gap_filled.is_empty(),
        "an archived hop must not add a download candidate"
    );
}

#[test]
fn resolve_roots_prefers_a_live_pointer_over_a_stale_archived_edge() {
    // When a clip is present live, its own (fresh) pointer wins; a stale
    // archived edge for that same clip is ignored (index before archive).
    let child = Clip {
        id: "child".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "live_root".into(),
        edited_clip_id: "live_root".into(),
        ..Default::default()
    };
    let live_root = Clip {
        id: "live_root".into(),
        title: "Live Root".into(),
        clip_type: "gen".into(),
        ..Default::default()
    };
    let archived: HashMap<String, String> = [("child".to_owned(), "stale_root".to_owned())]
        .into_iter()
        .collect();
    let http = ScriptedHttp::new().with_auth();
    let client = authed_client(&http);

    let info = pollster::block_on(resolve_roots(
        &[child, live_root],
        &archived,
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap()
    .roots["child"]
        .clone();

    assert_eq!(
        info.root_id, "live_root",
        "the live pointer wins over a stale archived edge"
    );
    assert_eq!(info.status, ResolveStatus::Resolved);
}

#[test]
fn resolve_roots_terminates_on_a_cycle_through_archived_edges() {
    // Archived edges form a cycle a -> b -> a; the walk must terminate via
    // the visited guard, never loop.
    let child = Clip {
        id: "child".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "a".into(),
        edited_clip_id: "a".into(),
        ..Default::default()
    };
    let archived: HashMap<String, String> = [
        ("a".to_owned(), "b".to_owned()),
        ("b".to_owned(), "a".to_owned()),
    ]
    .into_iter()
    .collect();
    let http = ScriptedHttp::new().with_auth();
    let client = authed_client(&http);

    let info = pollster::block_on(resolve_roots(
        &[child],
        &archived,
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap()
    .roots["child"]
        .clone();

    assert_eq!(
        info.status,
        ResolveStatus::Cycle,
        "an archived cycle terminates as a cycle, not an infinite loop"
    );
}

#[test]
fn resolve_roots_respects_the_hop_cap_through_archived_edges() {
    // A long archived chain past the hop cap terminates as unresolved,
    // without any network fetch.
    let child = Clip {
        id: "child".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "a".into(),
        edited_clip_id: "a".into(),
        ..Default::default()
    };
    let archived: HashMap<String, String> = [("a", "b"), ("b", "c"), ("c", "d"), ("d", "e")]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let opts = ResolveOpts {
        max_gap_fills: 0,
        hop_cap: 2,
        concurrency: 4,
    };
    let http = ScriptedHttp::new().with_auth();
    let client = authed_client(&http);

    let info = pollster::block_on(resolve_roots(&[child], &archived, &client, &http, opts))
        .unwrap()
        .roots["child"]
        .clone();

    assert_eq!(
        info.status,
        ResolveStatus::Unresolved,
        "a chain past the hop cap terminates as unresolved"
    );
    assert_eq!(
        http.count("/api/clip"),
        0,
        "archived hops need no clip fetch"
    );
}

#[test]
fn resolve_roots_without_archive_self_roots_a_purged_intermediate() {
    // The same clip WITHOUT the archived edge: the intermediate is missing
    // and unfetchable, so resolution stalls at it (external) rather than
    // reaching the true root. This is the pre-fix behaviour the archive
    // prevents.
    let child = Clip {
        id: "child".into(),
        title: "Neue Deutsche Harte".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "mid".into(),
        edited_clip_id: "mid".into(),
        ..Default::default()
    };
    let root = Clip {
        id: "root".into(),
        title: "Original".into(),
        clip_type: "gen".into(),
        ..Default::default()
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/clip/mid", Reply::status(404))
        .route("/api/clips/parent", Reply::status(404));
    let client = authed_client(&http);

    let info = pollster::block_on(resolve_roots(
        &[child, root],
        &HashMap::new(),
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap()
    .roots["child"]
        .clone();

    assert_ne!(
        info.root_id, "root",
        "without the archive, resolution cannot reach the true root"
    );
    assert_ne!(
        info.status,
        ResolveStatus::Resolved,
        "the purged intermediate cannot be cleanly resolved without the archive"
    );
}

#[test]
fn resolve_roots_returns_gap_filled_ancestors_for_archival() {
    // The fetched (often trashed) ancestor is handed back so a later phase
    // can archive it before Suno's purge (HARDENING H4). It resolves the
    // child's root yet stays out of the roots (download) set.
    let cover = Clip {
        id: "child".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "root".into(),
        edited_clip_id: "root".into(),
        ..Default::default()
    };
    let root_clip = serde_json::json!({
        "id": "root", "title": "Trashed Original", "status": "complete",
        "metadata": {"type": "gen"}
    })
    .to_string();
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/clip/root", Reply::json(&root_clip));
    let client = authed_client(&http);

    let resolution = pollster::block_on(resolve_roots(
        &[cover],
        &HashMap::new(),
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap();

    assert_eq!(resolution.gap_filled.len(), 1);
    assert_eq!(resolution.gap_filled[0].id, "root");
    assert_eq!(resolution.gap_filled[0].title, "Trashed Original");
    assert_eq!(resolution.roots["child"].root_id, "root");
    assert!(
        !resolution.roots.contains_key("root"),
        "gap-filled ancestors must never enter the roots set"
    );
}

#[test]
fn resolve_roots_falls_back_to_the_parent_endpoint() {
    let cover = Clip {
        id: "child".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "missing".into(),
        edited_clip_id: "missing".into(),
        ..Default::default()
    };
    // The per-id fetch of `missing` 404s; the parent endpoint yields its
    // parent (the root), which the walk then bridges over `missing` to.
    let parent_body = serde_json::json!({
        "id": "root", "title": "Original", "status": "complete",
        "metadata": {"type": "gen"}
    })
    .to_string();
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/clip/missing", Reply::status(404))
        .route("/api/clips/parent", Reply::json(&parent_body));
    let client = authed_client(&http);

    let roots = pollster::block_on(resolve_roots(
        &[cover],
        &HashMap::new(),
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap()
    .roots;

    let info = &roots["child"];
    assert_eq!(info.status, ResolveStatus::Resolved);
    assert_eq!(info.root_id, "root");
    assert_eq!(info.root_title, "Original");
    assert!(
        http.count("/api/clips/parent?clip_id=missing") >= 1,
        "the missing ancestor must be resolved via the parent endpoint"
    );
}

#[test]
fn resolve_roots_detects_a_cycle_without_looping() {
    let a = Clip {
        id: "a".into(),
        title: "A".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "b".into(),
        edited_clip_id: "b".into(),
        ..Default::default()
    };
    let b = Clip {
        id: "b".into(),
        title: "B".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "a".into(),
        edited_clip_id: "a".into(),
        ..Default::default()
    };
    let http = ScriptedHttp::new();
    let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());

    let roots = pollster::block_on(resolve_roots(
        &[a, b],
        &HashMap::new(),
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap()
    .roots;

    assert_eq!(roots["a"].status, ResolveStatus::Cycle);
    assert_eq!(roots["b"].status, ResolveStatus::Cycle);
    assert!(http.calls().is_empty());
}

#[test]
fn resolve_roots_marks_external_when_the_budget_is_exhausted() {
    // child -> m1 (missing) -> m2 (missing) -> ...; only one gap-fill allowed.
    let child = Clip {
        id: "child".into(),
        title: "Child".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "m1".into(),
        edited_clip_id: "m1".into(),
        ..Default::default()
    };
    let m1_clip = serde_json::json!({
        "id": "m1", "title": "Middle", "status": "complete",
        "metadata": {"type": "gen", "task": "cover", "cover_clip_id": "m2", "edited_clip_id": "m2"}
    })
    .to_string();
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/clip/m1", Reply::json(&m1_clip));
    let client = authed_client(&http);
    let opts = ResolveOpts {
        max_gap_fills: 1,
        hop_cap: 64,
        concurrency: 4,
    };

    let roots = pollster::block_on(resolve_roots(
        &[child],
        &HashMap::new(),
        &client,
        &http,
        opts,
    ))
    .unwrap()
    .roots;

    let info = &roots["child"];
    assert_eq!(info.status, ResolveStatus::External);
    assert_eq!(
        info.root_id, "m2",
        "resolution stops at the first ancestor it could not fetch"
    );
    assert_eq!(http.count("/api/clip/m1"), 1);
    assert_eq!(
        http.count("/api/clip/m2"),
        0,
        "the gap-fill budget must not be exceeded"
    );
}

#[test]
fn resolve_roots_external_root_endpoint_stops_the_walk() {
    // The parent endpoint reporting no parent means an external root: the
    // ancestor exists on Suno but is outside the caller's library.
    let cover = Clip {
        id: "child".into(),
        title: "Cover".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "outside".into(),
        edited_clip_id: "outside".into(),
        ..Default::default()
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/clip/outside", Reply::status(404))
        .route("/api/clips/parent", Reply::status(404));
    let client = authed_client(&http);

    let roots = pollster::block_on(resolve_roots(
        &[cover],
        &HashMap::new(),
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap()
    .roots;

    let info = &roots["child"];
    assert_eq!(info.status, ResolveStatus::External);
    assert_eq!(info.root_id, "outside");
}

#[test]
fn resolve_roots_seeds_a_same_owner_clip_root_but_not_a_foreign_one() {
    // A clip whose structural parent is missing triggers gap-fill. Its
    // same-owner clip_root is seeded into the same batch (an extra root
    // candidate), while its foreign-owned clip_root is NEVER fetched. The
    // structural walk alone still decides the root.
    let child = Clip {
        id: "child".into(),
        title: "Remix".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "struct-parent".into(),
        edited_clip_id: "struct-parent".into(),
        handle: "me".into(),
        clip_attribution_type: "remix".into(),
        clip_roots: vec![
            clip_root("own-root", "me"),
            clip_root("foreign-root", "stranger"),
        ],
        ..Default::default()
    };
    let struct_parent = serde_json::json!({
        "id": "struct-parent", "title": "Structural Root", "status": "complete",
        "metadata": {"type": "gen"}
    })
    .to_string();
    let own_root = serde_json::json!({
        "id": "own-root", "title": "Attribution Root", "status": "complete",
        "metadata": {"type": "gen"}
    })
    .to_string();
    // The batch returns both the structural parent and the seeded same-owner
    // root in one request; the per-id routes remain only as the fallback.
    let batch = format!(r#"{{"clips":[{struct_parent},{own_root}]}}"#);
    let http = ScriptedHttp::new()
        .with_auth()
        .route("get_songs_by_ids", Reply::json(&batch))
        .route("/api/clip/struct-parent", Reply::json(&struct_parent))
        .route("/api/clip/own-root", Reply::json(&own_root));
    let client = authed_client(&http);

    let resolution = pollster::block_on(resolve_roots(
        &[child],
        &HashMap::new(),
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap();

    // The structural walk (not clip_roots) decides the root.
    let info = &resolution.roots["child"];
    assert_eq!(info.status, ResolveStatus::Resolved);
    assert_eq!(info.root_id, "struct-parent");

    assert_eq!(
        http.count("own-root"),
        1,
        "the same-owner clip_root is seeded and fetched exactly once"
    );
    assert_eq!(
        http.count("foreign-root"),
        0,
        "a foreign-owned clip_root is NEVER seeded or fetched"
    );
}

#[test]
fn resolve_roots_clip_root_seed_is_best_effort_never_bridges_or_retries() {
    // A same-owner clip_root that the batch never returns (trashed/404) is
    // simply dropped: it is never bridged, never external, never re-seeded,
    // and the structural resolution is unaffected.
    let child = Clip {
        id: "child".into(),
        title: "Remix".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "mid".into(),
        edited_clip_id: "mid".into(),
        handle: "me".into(),
        clip_attribution_type: "remix".into(),
        clip_roots: vec![clip_root("gone-root", "me")],
        ..Default::default()
    };
    // "mid" resolves to "root" over two gap-fill rounds, so the seed would be
    // re-scanned on the second round if the attempted-set did not suppress it.
    let mid = serde_json::json!({
        "id": "mid", "title": "Mid", "status": "complete",
        "metadata": {"type": "gen", "task": "cover", "cover_clip_id": "root"}
    })
    .to_string();
    let root = serde_json::json!({
        "id": "root", "title": "Root", "status": "complete",
        "metadata": {"type": "gen"}
    })
    .to_string();
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/api/clip/mid", Reply::json(&mid))
        .route("/api/clip/root", Reply::json(&root))
        .route("/api/clip/gone-root", Reply::status(404));
    let client = authed_client(&http);

    let resolution = pollster::block_on(resolve_roots(
        &[child],
        &HashMap::new(),
        &client,
        &http,
        ResolveOpts::default(),
    ))
    .unwrap();

    let info = &resolution.roots["child"];
    assert_eq!(info.status, ResolveStatus::Resolved);
    assert_eq!(
        info.root_id, "root",
        "the structural chain resolves normally"
    );
    assert!(
        resolution.bridges.is_empty(),
        "a dropped seed must never become a bridge"
    );
    assert!(
        !resolution.gap_filled.iter().any(|c| c.id == "gone-root"),
        "a seed the batch omits is never added"
    );
    assert_eq!(
        http.count("/api/clip/gone-root"),
        1,
        "the seed is attempted once, never retried across rounds"
    );
    assert_eq!(
        http.count("/api/clips/parent"),
        0,
        "a seed never falls through to the parent endpoint"
    );
}
