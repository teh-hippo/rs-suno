use super::*;
use crate::manifest::ManifestEntry;
use crate::model::{LosslessAccess, LosslessUnavailableReason};
use crate::reconcile::{Action, SourceStatus, reconcile};

#[test]
fn unavailable_lossless_uses_mp3_but_preserves_present_lossless() {
    let keep = clip("keep", "Keep", "alice");
    let prior_mp3 = clip("mp3", "Prior MP3", "alice");
    let fresh = clip("fresh", "Fresh", "alice");
    let mut desired = desired_of(
        &[&keep, &prior_mp3, &fresh],
        AudioFormat::Flac,
        SourceMode::Mirror,
        ArtifactToggles::default(),
    );
    let old_paths: HashMap<String, String> = desired
        .iter()
        .map(|d| (d.clip.id.clone(), d.path.clone()))
        .collect();
    let mut manifest = Manifest::new();
    manifest.insert(
        "keep",
        ManifestEntry {
            path: old_paths["keep"].clone(),
            format: AudioFormat::Flac,
            size: 100,
            ..Default::default()
        },
    );
    manifest.insert(
        "mp3",
        ManifestEntry {
            path: old_paths["mp3"].replace(".flac", ".mp3"),
            format: AudioFormat::Mp3,
            size: 80,
            ..Default::default()
        },
    );
    let local = HashMap::from([
        (
            "keep".to_owned(),
            LocalFile {
                exists: true,
                size: 100,
                ..Default::default()
            },
        ),
        (
            "mp3".to_owned(),
            LocalFile {
                exists: true,
                size: 80,
                ..Default::default()
            },
        ),
    ]);

    let changes = apply_lossless_fallback(
        &mut desired,
        &manifest,
        &local,
        LosslessAccess::Unavailable(LosslessUnavailableReason::Paused),
        ArtifactToggles::default(),
    );

    let keep = desired.iter().find(|d| d.clip.id == "keep").unwrap();
    assert_eq!(keep.format, AudioFormat::Flac);
    assert_eq!(keep.path, old_paths["keep"]);
    for id in ["mp3", "fresh"] {
        let d = desired.iter().find(|d| d.clip.id == id).unwrap();
        assert_eq!(d.format, AudioFormat::Mp3);
        assert!(d.path.ends_with(".mp3"));
        assert_eq!(changes.get(&old_paths[id]), Some(&d.path));
    }
}

#[test]
fn playlist_paths_follow_the_effective_audio_format() {
    let old = "alice/Album/Song.flac";
    let new = "alice/Album/Song.mp3";
    let mut playlists = vec![PlaylistDesired {
        id: "pl".to_owned(),
        name: "Mix".to_owned(),
        path: "Mix.m3u8".to_owned(),
        content: format!("#EXTM3U\n#EXTINF:1,Song\n{old}\n"),
        hash: "old".to_owned(),
        cover_jpg: None,
    }];

    rewrite_playlist_paths(
        &mut playlists,
        &HashMap::from([(old.to_owned(), new.to_owned())]),
    );

    assert!(playlists[0].content.contains(new));
    assert!(!playlists[0].content.contains(old));
    assert_eq!(playlists[0].hash, content_hash(&playlists[0].content));
}

#[test]
fn unknown_access_keeps_the_configured_target() {
    let c = clip("a", "Song", "alice");
    let mut desired = desired_of(
        &[&c],
        AudioFormat::Flac,
        SourceMode::Mirror,
        ArtifactToggles::default(),
    );
    let original = desired[0].clone();

    let changes = apply_lossless_fallback(
        &mut desired,
        &Manifest::new(),
        &HashMap::new(),
        LosslessAccess::Unknown,
        ArtifactToggles::default(),
    );

    assert!(changes.is_empty());
    assert_eq!(desired[0], original);
}

#[test]
fn temporary_mp3_manifest_reformats_to_lossless_when_access_returns() {
    let c = clip("upgrade", "Song", "alice");
    let mut unavailable = desired_of(
        &[&c],
        AudioFormat::Flac,
        SourceMode::Mirror,
        ArtifactToggles::default(),
    );
    apply_lossless_fallback(
        &mut unavailable,
        &Manifest::new(),
        &HashMap::new(),
        LosslessAccess::Unavailable(LosslessUnavailableReason::Paused),
        ArtifactToggles::default(),
    );
    let fallback = unavailable[0].clone();
    assert_eq!(fallback.format, AudioFormat::Mp3);
    let first = reconcile(
        &Manifest::new(),
        std::slice::from_ref(&fallback),
        &HashMap::new(),
        &[SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: true,
        }],
    );
    assert!(matches!(
        &first.actions[0],
        Action::Download {
            format: AudioFormat::Mp3,
            ..
        }
    ));

    let mut manifest = Manifest::new();
    manifest.insert(
        "upgrade",
        ManifestEntry {
            path: fallback.path.clone(),
            format: AudioFormat::Mp3,
            size: 100,
            ..Default::default()
        },
    );
    let available = desired_of(
        &[&c],
        AudioFormat::Flac,
        SourceMode::Mirror,
        ArtifactToggles::default(),
    );
    let second = reconcile(
        &manifest,
        &available,
        &HashMap::from([(
            "upgrade".to_owned(),
            LocalFile {
                exists: true,
                size: 100,
                ..Default::default()
            },
        )]),
        &[SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: true,
        }],
    );
    assert!(matches!(
        &second.actions[0],
        Action::Reformat {
            from: AudioFormat::Mp3,
            to: AudioFormat::Flac,
            ..
        }
    ));
}
