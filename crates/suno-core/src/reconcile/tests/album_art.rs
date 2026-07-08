use super::*;
use crate::desired::album_desired;
use crate::hash::{art_url_hash, webp_art_hash};
use crate::vocab::WebpEncodeSettings;

#[test]
fn folder_art_missing_on_disk_forces_rewrite() {
    // The album store records a matching folder.jpg, but the file is absent:
    // the probe must force a WriteArtifact.
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", ""),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    let mut local: HashMap<String, LocalFile> = HashMap::new();
    local.insert("c/al/folder.jpg".to_owned(), LocalFile::default());
    let actions = plan_album_artifacts(&desired, &albums, true, &local);
    assert_eq!(actions.len(), 1, "missing folder art must be rewritten");
    assert!(matches!(
        &actions[0],
        Action::WriteArtifact {
            kind: ArtifactKind::FolderJpg,
            ..
        }
    ));
}

#[test]
fn folder_art_present_on_disk_no_churn() {
    // Matching hash+path and the file is present: no write.
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", ""),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    let mut local: HashMap<String, LocalFile> = HashMap::new();
    local.insert("c/al/folder.jpg".to_owned(), present(5000));
    let actions = plan_album_artifacts(&desired, &albums, true, &local);
    assert!(
        actions.is_empty(),
        "present folder art with matching hash must not churn"
    );
}

fn album_clip(id: &str, play_count: u64, created_at: &str, image: &str, video: &str) -> Clip {
    Clip {
        id: id.to_string(),
        title: "Song".to_string(),
        image_large_url: image.to_string(),
        video_cover_url: video.to_string(),
        play_count,
        created_at: created_at.to_string(),
        ..Default::default()
    }
}

fn album_member(clip: Clip, root_id: &str, path: &str) -> Desired {
    let mut lineage = LineageContext::own_root(&clip);
    lineage.root_id = root_id.to_string();
    Desired {
        clip,
        lineage,
        path: path.to_string(),
        format: AudioFormat::Flac,
        meta_hash: "m".to_string(),
        art_hash: "a".to_string(),
        modes: vec![SourceMode::Mirror],
        trashed: false,
        private: false,
        artifacts: Vec::new(),
        stems: None,
    }
}

fn stored(path: &str, hash: &str) -> ArtifactState {
    ArtifactState {
        path: path.to_string(),
        hash: hash.to_string(),
    }
}

#[test]
fn plan_writes_folder_art_when_store_empty() {
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", "vid-a"),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, true, false, WebpEncodeSettings::default());
    let actions = plan_album_artifacts(&desired, &BTreeMap::new(), true, &HashMap::new());
    assert_eq!(
        actions,
        vec![
            Action::WriteArtifact {
                kind: ArtifactKind::FolderJpg,
                path: "c/al/folder.jpg".to_string(),
                source_url: "art-a".to_string(),
                hash: art_url_hash("art-a"),
                owner_id: "root".to_string(),
                content: None,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::FolderWebp,
                path: "c/al/cover.webp".to_string(),
                source_url: "vid-a".to_string(),
                hash: webp_art_hash("vid-a", &WebpEncodeSettings::default()),
                owner_id: "root".to_string(),
                content: None,
            },
        ]
    );
}

#[test]
fn plan_skips_when_hash_and_path_match() {
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", ""),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    assert!(plan_album_artifacts(&desired, &albums, true, &HashMap::new()).is_empty());
}

#[test]
fn plan_rewrites_when_path_drifts_even_if_hash_matches() {
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", ""),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("old/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::WriteArtifact { path, .. } if path == "c/al/folder.jpg"
    ));
}

#[test]
fn h1_most_played_flip_to_same_art_writes_nothing() {
    // Two variants sharing identical art. Run 1: "a" is most-played.
    let run1 = vec![
        album_member(
            album_clip("a", 9, "t0", "same-art", ""),
            "root",
            "c/al/a.flac",
        ),
        album_member(
            album_clip("b", 1, "t1", "same-art", ""),
            "root",
            "c/al/b.flac",
        ),
    ];
    let desired1 = album_desired(&run1, false, false, WebpEncodeSettings::default());
    let write1 = plan_album_artifacts(&desired1, &BTreeMap::new(), true, &HashMap::new());
    assert_eq!(write1.len(), 1);

    // Persist the winner's state as the executor would.
    let mut albums = BTreeMap::new();
    if let Action::WriteArtifact {
        path,
        hash,
        owner_id,
        ..
    } = &write1[0]
    {
        albums.insert(
            owner_id.clone(),
            AlbumArt {
                folder_jpg: Some(stored(path, hash)),
                folder_webp: None,
                folder_mp4: None,
            },
        );
    }

    // Run 2: "b" overtakes "a" on plays, but the art content is identical.
    let run2 = vec![
        album_member(
            album_clip("a", 1, "t0", "same-art", ""),
            "root",
            "c/al/a.flac",
        ),
        album_member(
            album_clip("b", 9, "t1", "same-art", ""),
            "root",
            "c/al/b.flac",
        ),
    ];
    let desired2 = album_desired(&run2, false, false, WebpEncodeSettings::default());
    // The winner flipped, but the chosen art content hash did not: no churn.
    assert!(plan_album_artifacts(&desired2, &albums, true, &HashMap::new()).is_empty());
}

#[test]
fn h1_flip_to_different_art_writes_exactly_one() {
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("old-art"))),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    // The new most-played variant carries genuinely different art.
    let members = vec![
        album_member(
            album_clip("a", 1, "t0", "old-art", ""),
            "root",
            "c/al/a.flac",
        ),
        album_member(
            album_clip("b", 9, "t1", "new-art", ""),
            "root",
            "c/al/b.flac",
        ),
    ];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());
    let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::WriteArtifact { hash, .. } if *hash == art_url_hash("new-art")
    ));
}

#[test]
fn one_write_per_album_regardless_of_clip_count() {
    let members: Vec<Desired> = (0..200)
        .map(|i| {
            album_member(
                album_clip(
                    &format!("clip-{i:03}"),
                    i as u64,
                    &format!("t{i:03}"),
                    &format!("art-{i:03}"),
                    &format!("vid-{i:03}"),
                ),
                "root",
                &format!("c/al/clip-{i:03}.flac"),
            )
        })
        .collect();
    let desired = album_desired(&members, true, false, WebpEncodeSettings::default());
    assert_eq!(desired.len(), 1);
    let actions = plan_album_artifacts(&desired, &BTreeMap::new(), true, &HashMap::new());
    // Exactly one folder.jpg and one cover.webp for the whole 200-clip album.
    assert_eq!(actions.len(), 2);
    assert_eq!(
        actions
            .iter()
            .filter(|a| matches!(a, Action::WriteArtifact { .. }))
            .count(),
        2
    );
}

#[test]
fn emptied_album_deletes_only_when_can_delete() {
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", "h")),
            folder_webp: Some(stored("c/al/cover.webp", "hw")),
            folder_mp4: Some(stored("c/al/cover.mp4", "hm")),
        },
    );
    // No album desires this root any more (it emptied out this run).
    let desired: Vec<AlbumDesired> = Vec::new();

    // Gated off: an incomplete/unsafe listing removes nothing.
    assert!(plan_album_artifacts(&desired, &albums, false, &HashMap::new()).is_empty());

    // Gated on: every stored kind is removed, sorted by kind.
    let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
    assert_eq!(
        actions,
        vec![
            Action::DeleteArtifact {
                kind: ArtifactKind::FolderJpg,
                path: "c/al/folder.jpg".to_string(),
                owner_id: "root".to_string(),
            },
            Action::DeleteArtifact {
                kind: ArtifactKind::FolderWebp,
                path: "c/al/cover.webp".to_string(),
                owner_id: "root".to_string(),
            },
            Action::DeleteArtifact {
                kind: ArtifactKind::FolderMp4,
                path: "c/al/cover.mp4".to_string(),
                owner_id: "root".to_string(),
            },
        ]
    );
}

#[test]
fn disappeared_webp_source_deletes_only_that_kind_when_gated() {
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: Some(stored("c/al/cover.webp", &art_url_hash("vid-a"))),
            folder_mp4: None,
        },
    );
    // The album is still present with the same folder.jpg, but animated
    // covers are now off, so the webp source has disappeared.
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", "vid-a"),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());

    assert!(plan_album_artifacts(&desired, &albums, false, &HashMap::new()).is_empty());

    let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
    assert_eq!(
        actions,
        vec![Action::DeleteArtifact {
            kind: ArtifactKind::FolderWebp,
            path: "c/al/cover.webp".to_string(),
            owner_id: "root".to_string(),
        }]
    );
}

#[test]
fn disappeared_raw_cover_deletes_only_that_kind_when_gated() {
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: Some(stored(
                "c/al/cover.webp",
                &webp_art_hash("vid-a", &WebpEncodeSettings::default()),
            )),
            folder_mp4: Some(stored("c/al/cover.mp4", &art_url_hash("vid-a"))),
        },
    );
    // The album stays and animated covers stay on, but raw cover retention
    // is now off, so only the raw `cover.mp4` is no longer desired.
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", "vid-a"),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, true, false, WebpEncodeSettings::default());

    // Gated off: nothing removed on an unsafe listing.
    assert!(plan_album_artifacts(&desired, &albums, false, &HashMap::new()).is_empty());

    // Gated on: only the raw cover goes; folder.jpg and cover.webp stay.
    let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
    assert_eq!(
        actions,
        vec![Action::DeleteArtifact {
            kind: ArtifactKind::FolderMp4,
            path: "c/al/cover.mp4".to_string(),
            owner_id: "root".to_string(),
        }]
    );
}

#[test]
fn plan_album_artifacts_is_deterministically_ordered() {
    let members = vec![
        album_member(
            album_clip("a", 1, "t0", "art-a", "vid-a"),
            "r2",
            "c/al2/a.flac",
        ),
        album_member(
            album_clip("b", 1, "t0", "art-b", "vid-b"),
            "r1",
            "c/al1/b.flac",
        ),
    ];
    let desired = album_desired(&members, true, true, WebpEncodeSettings::default());
    let actions = plan_album_artifacts(&desired, &BTreeMap::new(), true, &HashMap::new());
    let keys: Vec<(&str, ArtifactKind)> = actions
        .iter()
        .map(|a| match a {
            Action::WriteArtifact { owner_id, kind, .. } => (owner_id.as_str(), *kind),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            ("r1", ArtifactKind::FolderJpg),
            ("r1", ArtifactKind::FolderWebp),
            ("r1", ArtifactKind::FolderMp4),
            ("r2", ArtifactKind::FolderJpg),
            ("r2", ArtifactKind::FolderWebp),
            ("r2", ArtifactKind::FolderMp4),
        ]
    );
}

#[test]
fn delete_album_artifact_gate_needs_the_verdict_and_a_path() {
    // The shared deletion verdict and a non-empty path yield a folder-art delete.
    assert_eq!(
        delete_album_artifact_action("r1", ArtifactKind::FolderJpg, "r1/folder.jpg", true),
        Some(Action::DeleteArtifact {
            kind: ArtifactKind::FolderJpg,
            path: "r1/folder.jpg".to_owned(),
            owner_id: "r1".to_owned(),
        })
    );
    // The shared deletion verdict is required (no album-specific override).
    assert_eq!(
        delete_album_artifact_action("r1", ArtifactKind::FolderJpg, "r1/folder.jpg", false),
        None
    );
    // An empty path can never delete the account root.
    assert_eq!(
        delete_album_artifact_action("r1", ArtifactKind::FolderJpg, "", true),
        None
    );
}
