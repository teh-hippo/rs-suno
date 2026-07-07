//! The desired-state builder test suite: drives `build_desired`,
//! `clip_stems`, `clip_artifacts`, and `build_playlist_desired` over crafted
//! clips and asserts the artifact set, naming, and playlist membership.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::PathBuf;

use super::*;
use crate::hash::{art_hash, art_url_hash, content_hash, synced_lrc_source_hash};
use crate::lineage::LineageContext;
use crate::naming::NamingConfig;
use crate::vocab::{ArtifactKind, AudioFormat, SourceMode};

fn clip(id: &str, title: &str, handle: &str) -> Clip {
    Clip {
        id: id.to_owned(),
        title: title.to_owned(),
        handle: handle.to_owned(),
        display_name: handle.to_owned(),
        ..Default::default()
    }
}

fn no_contexts() -> HashMap<String, LineageContext> {
    HashMap::new()
}

fn no_collisions() -> BTreeSet<String> {
    BTreeSet::new()
}

fn modes_for(clips: &[&Clip], mode: SourceMode) -> HashMap<String, Vec<SourceMode>> {
    clips.iter().map(|c| (c.id.clone(), vec![mode])).collect()
}

fn art_clip(id: &str) -> Clip {
    Clip {
        image_large_url: format!("https://art.suno.ai/{id}/large.jpg"),
        ..clip(id, "Song", "alice")
    }
}

fn path_of<'a>(desired: &'a [Desired], id: &str) -> &'a str {
    desired
        .iter()
        .find(|d| d.clip.id == id)
        .map(|d| d.path.as_str())
        .expect("clip in desired set")
}

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
    };
    let contexts: HashMap<String, LineageContext> =
        [(a.id.clone(), lineage.clone())].into_iter().collect();
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &contexts,
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
        crate::reconcile::album_desired(&desired, false, false, WebpEncodeSettings::default());
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
fn build_desired_emits_cover_jpg_next_to_audio() {
    let a = art_clip("id-a");
    let clips = [&a];
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    let base = desired[0].path.strip_suffix(".flac").unwrap();
    assert_eq!(desired[0].artifacts.len(), 1);
    let jpg = &desired[0].artifacts[0];
    assert_eq!(jpg.kind, ArtifactKind::CoverJpg);
    assert_eq!(jpg.path, format!("{base}.jpg"));
    assert_eq!(jpg.source_url, a.selected_image_url().unwrap());
    assert_eq!(jpg.hash, art_hash(&a));
}

#[test]
fn build_desired_omits_cover_jpg_when_art_is_empty() {
    let a = clip("id-a", "Song", "alice");
    assert!(a.selected_image_url().is_none());
    let clips = [&a];
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            animated_covers: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    assert!(desired[0].artifacts.is_empty());
}

#[test]
fn animated_covers_embed_via_art_hash_not_a_webp_sidecar() {
    let with_video = Clip {
        video_cover_url: "https://cdn.suno.ai/id-a/video.mp4".to_owned(),
        ..art_clip("id-a")
    };
    let clips = [&with_video];

    // Feature off: only the static CoverJpg sidecar; art_hash is the static hash.
    let off = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    assert_eq!(off[0].artifacts.len(), 1);
    assert_eq!(off[0].artifacts[0].kind, ArtifactKind::CoverJpg);
    assert_eq!(off[0].art_hash, crate::hash::art_hash(&with_video));

    // Feature on: still NO `CoverWebp` sidecar and the `.jpg` stays static,
    // but the audio's art_hash now reflects the animated-WebP embed intent,
    // so it drifts from the static hash and triggers a retag.
    let on = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            animated_covers: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    assert!(
        on[0]
            .artifacts
            .iter()
            .all(|art| art.kind != ArtifactKind::CoverWebp)
    );
    assert_eq!(
        on[0]
            .artifacts
            .iter()
            .filter(|a| a.kind == ArtifactKind::CoverJpg)
            .count(),
        1
    );
    assert_ne!(
        on[0].art_hash, off[0].art_hash,
        "embed intent drifts the art hash"
    );
    assert_eq!(
        on[0].art_hash,
        crate::hash::embedded_art_hash(&with_video, true, &WebpEncodeSettings::default())
    );

    // A clip with no video preview is unaffected: art_hash stays static.
    let no_video = art_clip("id-b");
    let clips = [&no_video];
    let on_novideo = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            animated_covers: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    assert!(
        on_novideo[0]
            .artifacts
            .iter()
            .all(|art| art.kind != ArtifactKind::CoverWebp)
    );
    assert_eq!(on_novideo[0].art_hash, crate::hash::art_hash(&no_video));

    // ALAC cannot embed WebP, so even with a video preview its art_hash stays
    // static (it always embeds the JPEG).
    let clips = [&with_video];
    let alac = build_desired(
        &clips,
        AudioFormat::Alac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            animated_covers: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    assert_eq!(alac[0].art_hash, crate::hash::art_hash(&with_video));
}

#[test]
fn build_desired_emits_video_mp4_only_when_enabled_and_video_present() {
    let with_video = Clip {
        video_url: "https://cdn.suno.ai/id-a/video.mp4".to_owned(),
        ..art_clip("id-a")
    };
    let clips = [&with_video];

    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    assert!(
        desired[0]
            .artifacts
            .iter()
            .all(|art| art.kind != ArtifactKind::VideoMp4)
    );

    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            video: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    let base = desired[0].path.strip_suffix(".flac").unwrap();
    let video = desired[0]
        .artifacts
        .iter()
        .find(|art| art.kind == ArtifactKind::VideoMp4)
        .expect("video expected");
    assert_eq!(video.path, format!("{base}.mp4"));
    assert_eq!(video.source_url, with_video.video_url);
    assert_eq!(video.hash, art_url_hash(&with_video.video_url));
    assert!(video.content.is_none());

    let no_video = art_clip("id-b");
    let clips = [&no_video];
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            video: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    assert!(
        desired[0]
            .artifacts
            .iter()
            .all(|art| art.kind != ArtifactKind::VideoMp4)
    );
}

#[test]
fn build_desired_emits_details_sidecar_only_when_enabled() {
    use crate::extras::render_clip_details;
    use crate::hash::content_hash;

    let a = clip("id-a", "Song", "alice");
    let clips = [&a];

    let off = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    assert!(
        off[0]
            .artifacts
            .iter()
            .all(|art| art.kind != ArtifactKind::DetailsTxt)
    );

    let on = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            details: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    let base = on[0].path.strip_suffix(".flac").unwrap();
    let details = on[0]
        .artifacts
        .iter()
        .find(|art| art.kind == ArtifactKind::DetailsTxt)
        .expect("details sidecar expected");
    assert_eq!(details.path, format!("{base}.details.txt"));
    assert_eq!(details.source_url, "");
    let body = render_clip_details(&a, &LineageContext::own_root(&a));
    assert_eq!(details.content.as_deref(), Some(body.as_str()));
    assert_eq!(details.hash, content_hash(&body));
}

#[test]
fn build_desired_emits_lyrics_sidecar_only_when_enabled_and_present() {
    let with_lyrics = Clip {
        lyrics: "la la la".to_owned(),
        ..clip("id-a", "Song", "alice")
    };
    let clips = [&with_lyrics];

    let off = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    assert!(
        off[0]
            .artifacts
            .iter()
            .all(|art| art.kind != ArtifactKind::LyricsTxt)
    );

    let on = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            lyrics: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    let base = on[0].path.strip_suffix(".flac").unwrap();
    let lyrics = on[0]
        .artifacts
        .iter()
        .find(|art| art.kind == ArtifactKind::LyricsTxt)
        .expect("lyrics sidecar expected");
    assert_eq!(lyrics.path, format!("{base}.lyrics.txt"));
    assert_eq!(lyrics.source_url, "");
    assert_eq!(lyrics.content.as_deref(), Some("la la la\n"));
    assert_eq!(lyrics.hash, content_hash("la la la\n"));
}

#[test]
fn build_desired_emits_lrc_sidecar_only_when_enabled() {
    let with_lyrics = Clip {
        lyrics: "la la la".to_owned(),
        ..clip("id-a", "Song", "alice")
    };
    let clips = [&with_lyrics];

    let off = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    assert!(
        off[0]
            .artifacts
            .iter()
            .all(|art| art.kind != ArtifactKind::Lrc)
    );

    let on = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            lrc: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    let base = on[0].path.strip_suffix(".flac").unwrap();
    let lrc = on[0]
        .artifacts
        .iter()
        .find(|art| art.kind == ArtifactKind::Lrc)
        .expect("lrc sidecar expected");
    assert_eq!(lrc.path, format!("{base}.lrc"));
    assert_eq!(lrc.source_url, "");
    assert_eq!(lrc.content, None);
    assert_eq!(lrc.hash, synced_lrc_source_hash(&with_lyrics.id));
}

#[test]
fn build_desired_emits_lrc_sidecar_from_prompt_when_feed_omits_lyrics() {
    let prompt_only = Clip {
        prompt: "the sung words live here".to_owned(),
        ..clip("id-a", "Song", "alice")
    };
    assert!(prompt_only.lyrics.is_empty());
    let clips = [&prompt_only];
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            lrc: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    let lrc = desired[0]
        .artifacts
        .iter()
        .find(|art| art.kind == ArtifactKind::Lrc)
        .expect("lrc sidecar expected");
    assert_eq!(lrc.content, None);
    assert_eq!(lrc.hash, synced_lrc_source_hash(&prompt_only.id));
}

#[test]
fn build_desired_emits_lrc_sidecar_even_when_feed_has_no_lyrics_or_prompt() {
    let bare = clip("id-a", "Song", "alice");
    assert!(bare.lyrics.is_empty() && bare.prompt.is_empty());
    let clips = [&bare];
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            lrc: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    let lrc = desired[0]
        .artifacts
        .iter()
        .find(|art| art.kind == ArtifactKind::Lrc)
        .expect("lrc sidecar expected even with no feed lyrics/prompt");
    assert_eq!(lrc.content, None);
    assert_eq!(lrc.hash, synced_lrc_source_hash(&bare.id));
}

#[test]
fn build_desired_omits_lyrics_sidecar_when_clip_has_no_lyrics() {
    let no_lyrics = clip("id-a", "Song", "alice");
    assert!(no_lyrics.lyrics.is_empty());
    let clips = [&no_lyrics];
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            lyrics: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    assert!(
        desired[0]
            .artifacts
            .iter()
            .all(|art| art.kind != ArtifactKind::LyricsTxt)
    );
}

#[test]
fn build_desired_text_sidecars_are_independent() {
    let full = Clip {
        lyrics: "words".to_owned(),
        ..art_clip("id-a")
    };
    let clips = [&full];
    let desired = build_desired(
        &clips,
        AudioFormat::Flac,
        &modes_for(&clips, SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles {
            details: true,
            lyrics: true,
            ..Default::default()
        },
        &NamingConfig::default(),
    );
    let base = desired[0].path.strip_suffix(".flac").unwrap();
    let kinds: BTreeSet<ArtifactKind> = desired[0].artifacts.iter().map(|a| a.kind).collect();
    assert!(kinds.contains(&ArtifactKind::CoverJpg));
    assert!(kinds.contains(&ArtifactKind::DetailsTxt));
    assert!(kinds.contains(&ArtifactKind::LyricsTxt));
    let path_of_kind = |k: ArtifactKind| {
        desired[0]
            .artifacts
            .iter()
            .find(|a| a.kind == k)
            .unwrap()
            .path
            .clone()
    };
    assert_eq!(
        path_of_kind(ArtifactKind::DetailsTxt),
        format!("{base}.details.txt")
    );
    assert_eq!(
        path_of_kind(ArtifactKind::LyricsTxt),
        format!("{base}.lyrics.txt")
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
fn build_playlist_desired_orders_members_and_marks_absent() {
    let a = clip("id-a", "Song A", "alice");
    let b = clip("id-b", "Song B", "alice");
    let desired = build_desired(
        &[&a, &b],
        AudioFormat::Flac,
        &modes_for(&[&a, &b], SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    let missing = clip("id-x", "Missing Song", "bob");
    let members = vec![b.clone(), missing.clone(), a.clone()];
    let inputs = vec![PlaylistInput {
        id: "pl1",
        name: "Road/Trip",
        members: &members,
    }];

    let out = build_playlist_desired(&inputs, &desired);
    assert_eq!(out.len(), 1);
    let pl = &out[0];
    assert_eq!(pl.id, "pl1");
    assert_eq!(pl.path, "Road Trip.m3u8");
    assert!(pl.content.starts_with("#EXTM3U\n#PLAYLIST:Road/Trip\n"));

    let pos_b = pl.content.find(path_of(&desired, "id-b")).unwrap();
    let pos_missing = pl.content.find("# (not in library) Missing Song").unwrap();
    let pos_a = pl.content.find(path_of(&desired, "id-a")).unwrap();
    assert!(pos_b < pos_missing && pos_missing < pos_a);
    assert!(!pl.content.contains("Missing Song\nbob/"));
    assert_eq!(pl.hash, content_hash(&pl.content));
}

#[test]
fn build_playlist_desired_builds_liked_and_multiple_in_order() {
    let a = clip("id-a", "Song A", "alice");
    let desired = build_desired(
        &[&a],
        AudioFormat::Flac,
        &modes_for(&[&a], SourceMode::Mirror),
        &no_contexts(),
        &no_collisions(),
        ArtifactToggles::default(),
        &NamingConfig::default(),
    );
    let members = vec![a.clone()];
    let inputs = vec![
        PlaylistInput {
            id: "pl1",
            name: "First",
            members: &members,
        },
        PlaylistInput {
            id: LIKED_PLAYLIST_ID,
            name: "Liked Songs",
            members: &members,
        },
    ];

    let out = build_playlist_desired(&inputs, &desired);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].id, "pl1");
    assert_eq!(out[1].id, LIKED_PLAYLIST_ID);
    assert_eq!(out[1].path, "Liked Songs.m3u8");
    assert!(out[0].content.contains(path_of(&desired, "id-a")));
    assert!(out[1].content.contains(path_of(&desired, "id-a")));
}

#[test]
fn build_playlist_desired_is_empty_for_no_inputs() {
    assert!(build_playlist_desired(&[], &[]).is_empty());
}
