use super::*;

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
