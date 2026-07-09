use super::*;

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
        track: 0,
        track_total: 0,
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
            embedded_lyrics_hash: String::new(),
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

    let albums = crate::desired::album_desired(
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
            embedded_lyrics_hash: String::new(),
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
    let albums = crate::desired::album_desired(
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
