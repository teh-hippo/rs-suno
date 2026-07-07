use super::*;

#[test]
fn write_artifact_fetches_writes_and_updates_manifest() {
    // The owning entry exists (its audio was kept this run); WriteArtifact
    // fetches the source, writes the sidecar, and records it on the entry.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.mp3", AudioFormat::Mp3));
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::CoverJpg,
            path: "a/cover.jpg".to_owned(),
            source_url: "https://art.suno.ai/a/large.jpg".to_owned(),
            hash: "h1".to_owned(),
            owner_id: "a".to_owned(),
            content: None,
        }],
    };
    let http = ScriptedHttp::new().route("a/large.jpg", Reply::ok(b"jpg-bytes".to_vec()));
    let fs = MemFs::new();

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

    assert_eq!(outcome.artifacts_written, 1);
    assert_eq!(outcome.failed(), 0);
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(fs.read_file("a/cover.jpg").unwrap(), b"jpg-bytes");
    assert_eq!(
        manifest.get("a").unwrap().cover_jpg,
        Some(ArtifactState {
            path: "a/cover.jpg".to_owned(),
            hash: "h1".to_owned(),
        })
    );
}

#[test]
fn write_text_sidecar_records_slot_with_no_network_fetch() {
    // A generated text sidecar carries its body inline, so it is written
    // verbatim with NO HTTP fetch and the details slot records its state.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.mp3", AudioFormat::Mp3));
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::DetailsTxt,
            path: "a.details.txt".to_owned(),
            source_url: String::new(),
            hash: "dh".to_owned(),
            owner_id: "a".to_owned(),
            content: Some("Title: A\n".to_owned()),
        }],
    };
    // An empty HTTP script: any fetch would fail, proving none happens.
    let http = ScriptedHttp::new();
    let fs = MemFs::new();

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

    assert_eq!(outcome.artifacts_written, 1);
    assert_eq!(outcome.failed(), 0);
    assert_eq!(fs.read_file("a.details.txt").unwrap(), b"Title: A\n");
    assert_eq!(
        manifest.get("a").unwrap().details_txt,
        Some(ArtifactState {
            path: "a.details.txt".to_owned(),
            hash: "dh".to_owned(),
        })
    );
}

#[test]
fn write_lyrics_sidecar_relocation_removes_old_file() {
    // The audio moved, so the lyrics sidecar is re-emitted at the new path;
    // the executor writes the new file and prunes the stale one.
    let mut manifest = Manifest::new();
    let mut e = entry("old/a.flac", AudioFormat::Flac);
    e.lyrics_txt = Some(ArtifactState {
        path: "old/a.lyrics.txt".to_owned(),
        hash: "lh".to_owned(),
    });
    manifest.insert("a", e);
    let fs = MemFs::new()
        .with_file("old/a.flac", b"AUDIO".to_vec())
        .with_file("old/a.lyrics.txt", b"old words\n".to_vec());
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::LyricsTxt,
            path: "new/a.lyrics.txt".to_owned(),
            source_url: String::new(),
            hash: "lh".to_owned(),
            owner_id: "a".to_owned(),
            content: Some("new words\n".to_owned()),
        }],
    };

    let outcome = run(
        &plan,
        &mut manifest,
        &[],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.failed(), 0);
    assert_eq!(fs.read_file("new/a.lyrics.txt").unwrap(), b"new words\n");
    assert!(!fs.exists("old/a.lyrics.txt"));
    assert_eq!(
        manifest.get("a").unwrap().lyrics_txt.as_ref().unwrap().path,
        "new/a.lyrics.txt"
    );
}

#[test]
fn sidecar_path_swap_never_deletes_a_file_written_this_run() {
    // Two clips swap sidecar paths in one run (A: x -> y while B: y -> x).
    // Each write's inline old-path cleanup must skip a path another action
    // writes this run, or the second write would delete the first's freshly
    // written file (issue #76). The guard is kind-agnostic; lyrics stands in
    // for every sidecar, including the .mp4 video.
    let mut manifest = Manifest::new();
    let mut a = entry("a.flac", AudioFormat::Flac);
    a.lyrics_txt = Some(ArtifactState {
        path: "x.lyrics.txt".to_owned(),
        hash: "ah".to_owned(),
    });
    manifest.insert("a", a);
    let mut b = entry("b.flac", AudioFormat::Flac);
    b.lyrics_txt = Some(ArtifactState {
        path: "y.lyrics.txt".to_owned(),
        hash: "bh".to_owned(),
    });
    manifest.insert("b", b);
    let fs = MemFs::new()
        .with_file("a.flac", b"A".to_vec())
        .with_file("b.flac", b"B".to_vec())
        .with_file("x.lyrics.txt", b"A words\n".to_vec())
        .with_file("y.lyrics.txt", b"B words\n".to_vec());
    // A moves its sidecar x -> y; B moves its sidecar y -> x (the swap).
    let plan = Plan {
        actions: vec![
            Action::WriteArtifact {
                kind: ArtifactKind::LyricsTxt,
                path: "y.lyrics.txt".to_owned(),
                source_url: String::new(),
                hash: "ah".to_owned(),
                owner_id: "a".to_owned(),
                content: Some("A words\n".to_owned()),
            },
            Action::WriteArtifact {
                kind: ArtifactKind::LyricsTxt,
                path: "x.lyrics.txt".to_owned(),
                source_url: String::new(),
                hash: "bh".to_owned(),
                owner_id: "b".to_owned(),
                content: Some("B words\n".to_owned()),
            },
        ],
    };

    let outcome = run(
        &plan,
        &mut manifest,
        &[],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.failed(), 0);
    // Both freshly written files survive; neither cleanup clobbered the other.
    assert_eq!(fs.read_file("y.lyrics.txt").unwrap(), b"A words\n");
    assert_eq!(fs.read_file("x.lyrics.txt").unwrap(), b"B words\n");
    assert_eq!(
        manifest.get("a").unwrap().lyrics_txt.as_ref().unwrap().path,
        "y.lyrics.txt"
    );
    assert_eq!(
        manifest.get("b").unwrap().lyrics_txt.as_ref().unwrap().path,
        "x.lyrics.txt"
    );
}

#[test]
fn old_sidecar_kept_when_another_clip_still_references_it() {
    // A prior failed swap can leave two clips pointing at one path (A -> y and
    // B -> y). When B now moves y -> x, its cleanup must not delete y, which is
    // still A's live file (#76). tracked_paths counts two references to y, so
    // the removal is skipped even though y is not a write target this run.
    let mut manifest = Manifest::new();
    let mut a = entry("a.flac", AudioFormat::Flac);
    a.lyrics_txt = Some(ArtifactState {
        path: "y.lyrics.txt".to_owned(),
        hash: "ah".to_owned(),
    });
    manifest.insert("a", a);
    let mut b = entry("b.flac", AudioFormat::Flac);
    b.lyrics_txt = Some(ArtifactState {
        path: "y.lyrics.txt".to_owned(),
        hash: "bh".to_owned(),
    });
    manifest.insert("b", b);
    let fs = MemFs::new()
        .with_file("a.flac", b"A".to_vec())
        .with_file("b.flac", b"B".to_vec())
        .with_file("y.lyrics.txt", b"A words\n".to_vec());
    // Only B moves this run: y -> x. A is stable, so y is not a write target;
    // the tracked-reference count is what protects A's file.
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::LyricsTxt,
            path: "x.lyrics.txt".to_owned(),
            source_url: String::new(),
            hash: "bh".to_owned(),
            owner_id: "b".to_owned(),
            content: Some("B words\n".to_owned()),
        }],
    };

    let outcome = run(
        &plan,
        &mut manifest,
        &[],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.failed(), 0);
    assert!(
        fs.exists("y.lyrics.txt"),
        "A's live sidecar must not be deleted"
    );
    assert_eq!(fs.read_file("x.lyrics.txt").unwrap(), b"B words\n");
}

#[test]
fn shared_old_path_is_reclaimed_when_every_referencing_clip_moves_away() {
    // Two clips share one path (A -> s and B -> s, from a prior failed swap).
    // When BOTH move away this run, the path is no longer live, so the last
    // mover must reclaim it: it is neither kept as an orphan nor deleted while
    // still referenced. The dynamic reference count drops to zero only after
    // both moves, so exactly the final cleanup removes it (#76).
    let mut manifest = Manifest::new();
    let mut a = entry("a.flac", AudioFormat::Flac);
    a.lyrics_txt = Some(ArtifactState {
        path: "s.lyrics.txt".to_owned(),
        hash: "ah".to_owned(),
    });
    manifest.insert("a", a);
    let mut b = entry("b.flac", AudioFormat::Flac);
    b.lyrics_txt = Some(ArtifactState {
        path: "s.lyrics.txt".to_owned(),
        hash: "bh".to_owned(),
    });
    manifest.insert("b", b);
    let fs = MemFs::new()
        .with_file("a.flac", b"A".to_vec())
        .with_file("b.flac", b"B".to_vec())
        .with_file("s.lyrics.txt", b"shared\n".to_vec());
    let plan = Plan {
        actions: vec![
            Action::WriteArtifact {
                kind: ArtifactKind::LyricsTxt,
                path: "pa.lyrics.txt".to_owned(),
                source_url: String::new(),
                hash: "ah".to_owned(),
                owner_id: "a".to_owned(),
                content: Some("A words\n".to_owned()),
            },
            Action::WriteArtifact {
                kind: ArtifactKind::LyricsTxt,
                path: "pb.lyrics.txt".to_owned(),
                source_url: String::new(),
                hash: "bh".to_owned(),
                owner_id: "b".to_owned(),
                content: Some("B words\n".to_owned()),
            },
        ],
    };

    let outcome = run(
        &plan,
        &mut manifest,
        &[],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.failed(), 0);
    assert_eq!(fs.read_file("pa.lyrics.txt").unwrap(), b"A words\n");
    assert_eq!(fs.read_file("pb.lyrics.txt").unwrap(), b"B words\n");
    assert!(
        !fs.exists("s.lyrics.txt"),
        "the vacated shared path must be reclaimed, not orphaned"
    );
}

#[test]
fn write_text_sidecar_skipped_when_owner_audio_absent() {
    // A text sidecar for a clip with no manifest entry (its audio download
    // failed) must be skipped, never writing an untracked file.
    let plan = Plan {
        actions: vec![Action::WriteArtifact {
            kind: ArtifactKind::DetailsTxt,
            path: "gone.details.txt".to_owned(),
            source_url: String::new(),
            hash: "dh".to_owned(),
            owner_id: "gone".to_owned(),
            content: Some("Title: Gone\n".to_owned()),
        }],
    };
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.artifacts_written, 0);
    assert_eq!(outcome.skipped, 1);
    assert!(!fs.exists("gone.details.txt"));
    assert!(manifest.get("gone").is_none());
}

#[test]
fn delete_artifact_removes_file_and_clears_slot() {
    let fs = MemFs::new().with_file("a/cover.jpg", b"jpg".to_vec());
    let mut manifest = Manifest::new();
    let mut e = entry("a.mp3", AudioFormat::Mp3);
    e.cover_jpg = Some(ArtifactState {
        path: "a/cover.jpg".to_owned(),
        hash: "h1".to_owned(),
    });
    manifest.insert("a", e);
    let plan = Plan {
        actions: vec![Action::DeleteArtifact {
            kind: ArtifactKind::CoverJpg,
            path: "a/cover.jpg".to_owned(),
            owner_id: "a".to_owned(),
        }],
    };

    let outcome = run(
        &plan,
        &mut manifest,
        &[],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.artifacts_deleted, 1);
    assert!(!fs.exists("a/cover.jpg"));
    assert_eq!(manifest.get("a").unwrap().cover_jpg, None);
}

#[test]
fn delete_artifact_tolerates_already_absent_file() {
    // `remove` is idempotent, so co-deleting a sidecar that is already gone
    // is not a failure.
    let mut manifest = Manifest::new();
    let mut e = entry("a.mp3", AudioFormat::Mp3);
    e.cover_jpg = Some(ArtifactState {
        path: "a/cover.jpg".to_owned(),
        hash: "h1".to_owned(),
    });
    manifest.insert("a", e);
    let plan = Plan {
        actions: vec![Action::DeleteArtifact {
            kind: ArtifactKind::CoverJpg,
            path: "a/cover.jpg".to_owned(),
            owner_id: "a".to_owned(),
        }],
    };

    let outcome = run(
        &plan,
        &mut manifest,
        &[],
        &ScriptedHttp::new(),
        &MemFs::new(),
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.artifacts_deleted, 1);
    assert_eq!(outcome.failed(), 0);
    assert_eq!(manifest.get("a").unwrap().cover_jpg, None);
}

#[test]
fn write_artifact_http_failure_is_a_per_clip_failure_not_a_run_abort() {
    // A permanent 404 on one sidecar fetch is recorded as a per-clip failure;
    // the run continues and the following WriteArtifact still succeeds.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.mp3", AudioFormat::Mp3));
    manifest.insert("b", entry("b.mp3", AudioFormat::Mp3));
    let plan = Plan {
        actions: vec![
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "a/cover.jpg".to_owned(),
                source_url: "https://art.suno.ai/a/large.jpg".to_owned(),
                hash: "h1".to_owned(),
                owner_id: "a".to_owned(),
                content: None,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "b/cover.jpg".to_owned(),
                source_url: "https://art.suno.ai/b/large.jpg".to_owned(),
                hash: "h2".to_owned(),
                owner_id: "b".to_owned(),
                content: None,
            },
        ],
    };
    let http = ScriptedHttp::new()
        .route("a/large.jpg", Reply::status(404))
        .route("b/large.jpg", Reply::ok(b"jpg-b".to_vec()));
    let fs = MemFs::new();

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

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.failures[0].clip_id, "a");
    assert_eq!(outcome.artifacts_written, 1);
    // The failed sidecar left no file and no manifest record.
    assert!(!fs.exists("a/cover.jpg"));
    assert_eq!(manifest.get("a").unwrap().cover_jpg, None);
    // The following sidecar was written and recorded.
    assert_eq!(fs.read_file("b/cover.jpg").unwrap(), b"jpg-b");
    assert!(manifest.get("b").unwrap().cover_jpg.is_some());
}

#[test]
fn stranded_old_sidecar_removed_when_colliding_writer_fails() {
    // #142: clip A moves its cover shared -> a/cover.jpg (fetch succeeds);
    // clip B is planned to write the vacated `shared` path but its fetch
    // fails. The old-path cleanup is gated on COMMITTED writes, not planned
    // ones, so B's failed write no longer protects the stale file: A's old
    // `shared` copy is removed rather than left as an untracked orphan.
    let mut manifest = Manifest::new();
    let mut a = entry("a.mp3", AudioFormat::Mp3);
    a.cover_jpg = Some(ArtifactState {
        path: "shared/cover.jpg".to_owned(),
        hash: "ha".to_owned(),
    });
    manifest.insert("a", a);
    manifest.insert("b", entry("b.mp3", AudioFormat::Mp3));
    let fs = MemFs::new().with_file("shared/cover.jpg", b"old-shared".to_vec());
    let plan = Plan {
        actions: vec![
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "a/cover.jpg".to_owned(),
                source_url: "https://art.suno.ai/a/large.jpg".to_owned(),
                hash: "ha".to_owned(),
                owner_id: "a".to_owned(),
                content: None,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "shared/cover.jpg".to_owned(),
                source_url: "https://art.suno.ai/b/large.jpg".to_owned(),
                hash: "hb".to_owned(),
                owner_id: "b".to_owned(),
                content: None,
            },
        ],
    };
    let http = ScriptedHttp::new()
        .route("a/large.jpg", Reply::ok(b"jpg-a".to_vec()))
        .route("b/large.jpg", Reply::status(404));

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

    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.failures[0].clip_id, "b");
    // A's move committed; the vacated file is gone, not an orphan.
    assert_eq!(fs.read_file("a/cover.jpg").unwrap(), b"jpg-a");
    assert!(
        !fs.exists("shared/cover.jpg"),
        "the vacated file must be removed once the colliding writer failed"
    );
    assert_eq!(
        manifest.get("a").unwrap().cover_jpg.as_ref().unwrap().path,
        "a/cover.jpg"
    );
}

#[test]
fn committed_write_at_old_path_is_preserved() {
    // #142: clip B writes `shared` and commits BEFORE clip A vacates it
    // (A moves shared -> a/cover.jpg). A's cleanup sees `shared` in the
    // committed set and keeps B's freshly written file rather than deleting
    // it. This is the successful-collision case the guard must still protect.
    let mut manifest = Manifest::new();
    let mut a = entry("a.mp3", AudioFormat::Mp3);
    a.cover_jpg = Some(ArtifactState {
        path: "shared/cover.jpg".to_owned(),
        hash: "ha".to_owned(),
    });
    manifest.insert("a", a);
    manifest.insert("b", entry("b.mp3", AudioFormat::Mp3));
    let fs = MemFs::new().with_file("shared/cover.jpg", b"old-shared".to_vec());
    let plan = Plan {
        actions: vec![
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "shared/cover.jpg".to_owned(),
                source_url: "https://art.suno.ai/b/large.jpg".to_owned(),
                hash: "hb".to_owned(),
                owner_id: "b".to_owned(),
                content: None,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "a/cover.jpg".to_owned(),
                source_url: "https://art.suno.ai/a/large.jpg".to_owned(),
                hash: "ha".to_owned(),
                owner_id: "a".to_owned(),
                content: None,
            },
        ],
    };
    let http = ScriptedHttp::new()
        .route("b/large.jpg", Reply::ok(b"jpg-b".to_vec()))
        .route("a/large.jpg", Reply::ok(b"jpg-a".to_vec()));

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
    // B's committed write survives A's subsequent move; both files are present.
    assert_eq!(fs.read_file("shared/cover.jpg").unwrap(), b"jpg-b");
    assert_eq!(fs.read_file("a/cover.jpg").unwrap(), b"jpg-a");
    assert_eq!(
        manifest.get("b").unwrap().cover_jpg.as_ref().unwrap().path,
        "shared/cover.jpg"
    );
    assert_eq!(
        manifest.get("a").unwrap().cover_jpg.as_ref().unwrap().path,
        "a/cover.jpg"
    );
}

#[test]
fn write_artifact_is_skipped_when_the_owner_audio_is_absent() {
    // A clip whose Download fails leaves no manifest entry, so its following
    // WriteArtifact must not strand an untracked sidecar: it is skipped with
    // no fetch and no write. A following healthy clip still succeeds.
    let ca = clip("a");
    let plan = Plan {
        actions: vec![
            Action::Download {
                clip: ca.clone(),
                lineage: LineageContext::own_root(&ca),
                path: "a.mp3".to_owned(),
                format: AudioFormat::Mp3,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "a/cover.jpg".to_owned(),
                source_url: "https://art.suno.ai/a/large.jpg".to_owned(),
                hash: "h1".to_owned(),
                owner_id: "a".to_owned(),
                content: None,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "b/cover.jpg".to_owned(),
                source_url: "https://art.suno.ai/b/large.jpg".to_owned(),
                hash: "h2".to_owned(),
                owner_id: "b".to_owned(),
                content: None,
            },
        ],
    };
    // The Download's audio 404s (permanent), so no entry for "a" is created.
    let http = ScriptedHttp::new()
        .route("a.mp3", Reply::status(404))
        .route("a/large.jpg", Reply::ok(b"jpg-a".to_vec()))
        .route("b/large.jpg", Reply::ok(b"jpg-b".to_vec()));
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    // "b" already has audio (a prior-run clip), so its sidecar write proceeds.
    manifest.insert("b", entry("b.mp3", AudioFormat::Mp3));

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

    assert_eq!(outcome.status, RunStatus::Completed);
    // The audio download is the only failure; the orphan artifact is skipped.
    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.failures[0].clip_id, "a");
    assert_eq!(outcome.skipped, 1);
    // The orphan sidecar was neither fetched nor written, and left no record.
    assert_eq!(http.count("a/large.jpg"), 0);
    assert!(!fs.exists("a/cover.jpg"));
    assert!(manifest.get("a").is_none());
    // The healthy clip's sidecar still succeeded.
    assert_eq!(outcome.artifacts_written, 1);
    assert_eq!(fs.read_file("b/cover.jpg").unwrap(), b"jpg-b");
    assert!(manifest.get("b").unwrap().cover_jpg.is_some());
}
