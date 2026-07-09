use super::*;

#[test]
fn build_desired_appends_extension_and_mode() {
    let a = clip("id-a", "Song A", "alice");
    let clips = [&a];
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    assert_eq!(desired.len(), 1);
    assert!(
        desired[0].path.ends_with(".flac"),
        "path: {}",
        desired[0].path
    );
    assert_eq!(desired[0].format, AudioFormat::Flac);
    assert_eq!(desired[0].modes, vec![SourceMode::Mirror]);
    assert!(!desired[0].trashed);
    assert!(!desired[0].private);
    let lineage = LineageContext::own_root(&a);
    assert_eq!(desired[0].meta_hash, crate::hash::meta_hash(&a, &lineage));
    assert_eq!(desired[0].art_hash, art_hash(&a));
    assert_eq!(desired[0].lineage, lineage);
}

#[test]
fn build_desired_carries_the_trashed_flag_from_the_clip() {
    let mut gone = clip("id-gone", "Removed", "alice");
    gone.is_trashed = true;
    let live = clip("id-live", "Kept", "alice");
    let clips = [&gone, &live];
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    assert!(desired[0].trashed, "a trashed clip is marked trashed");
    assert!(!desired[1].trashed, "a live clip is not");
}

#[test]
fn build_desired_uses_supplied_lineage_context() {
    use crate::lineage::ResolveStatus;

    let a = clip("child-1", "Remix", "alice");
    let clips = [&a];
    let lineage = LineageContext {
        root_id: "root-1".to_owned(),
        root_title: "Original".to_owned(),
        root_date: String::new(),
        parent_id: "root-1".to_owned(),
        edge_type: None,
        status: ResolveStatus::Resolved,
        track: 0,
        track_total: 0,
    };
    let contexts: HashMap<String, LineageContext> =
        [(a.id.clone(), lineage.clone())].into_iter().collect();
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &contexts,
        &no_collisions(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    assert!(
        desired[0].path.contains("/Original/"),
        "path: {}",
        desired[0].path
    );
    assert_eq!(desired[0].lineage, lineage);
    assert_eq!(desired[0].meta_hash, crate::hash::meta_hash(&a, &lineage));
}

#[test]
fn lineage_is_stable_when_a_later_resolution_fails() {
    use crate::graph::LineageStore;
    use crate::lineage::{Resolution, ResolveStatus, RootInfo};

    let root = Clip {
        id: "root-break".into(),
        title: "Break Through".into(),
        clip_type: "gen".into(),
        handle: "alice".into(),
        display_name: "alice".into(),
        ..Default::default()
    };
    let child = Clip {
        id: "child-remix".into(),
        title: "Remix".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "root-break".into(),
        edited_clip_id: "root-break".into(),
        handle: "alice".into(),
        display_name: "alice".into(),
        ..Default::default()
    };
    let clips = [&root, &child];

    let contexts_of = |store: &LineageStore| -> HashMap<String, LineageContext> {
        clips
            .iter()
            .map(|c| (c.id.clone(), store.context_for(c)))
            .collect()
    };

    let mut roots = HashMap::new();
    for id in ["root-break", "child-remix"] {
        roots.insert(
            id.to_owned(),
            RootInfo {
                root_id: "root-break".into(),
                root_title: "Break Through".into(),
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
    store.update(&[root.clone(), child.clone()], &resolution, "t1");

    let cycle1 = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &contexts_of(&store),
        &store.colliding_root_titles(),
        &store.colliding_clip_ids(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    let child1 = cycle1.iter().find(|d| d.clip.id == "child-remix").unwrap();
    assert!(
        child1.path.contains("/Break Through/"),
        "the remix should folder under its root album, got {}",
        child1.path
    );

    let cycle2 = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &contexts_of(&store),
        &store.colliding_root_titles(),
        &store.colliding_clip_ids(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    for (a, b) in cycle1.iter().zip(&cycle2) {
        assert_eq!(a.path, b.path, "album path drifted for {}", a.clip.id);
        assert_eq!(
            a.meta_hash, b.meta_hash,
            "meta_hash drifted for {}",
            a.clip.id
        );
    }

    let own = LineageContext::own_root(&child);
    assert_ne!(
        crate::hash::meta_hash(&child, &own),
        child1.meta_hash,
        "own-root fallback must differ from the store-driven hash"
    );
}

#[test]
fn build_desired_disambiguates_collisions() {
    let a = clip("id-a", "Same", "alice");
    let b = clip("id-b", "Same", "alice");
    let clips = [&a, &b];
    let desired = build_desired(
        &clips,
        AudioFormat::Mp3,
        &modes_for(&clips, SourceMode::Copy),
        &no_contexts(),
        &no_collisions(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    assert_ne!(desired[0].path, desired[1].path);
    assert!(desired.iter().all(|d| d.path.ends_with(".mp3")));
    assert!(desired.iter().all(|d| d.modes == vec![SourceMode::Copy]));
}

#[test]
fn build_desired_uses_forward_slashes() {
    let a = clip("id-a", "Song A", "alice");
    let clips = [&a];
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    assert!(!desired[0].path.contains('\\'));
    assert!(desired[0].path.contains('/'));
}

#[test]
fn rel_to_string_normalises_os_separators_to_forward_slash() {
    // rel_to_string is the single source of truth for every stored path, so
    // it must render '/' on every OS. A PathBuf assembled per-component (as
    // render_clip_name does) renders with the platform separator internally,
    // yet rel_to_string flattens it to '/', so a manifest written on one OS
    // reconciles byte-identically on another (#236).
    let mut path = PathBuf::new();
    for part in ["alice", "Album Name", "alice-Song [clipaaaa]"] {
        path.push(part);
    }
    let rendered = rel_to_string(&path);
    assert_eq!(rendered, "alice/Album Name/alice-Song [clipaaaa]");
    assert!(
        !rendered.contains('\\'),
        "rendered a platform separator: {rendered}"
    );
}

#[test]
fn cover_album_and_stem_paths_use_forward_slashes() {
    // The audio path is pinned by build_desired_uses_forward_slashes; extend
    // that guard to every OTHER persisted path kind — the per-clip cover.jpg,
    // the album-scoped folder.jpg, and a stem — so none can leak a platform
    // separator into the manifest on Windows (#236).
    let a = art_clip("clipaaaa-1234");
    let clips = [&a];
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    let d = &desired[0];

    let cover = d
        .artifacts
        .iter()
        .find(|art| art.kind == ArtifactKind::CoverJpg)
        .expect("an art-bearing clip yields a cover.jpg");
    assert!(
        cover.path.contains('/') && !cover.path.contains('\\'),
        "cover: {}",
        cover.path
    );

    let albums =
        crate::desired::album_desired(&desired, false, false, WebpEncodeSettings::default());
    let folder_jpg = albums[0]
        .folder_jpg
        .as_ref()
        .expect("the album has a folder.jpg");
    assert!(
        folder_jpg.path.contains('/') && !folder_jpg.path.contains('\\'),
        "folder.jpg: {}",
        folder_jpg.path
    );

    let base = d.path.strip_suffix(".flac").expect("a .flac audio path");
    let stems = clip_stems(
        base,
        &[Stem {
            id: "stemvocal-9f8e".to_owned(),
            label: "Vocals".to_owned(),
            url: "https://cdn.suno.ai/stem.mp3".to_owned(),
        }],
        StemFormat::Mp3,
        CharacterSet::Unicode,
    );
    assert!(
        stems[0].path.contains('/') && !stems[0].path.contains('\\'),
        "stem: {}",
        stems[0].path
    );
}

#[test]
fn build_desired_one_pass_disambiguates_and_stamps_modes() {
    let a = clip("lib-1", "Song", "alice");
    let b = clip("pl-1", "Song", "alice");
    let clips = [&a, &b];
    let mut modes = HashMap::new();
    modes.insert("lib-1".to_owned(), vec![SourceMode::Copy]);
    modes.insert(
        "pl-1".to_owned(),
        vec![SourceMode::Mirror, SourceMode::Copy],
    );
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes,
        &no_contexts(),
        &no_collisions(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    assert_eq!(desired.len(), 2);
    assert_ne!(desired[0].path, desired[1].path);
    assert_eq!(desired[1].modes, vec![SourceMode::Mirror, SourceMode::Copy]);
}

#[test]
fn build_desired_respects_custom_naming_config() {
    use crate::naming::CharacterSet;

    let a = clip("abcdefgh-1234", "Song A", "alice");
    let clips = [&a];
    let custom = NamingConfig {
        template: "{title}/{id8}".to_owned(),
        character_set: CharacterSet::Ascii,
        ..NamingConfig::default()
    };
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &HashMap::from([("abcdefgh-1234".to_owned(), vec![SourceMode::Mirror])]),
        &no_contexts(),
        &no_collisions(),
        &no_collisions(),
        ArtifactToggles::default(),
        &custom,
    );
    assert!(
        desired[0].path.starts_with("Song A/"),
        "path: {}",
        desired[0].path
    );
    assert!(desired[0].path.contains(&a.id[..8]));
}

#[test]
fn id8_twin_path_is_stable_when_a_twin_leaves_the_selection() {
    use crate::graph::LineageStore;
    use crate::lineage::{Resolution, ResolveStatus, RootInfo};

    // #356 at the desired-state layer, mirroring the album idempotence test:
    // two id8-twins recorded in the store keep the kept clip's path fixed when
    // its twin drops out of the selection, because `colliding_clip_ids` is
    // store-derived and batch-independent.
    let a = clip("abcd1234-a", "Untitled", "alice");
    let b = clip("abcd1234-b", "Untitled", "alice");

    let mut roots = HashMap::new();
    for id in ["abcd1234-a", "abcd1234-b"] {
        roots.insert(
            id.to_owned(),
            RootInfo {
                root_id: id.to_owned(),
                root_title: "Untitled".into(),
                status: ResolveStatus::Resolved,
            },
        );
    }
    let mut store = LineageStore::new();
    store.update(
        &[a.clone(), b.clone()],
        &Resolution {
            roots,
            gap_filled: Vec::new(),
            bridges: Vec::new(),
        },
        "t1",
    );
    let colliding_ids = store.colliding_clip_ids();
    assert!(
        colliding_ids.contains("abcd1234-a") && colliding_ids.contains("abcd1234-b"),
        "the store should flag both twins"
    );

    let both = [&a, &b];
    let with_twin = build_desired(
        &both,
        AudioFormat::Flac,
        &modes_for(&both, SourceMode::Mirror),
        &no_contexts(),
        &store.colliding_root_titles(),
        &colliding_ids,
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    let a_with_twin = with_twin
        .iter()
        .find(|d| d.clip.id == "abcd1234-a")
        .unwrap();
    assert!(
        a_with_twin.path.contains("[abcd1234-a]"),
        "the twin should carry the whole-library suffix: {}",
        a_with_twin.path
    );

    let alone = [&a];
    let without_twin = build_desired(
        &alone,
        AudioFormat::Flac,
        &modes_for(&alone, SourceMode::Mirror),
        &no_contexts(),
        &store.colliding_root_titles(),
        &colliding_ids,
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    let a_alone = without_twin
        .iter()
        .find(|d| d.clip.id == "abcd1234-a")
        .unwrap();

    assert_eq!(
        a_with_twin.path, a_alone.path,
        "the kept clip's path drifted when its twin left the selection"
    );
}
