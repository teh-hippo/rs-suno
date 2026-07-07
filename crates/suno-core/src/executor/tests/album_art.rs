use super::*;

#[test]
fn folder_jpg_write_records_album_state_and_skips_manifest() {
    // Folder art is owned by the album root id, not a manifest clip: it
    // writes even with an empty manifest and records on the album store.
    let mut manifest = Manifest::new();
    let mut albums: BTreeMap<String, AlbumArt> = BTreeMap::new();
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::FolderJpg,
            path: "creator/album/folder.jpg".to_owned(),
            source_url: "https://art.suno.ai/root/large.jpg".to_owned(),
            hash: "jh".to_owned(),
            owner_id: "root".to_owned(),
            content: None,
        }],
    };
    let http = ScriptedHttp::new().route("root/large.jpg", Reply::ok(b"folder-jpg".to_vec()));
    let fs = MemFs::new();

    let outcome = run_with_albums(
        &plan,
        &mut manifest,
        &mut albums,
        &[],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.artifacts_written, 1);
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(
        fs.read_file("creator/album/folder.jpg").unwrap(),
        b"folder-jpg"
    );
    assert_eq!(
        albums.get("root").unwrap().folder_jpg,
        Some(ArtifactState {
            path: "creator/album/folder.jpg".to_owned(),
            hash: "jh".to_owned(),
        })
    );
    assert!(manifest.get("root").is_none());
}

#[test]
fn folder_webp_write_transcodes_and_records_album_state() {
    let mut manifest = Manifest::new();
    let mut albums: BTreeMap<String, AlbumArt> = BTreeMap::new();
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::FolderWebp,
            path: "creator/album/cover.webp".to_owned(),
            source_url: "https://cdn.suno.ai/root/video.mp4".to_owned(),
            hash: "wh".to_owned(),
            owner_id: "root".to_owned(),
            content: None,
        }],
    };
    let http = ScriptedHttp::new().route("root/video.mp4", Reply::ok(b"mp4-bytes".to_vec()));
    let fs = MemFs::new();

    let outcome = run_with_albums(
        &plan,
        &mut manifest,
        &mut albums,
        &[],
        &http,
        &fs,
        &StubFfmpeg::webp(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.artifacts_written, 1);
    assert_eq!(outcome.failed(), 0);
    // The MP4 was transcoded to WebP, not written verbatim.
    let written = fs.read_file("creator/album/cover.webp").unwrap();
    assert_ne!(written, b"mp4-bytes");
    assert!(written.starts_with(b"RIFF"));
    assert_eq!(
        albums.get("root").unwrap().folder_webp,
        Some(ArtifactState {
            path: "creator/album/cover.webp".to_owned(),
            hash: "wh".to_owned(),
        })
    );
}

#[test]
fn folder_mp4_write_keeps_the_source_verbatim() {
    let mut manifest = Manifest::new();
    let mut albums: BTreeMap<String, AlbumArt> = BTreeMap::new();
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::FolderMp4,
            path: "creator/album/cover.mp4".to_owned(),
            source_url: "https://cdn.suno.ai/root/video.mp4".to_owned(),
            hash: "mh".to_owned(),
            owner_id: "root".to_owned(),
            content: None,
        }],
    };
    let http = ScriptedHttp::new().route("root/video.mp4", Reply::ok(b"mp4-bytes".to_vec()));
    let fs = MemFs::new();

    let outcome = run_with_albums(
        &plan,
        &mut manifest,
        &mut albums,
        &[],
        &http,
        &fs,
        &StubFfmpeg::webp(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.artifacts_written, 1);
    assert_eq!(outcome.failed(), 0);
    // The raw MP4 is written byte-for-byte, never transcoded.
    assert_eq!(
        fs.read_file("creator/album/cover.mp4").unwrap(),
        b"mp4-bytes"
    );
    assert_eq!(
        albums.get("root").unwrap().folder_mp4,
        Some(ArtifactState {
            path: "creator/album/cover.mp4".to_owned(),
            hash: "mh".to_owned(),
        })
    );
}

#[test]
fn both_folder_covers_fetch_the_video_cover_once() {
    let mut manifest = Manifest::new();
    let mut albums: BTreeMap<String, AlbumArt> = BTreeMap::new();
    // `both` retention keeps cover.webp (transcoded) and cover.mp4 (raw) from
    // the one video_cover_url. FolderWebp sorts first and caches the fetched
    // source; FolderMp4 drains it, so the source is fetched exactly once.
    let plan = Plan {
        actions: vec![
            Action::WriteArtifact {
                kind: ArtifactKind::FolderWebp,
                path: "creator/album/cover.webp".to_owned(),
                source_url: "https://cdn.suno.ai/root/video.mp4".to_owned(),
                hash: "wh".to_owned(),
                owner_id: "root".to_owned(),
                content: None,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::FolderMp4,
                path: "creator/album/cover.mp4".to_owned(),
                source_url: "https://cdn.suno.ai/root/video.mp4".to_owned(),
                hash: "mh".to_owned(),
                owner_id: "root".to_owned(),
                content: None,
            },
        ],
    };
    let http = ScriptedHttp::new().route("root/video.mp4", Reply::ok(b"mp4-bytes".to_vec()));
    let fs = MemFs::new();

    let outcome = run_with_albums(
        &plan,
        &mut manifest,
        &mut albums,
        &[],
        &http,
        &fs,
        &StubFfmpeg::webp(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.artifacts_written, 2);
    assert_eq!(outcome.failed(), 0);
    // Fetched exactly once despite two artifacts consuming it (#90 / #89).
    assert_eq!(http.count("root/video.mp4"), 1);
    // The webp is transcoded; the mp4 is the raw source verbatim.
    assert!(
        fs.read_file("creator/album/cover.webp")
            .unwrap()
            .starts_with(b"RIFF")
    );
    assert_eq!(
        fs.read_file("creator/album/cover.mp4").unwrap(),
        b"mp4-bytes"
    );
}

#[test]
fn folder_art_delete_clears_album_state() {
    let fs = MemFs::new().with_file("creator/album/folder.jpg", b"jpg".to_vec());
    let mut manifest = Manifest::new();
    let mut albums: BTreeMap<String, AlbumArt> = BTreeMap::new();
    albums.insert(
        "root".to_owned(),
        AlbumArt {
            folder_jpg: Some(ArtifactState {
                path: "creator/album/folder.jpg".to_owned(),
                hash: "jh".to_owned(),
            }),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    let plan = Plan {
        actions: vec![Action::DeleteArtifact {
            kind: ArtifactKind::FolderJpg,
            path: "creator/album/folder.jpg".to_owned(),
            owner_id: "root".to_owned(),
        }],
    };

    let outcome = run_with_albums(
        &plan,
        &mut manifest,
        &mut albums,
        &[],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.artifacts_deleted, 1);
    assert!(!fs.exists("creator/album/folder.jpg"));
    // The album row had only the one kind, so it is pruned entirely.
    assert!(!albums.contains_key("root"));
}

#[test]
fn playlist_write_uses_inline_content_and_records_state() {
    // A playlist body is generated, carried inline. With an empty manifest
    // and NO http routes, the write still succeeds — proving it skipped the
    // network — and records the playlist store keyed by the playlist id.
    let mut manifest = Manifest::new();
    let mut albums: BTreeMap<String, AlbumArt> = BTreeMap::new();
    let mut playlists: BTreeMap<String, PlaylistState> = BTreeMap::new();
    let body = "#EXTM3U\n#PLAYLIST:Road Trip\n#EXTINF:60,One\nA/One.flac\n";
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::Playlist,
            path: "Road Trip.m3u8".to_owned(),
            source_url: String::new(),
            hash: "ph1".to_owned(),
            owner_id: "pl1".to_owned(),
            content: Some(body.to_owned()),
        }],
    };
    let fs = MemFs::new();

    let outcome = run_full(
        &plan,
        &mut manifest,
        &mut albums,
        &mut playlists,
        &[],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.artifacts_written, 1);
    assert_eq!(outcome.failed(), 0);
    // The exact inline bytes were written, verbatim.
    assert_eq!(fs.read_file("Road Trip.m3u8").unwrap(), body.as_bytes());
    assert_eq!(
        playlists.get("pl1"),
        Some(&PlaylistState {
            name: "Road Trip".to_owned(),
            path: "Road Trip.m3u8".to_owned(),
            hash: "ph1".to_owned(),
        })
    );
}

#[test]
fn playlist_delete_removes_file_and_clears_state() {
    let fs = MemFs::new().with_file("Old.m3u8", b"#EXTM3U\n".to_vec());
    let mut manifest = Manifest::new();
    let mut albums: BTreeMap<String, AlbumArt> = BTreeMap::new();
    let mut playlists: BTreeMap<String, PlaylistState> = BTreeMap::new();
    playlists.insert(
        "pl1".to_owned(),
        PlaylistState {
            name: "Old".to_owned(),
            path: "Old.m3u8".to_owned(),
            hash: "ph1".to_owned(),
        },
    );
    let plan = Plan {
        actions: vec![Action::DeleteArtifact {
            kind: ArtifactKind::Playlist,
            path: "Old.m3u8".to_owned(),
            owner_id: "pl1".to_owned(),
        }],
    };

    let outcome = run_full(
        &plan,
        &mut manifest,
        &mut albums,
        &mut playlists,
        &[],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.artifacts_deleted, 1);
    assert!(!fs.exists("Old.m3u8"));
    assert!(
        !playlists.contains_key("pl1"),
        "the playlist row is cleared on delete"
    );
}

#[test]
fn rename_move_relocates_cover_and_prunes_old_album() {
    // A title/album change moves the audio (Rename) and re-emits the cover
    // at the NEW path. The old cover must be removed and the now-empty old
    // album directory pruned, leaving no orphan sidecar and no ghost dir.
    let mut manifest = Manifest::new();
    let mut e = entry("Creator/AlbumA/song.flac", AudioFormat::Flac);
    e.cover_jpg = Some(ArtifactState {
        path: "Creator/AlbumA/cover.jpg".to_owned(),
        hash: "h1".to_owned(),
    });
    manifest.insert("a", e);
    let fs = MemFs::new()
        .with_file("Creator/AlbumA/song.flac", b"AUDIO".to_vec())
        .with_file("Creator/AlbumA/cover.jpg", b"old-jpg".to_vec());
    let plan = Plan {
        actions: vec![
            Action::Rename {
                from: "Creator/AlbumA/song.flac".to_owned(),
                to: "Creator/AlbumB/song.flac".to_owned(),
            },
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "Creator/AlbumB/cover.jpg".to_owned(),
                source_url: "https://art.suno.ai/a/large.jpg".to_owned(),
                hash: "h1".to_owned(),
                owner_id: "a".to_owned(),
                content: None,
            },
        ],
    };
    let http = ScriptedHttp::new().route("a/large.jpg", Reply::ok(b"new-jpg".to_vec()));

    let outcome = run(
        &plan,
        &mut manifest,
        &[],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.failed(), 0);
    // Audio moved, the new cover was written, the old cover removed.
    assert!(fs.exists("Creator/AlbumB/song.flac"));
    assert_eq!(
        fs.read_file("Creator/AlbumB/cover.jpg").unwrap(),
        b"new-jpg"
    );
    assert!(!fs.exists("Creator/AlbumA/cover.jpg"));
    assert!(!fs.exists("Creator/AlbumA/song.flac"));
    // The manifest cover slot now points at the new path.
    assert_eq!(
        manifest.get("a").unwrap().cover_jpg.as_ref().unwrap().path,
        "Creator/AlbumB/cover.jpg"
    );
    // The emptied old album directory is pruned; the new one survives.
    assert!(!fs.has_dir("Creator/AlbumA"));
    assert!(fs.has_dir("Creator/AlbumB"));
}

#[test]
fn rename_move_relocates_folder_art_and_prunes_old_album() {
    // An album rename moves folder.jpg: the old file is removed, the album
    // store slot advanced to the new path, and the emptied dir pruned.
    let mut manifest = Manifest::new();
    let mut albums: BTreeMap<String, AlbumArt> = BTreeMap::new();
    albums.insert(
        "root".to_owned(),
        AlbumArt {
            folder_jpg: Some(ArtifactState {
                path: "Creator/AlbumA/folder.jpg".to_owned(),
                hash: "jh".to_owned(),
            }),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    let fs = MemFs::new().with_file("Creator/AlbumA/folder.jpg", b"old-folder".to_vec());
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::FolderJpg,
            path: "Creator/AlbumB/folder.jpg".to_owned(),
            source_url: "https://art.suno.ai/root/large.jpg".to_owned(),
            hash: "jh".to_owned(),
            owner_id: "root".to_owned(),
            content: None,
        }],
    };
    let http = ScriptedHttp::new().route("root/large.jpg", Reply::ok(b"new-folder".to_vec()));

    let outcome = run_with_albums(
        &plan,
        &mut manifest,
        &mut albums,
        &[],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.failed(), 0);
    assert_eq!(
        fs.read_file("Creator/AlbumB/folder.jpg").unwrap(),
        b"new-folder"
    );
    assert!(!fs.exists("Creator/AlbumA/folder.jpg"));
    assert_eq!(
        albums
            .get("root")
            .unwrap()
            .folder_jpg
            .as_ref()
            .unwrap()
            .path,
        "Creator/AlbumB/folder.jpg"
    );
    assert!(!fs.has_dir("Creator/AlbumA"));
    assert!(fs.has_dir("Creator/AlbumB"));
}

#[test]
fn prune_empty_dirs_removes_only_empty_dirs() {
    // A direct exercise of the prune port's safety guarantees on a mixed
    // tree: nested empties go, anything holding a file (hidden ones too)
    // stays, and no file is touched.
    let fs = MemFs::new()
        .with_file("keep/full/song.flac", b"x".to_vec())
        .with_file("hidden/.suno-manifest.json", b"{}".to_vec())
        .with_dir("empty/leaf")
        .with_dir("nested/a/b/c");

    fs.prune_empty_dirs("").unwrap();

    // Every empty directory, however deeply nested, is pruned bottom-up.
    for gone in [
        "empty",
        "empty/leaf",
        "nested",
        "nested/a",
        "nested/a/b",
        "nested/a/b/c",
    ] {
        assert!(!fs.has_dir(gone), "empty dir {gone} should be pruned");
    }
    // A directory holding any file — including only a hidden dotfile — stays.
    assert!(fs.has_dir("keep"));
    assert!(fs.has_dir("keep/full"));
    assert!(fs.has_dir("hidden"));
    // No file was touched.
    assert!(fs.exists("keep/full/song.flac"));
    assert!(fs.exists("hidden/.suno-manifest.json"));
}

#[test]
fn prune_empty_dirs_never_removes_the_named_root() {
    // Pruning under a named root clears its empty children but keeps the
    // root itself, even when the root is now empty.
    let fs = MemFs::new().with_dir("empty/leaf");
    fs.prune_empty_dirs("empty").unwrap();
    assert!(fs.has_dir("empty"), "the named root is never removed");
    assert!(!fs.has_dir("empty/leaf"));
}

#[test]
fn old_sidecar_remove_failure_is_per_clip_and_converges_next_run() {
    // If removing the old sidecar fails, the write is a per-clip failure
    // that never aborts the run and does NOT advance the state slot, so the
    // next identical run re-attempts the cleanup and the tree converges.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac);
    e.cover_jpg = Some(ArtifactState {
        path: "AlbumA/cover.jpg".to_owned(),
        hash: "h1".to_owned(),
    });
    manifest.insert("a", e);
    let fs = MemFs::new()
        .with_file("a.flac", b"AUDIO".to_vec())
        .with_file("AlbumA/cover.jpg", b"old".to_vec());
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::CoverJpg,
            path: "AlbumB/cover.jpg".to_owned(),
            source_url: "https://art.suno.ai/a/large.jpg".to_owned(),
            hash: "h1".to_owned(),
            owner_id: "a".to_owned(),
            content: None,
        }],
    };
    let http = ScriptedHttp::new().route("a/large.jpg", Reply::ok(b"new".to_vec()));

    // Run 1: the old-cover remove is forced to fail.
    fs.arm_fail_remove("AlbumA/cover.jpg");
    let first = run(
        &plan,
        &mut manifest,
        &[],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );
    assert_eq!(
        first.status,
        RunStatus::Completed,
        "a remove failure never aborts the run"
    );
    assert_eq!(first.failed(), 1);
    // The new cover is written but the old one lingers and the slot is stale.
    assert!(fs.exists("AlbumB/cover.jpg"));
    assert!(fs.exists("AlbumA/cover.jpg"));
    assert_eq!(
        manifest.get("a").unwrap().cover_jpg.as_ref().unwrap().path,
        "AlbumA/cover.jpg"
    );
    assert!(fs.has_dir("AlbumA"), "the orphan keeps its directory alive");

    // Run 2: the same plan re-runs with the fault cleared and converges.
    fs.disarm_fail_remove("AlbumA/cover.jpg");
    let second = run(
        &plan,
        &mut manifest,
        &[],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );
    assert_eq!(second.failed(), 0);
    assert!(fs.exists("AlbumB/cover.jpg"));
    assert!(!fs.exists("AlbumA/cover.jpg"), "no orphan persists");
    assert_eq!(
        manifest.get("a").unwrap().cover_jpg.as_ref().unwrap().path,
        "AlbumB/cover.jpg"
    );
    assert!(!fs.has_dir("AlbumA"), "the emptied directory is pruned");
}

#[test]
fn same_path_artifact_rewrite_does_no_remove_and_prunes_nothing() {
    // The idempotent case: a content-only cover rewrite (hash drift, path
    // unchanged) attempts no remove and prunes no live directory. A remove
    // failure is armed on the cover path, so any spurious remove would
    // surface as a failure — none does.
    let mut manifest = Manifest::new();
    let mut e = entry("Album/a.mp3", AudioFormat::Mp3);
    e.cover_jpg = Some(ArtifactState {
        path: "Album/cover.jpg".to_owned(),
        hash: "h1".to_owned(),
    });
    manifest.insert("a", e);
    let fs = MemFs::new()
        .with_file("Album/a.mp3", b"AUDIO".to_vec())
        .with_file("Album/cover.jpg", b"old".to_vec());
    fs.arm_fail_remove("Album/cover.jpg");
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::CoverJpg,
            path: "Album/cover.jpg".to_owned(),
            source_url: "https://art.suno.ai/a/large.jpg".to_owned(),
            hash: "h2".to_owned(),
            owner_id: "a".to_owned(),
            content: None,
        }],
    };
    let http = ScriptedHttp::new().route("a/large.jpg", Reply::ok(b"new".to_vec()));

    let outcome = run(
        &plan,
        &mut manifest,
        &[],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(
        outcome.failed(),
        0,
        "no remove is attempted, so the armed failure never fires"
    );
    assert_eq!(outcome.artifacts_written, 1);
    assert_eq!(fs.read_file("Album/cover.jpg").unwrap(), b"new");
    assert_eq!(
        manifest.get("a").unwrap().cover_jpg.as_ref().unwrap().hash,
        "h2"
    );
    // The live directory is untouched by prune.
    assert!(fs.has_dir("Album"));
}
