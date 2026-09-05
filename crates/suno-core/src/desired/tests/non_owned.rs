use super::*;
use crate::ManifestEntry;

#[test]
fn non_owned_lossless_clip_uses_native_mp3() {
    let mut clip = clip("foreign", "Foreign", "guest");
    clip.user_id = "other-user".to_owned();
    let mut desired = desired_of(
        &[&clip],
        AudioFormat::Flac,
        SourceMode::Mirror,
        ArtifactToggles::default(),
    );
    let old_path = desired[0].path.clone();

    let changes = apply_non_owned_audio_policy(
        &mut desired,
        &Manifest::new(),
        &HashMap::new(),
        "current-user",
        ArtifactToggles::default(),
    );

    assert_eq!(desired[0].format, AudioFormat::Mp3);
    assert!(desired[0].path.ends_with(".mp3"));
    assert_eq!(changes.get(&old_path), Some(&desired[0].path));
}

#[test]
fn owned_and_unknown_owner_clips_remain_strict_lossless() {
    let mut owned = clip("owned", "Owned", "me");
    owned.user_id = "current-user".to_owned();
    let unknown = clip("unknown", "Unknown", "me");
    let mut desired = desired_of(
        &[&owned, &unknown],
        AudioFormat::Flac,
        SourceMode::Mirror,
        ArtifactToggles::default(),
    );

    let changes = apply_non_owned_audio_policy(
        &mut desired,
        &Manifest::new(),
        &HashMap::new(),
        "current-user",
        ArtifactToggles::default(),
    );

    assert!(changes.is_empty());
    assert!(desired.iter().all(|item| item.format == AudioFormat::Flac));
}

#[test]
fn existing_non_owned_lossless_file_is_never_downgraded() {
    let mut clip = clip("foreign", "Foreign", "guest");
    clip.user_id = "other-user".to_owned();
    let mut desired = desired_of(
        &[&clip],
        AudioFormat::Flac,
        SourceMode::Mirror,
        ArtifactToggles::default(),
    );
    let mut manifest = Manifest::new();
    manifest.insert(
        clip.id.clone(),
        ManifestEntry {
            path: desired[0].path.clone(),
            format: AudioFormat::Flac,
            size: 100,
            ..Default::default()
        },
    );
    let local = HashMap::from([(
        clip.id.clone(),
        LocalFile {
            exists: true,
            size: 100,
            ..Default::default()
        },
    )]);

    let changes = apply_non_owned_audio_policy(
        &mut desired,
        &manifest,
        &local,
        "current-user",
        ArtifactToggles::default(),
    );

    assert!(changes.is_empty());
    assert_eq!(desired[0].format, AudioFormat::Flac);
}

#[test]
fn playlist_paths_follow_the_effective_non_owned_format() {
    let old = "guest/Album/Song.flac";
    let new = "guest/Album/Song.mp3";
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
    assert_ne!(playlists[0].hash, "old");
}
