use super::*;

#[test]
fn cover_move_renames_without_fetching() {
    // #141: a MoveArtifact relocates the cover with a local rename. The
    // ScriptedHttp has no route, so any fetch would fail the run; a clean
    // outcome proves the bytes were renamed, not re-downloaded.
    let mut manifest = Manifest::new();
    let mut e = entry("a.mp3", AudioFormat::Mp3);
    e.cover_jpg = Some(ArtifactState {
        path: "old/cover.jpg".to_owned(),
        hash: "h".to_owned(),
    });
    manifest.insert("a", e);
    let fs = MemFs::new().with_file("old/cover.jpg", b"JPGBYTES".to_vec());
    let plan = Plan {
        actions: vec![Action::MoveArtifact {
            kind: ArtifactKind::CoverJpg,
            from: "old/cover.jpg".to_owned(),
            to: "new/cover.jpg".to_owned(),
            source_url: "https://art.suno.ai/a/large.jpg".to_owned(),
            hash: "h".to_owned(),
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

    assert_eq!(outcome.failed(), 0);
    assert_eq!(outcome.renamed, 1, "counted as a rename, not a write");
    // Renamed in place: the new path carries the ORIGINAL bytes, old is gone.
    assert_eq!(fs.read_file("new/cover.jpg").unwrap(), b"JPGBYTES");
    assert!(!fs.exists("old/cover.jpg"));
    assert_eq!(
        manifest.get("a").unwrap().cover_jpg.as_ref().unwrap().path,
        "new/cover.jpg"
    );
}

#[test]
fn cover_move_falls_back_to_fetch_when_old_file_missing() {
    // #141: the old file vanished before commit, so the rename fails and the
    // executor fetches fresh bytes at the new path rather than failing.
    let mut manifest = Manifest::new();
    let mut e = entry("a.mp3", AudioFormat::Mp3);
    e.cover_jpg = Some(ArtifactState {
        path: "old/cover.jpg".to_owned(),
        hash: "h".to_owned(),
    });
    manifest.insert("a", e);
    let fs = MemFs::new(); // old/cover.jpg is absent.
    let http = ScriptedHttp::new().route("a/large.jpg", Reply::ok(b"FETCHED".to_vec()));
    let plan = Plan {
        actions: vec![Action::MoveArtifact {
            kind: ArtifactKind::CoverJpg,
            from: "old/cover.jpg".to_owned(),
            to: "new/cover.jpg".to_owned(),
            source_url: "https://art.suno.ai/a/large.jpg".to_owned(),
            hash: "h".to_owned(),
            owner_id: "a".to_owned(),
        }],
    };

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
    assert_eq!(fs.read_file("new/cover.jpg").unwrap(), b"FETCHED");
    assert_eq!(
        manifest.get("a").unwrap().cover_jpg.as_ref().unwrap().path,
        "new/cover.jpg"
    );
}

#[test]
fn cover_move_falls_back_when_source_co_referenced() {
    // Two clips' covers share old/cover.jpg after a prior failed swap. A move
    // for `a` must NOT rename the shared file away (that would strand `b`); it
    // falls back to a fetch, and `b`'s file survives.
    let mut manifest = Manifest::new();
    let mut a = entry("a.mp3", AudioFormat::Mp3);
    a.cover_jpg = Some(ArtifactState {
        path: "old/cover.jpg".to_owned(),
        hash: "h".to_owned(),
    });
    manifest.insert("a", a);
    let mut b = entry("b.mp3", AudioFormat::Mp3);
    b.cover_jpg = Some(ArtifactState {
        path: "old/cover.jpg".to_owned(),
        hash: "h".to_owned(),
    });
    manifest.insert("b", b);
    let fs = MemFs::new().with_file("old/cover.jpg", b"SHARED".to_vec());
    let http = ScriptedHttp::new().route("a/large.jpg", Reply::ok(b"FETCHED-A".to_vec()));
    // Only `a` moves this run: old/cover.jpg -> a/cover.jpg.
    let plan = Plan {
        actions: vec![Action::MoveArtifact {
            kind: ArtifactKind::CoverJpg,
            from: "old/cover.jpg".to_owned(),
            to: "a/cover.jpg".to_owned(),
            source_url: "https://art.suno.ai/a/large.jpg".to_owned(),
            hash: "h".to_owned(),
            owner_id: "a".to_owned(),
        }],
    };

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
    // `a` got a fresh fetched copy; `b`'s shared file is untouched.
    assert_eq!(fs.read_file("a/cover.jpg").unwrap(), b"FETCHED-A");
    assert_eq!(
        fs.read_file("old/cover.jpg").unwrap(),
        b"SHARED",
        "the co-referenced file must survive"
    );
}

#[test]
fn stem_move_renames_without_refetch() {
    // #141: a MoveStem relocates the raw stem with a rename; no route is set,
    // so a clean outcome proves it did not re-render or re-fetch.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac);
    e.stems.insert(
        "voc".to_owned(),
        ArtifactState {
            path: "old.stems/voc.mp3".to_owned(),
            hash: "h1".to_owned(),
        },
    );
    manifest.insert("a", e);
    let fs = MemFs::new().with_file("old.stems/voc.mp3", b"STEMBYTES".to_vec());
    let plan = Plan {
        actions: vec![Action::MoveStem {
            clip_id: "a".to_owned(),
            key: "voc".to_owned(),
            stem_id: "voc".to_owned(),
            from: "old.stems/voc.mp3".to_owned(),
            to: "new.stems/voc.mp3".to_owned(),
            source_url: "https://cdn1.suno.ai/voc.mp3".to_owned(),
            format: StemFormat::Mp3,
            hash: "h1".to_owned(),
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
    assert_eq!(outcome.renamed, 1);
    assert_eq!(fs.read_file("new.stems/voc.mp3").unwrap(), b"STEMBYTES");
    assert!(!fs.exists("old.stems/voc.mp3"));
    assert_eq!(
        manifest.get("a").unwrap().stems.get("voc").unwrap().path,
        "new.stems/voc.mp3"
    );
}

#[test]
fn stem_move_falls_back_to_fetch_when_source_co_referenced() {
    // Two clips' stems share shared.stems/voc.mp3 after a partially-failed
    // swap (the file holds `a`'s bytes). When `b` moves it, move_stem must NOT
    // rename the shared file under `b`'s hash (that records `a`'s bytes as
    // `b`'s); it falls back to a fetch of `b`'s correct bytes.
    let mut manifest = Manifest::new();
    let mut a = entry("a.flac", AudioFormat::Flac);
    a.stems.insert(
        "voc".to_owned(),
        ArtifactState {
            path: "shared.stems/voc.mp3".to_owned(),
            hash: "h".to_owned(),
        },
    );
    manifest.insert("a", a);
    let mut b = entry("b.flac", AudioFormat::Flac);
    b.stems.insert(
        "voc".to_owned(),
        ArtifactState {
            path: "shared.stems/voc.mp3".to_owned(),
            hash: "h".to_owned(),
        },
    );
    manifest.insert("b", b);
    let fs = MemFs::new().with_file("shared.stems/voc.mp3", b"A-STEM".to_vec());
    let http = ScriptedHttp::new().route("bvoc.mp3", Reply::ok(b"B-STEM".to_vec()));
    let plan = Plan {
        actions: vec![Action::MoveStem {
            clip_id: "b".to_owned(),
            key: "voc".to_owned(),
            stem_id: "bvoc".to_owned(),
            from: "shared.stems/voc.mp3".to_owned(),
            to: "b.stems/voc.mp3".to_owned(),
            source_url: "https://cdn1.suno.ai/bvoc.mp3".to_owned(),
            format: StemFormat::Mp3,
            hash: "h".to_owned(),
        }],
    };

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
    // b's new stem carries b's freshly fetched bytes, never a's renamed bytes.
    assert_eq!(fs.read_file("b.stems/voc.mp3").unwrap(), b"B-STEM");
    assert_eq!(
        fs.read_file("shared.stems/voc.mp3").unwrap(),
        b"A-STEM",
        "the co-referenced stem must survive"
    );
}

#[test]
fn write_stem_keeps_shared_stem_when_co_referenced() {
    // Two clips share shared.stems/voc.mp3 after a prior partially-failed swap.
    // When `b` writes to a new path, write_stem must NOT remove the shared file;
    // clip `a` still references it and its stem must survive.
    let mut manifest = Manifest::new();
    let mut a = entry("a.flac", AudioFormat::Flac);
    a.stems.insert(
        "voc".to_owned(),
        ArtifactState {
            path: "shared.stems/voc.mp3".to_owned(),
            hash: "h".to_owned(),
        },
    );
    manifest.insert("a", a);
    let mut b = entry("b.flac", AudioFormat::Flac);
    b.stems.insert(
        "voc".to_owned(),
        ArtifactState {
            path: "shared.stems/voc.mp3".to_owned(),
            hash: "h".to_owned(),
        },
    );
    manifest.insert("b", b);
    let fs = MemFs::new().with_file("shared.stems/voc.mp3", b"A-STEM".to_vec());
    let http = ScriptedHttp::new().route("bvoc.mp3", Reply::ok(b"B-STEM".to_vec()));
    let plan = Plan {
        actions: vec![Action::WriteStem {
            clip_id: "b".to_owned(),
            key: "voc".to_owned(),
            stem_id: "bvoc".to_owned(),
            path: "b.stems/voc.mp3".to_owned(),
            source_url: "https://cdn1.suno.ai/bvoc.mp3".to_owned(),
            format: StemFormat::Mp3,
            hash: "bh".to_owned(),
        }],
    };

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
    assert_eq!(fs.read_file("b.stems/voc.mp3").unwrap(), b"B-STEM");
    assert_eq!(
        fs.read_file("shared.stems/voc.mp3").unwrap(),
        b"A-STEM",
        "the co-referenced stem must survive"
    );
}

#[test]
fn co_delete_executes_audio_delete_then_artifact_delete() {
    // The plan orders the audio Delete before its sidecar DeleteArtifact.
    // The audio delete removes the manifest entry; the sidecar delete then
    // removes the file and tolerates the now-absent entry.
    let fs = MemFs::new()
        .with_file("gone.mp3", b"DATA".to_vec())
        .with_file("gone/cover.jpg", b"jpg".to_vec());
    let mut manifest = Manifest::new();
    let mut e = entry("gone.mp3", AudioFormat::Mp3);
    e.cover_jpg = Some(ArtifactState {
        path: "gone/cover.jpg".to_owned(),
        hash: "h1".to_owned(),
    });
    manifest.insert("gone", e);
    let plan = Plan {
        actions: vec![
            Action::Delete {
                path: "gone.mp3".to_owned(),
                clip_id: "gone".to_owned(),
            },
            Action::DeleteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "gone/cover.jpg".to_owned(),
                owner_id: "gone".to_owned(),
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

    assert_eq!(outcome.deleted, 1);
    assert_eq!(outcome.artifacts_deleted, 1);
    assert_eq!(outcome.failed(), 0);
    assert!(!fs.exists("gone.mp3"));
    assert!(!fs.exists("gone/cover.jpg"));
    assert!(manifest.get("gone").is_none());
}

#[test]
fn write_stem_mp3_stores_raw_and_records_slot() {
    // An MP3 stem is downloaded straight from its CDN url and stored verbatim
    // (no transcode, no WAV render): the bytes land at the `.mp3` path and the
    // keyed slot records the path and hash.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac));
    let plan = Plan {
        actions: vec![Action::WriteStem {
            clip_id: "a".to_owned(),
            key: "voc".to_owned(),
            stem_id: "voc".to_owned(),
            path: "a.stems/a - Vocals [voc].mp3".to_owned(),
            source_url: "https://cdn1.suno.ai/voc.mp3".to_owned(),
            format: StemFormat::Mp3,
            hash: "vh".to_owned(),
        }],
    };
    let http = ScriptedHttp::new().route("voc.mp3", Reply::ok(b"stem-bytes".to_vec()));
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
    // Bytes are stored exactly as delivered (no transcode applied).
    assert_eq!(
        fs.read_file("a.stems/a - Vocals [voc].mp3").unwrap(),
        b"stem-bytes"
    );
    // An MP3 stem never renders WAV: no convert_wav, no generation.
    assert_eq!(http.count("convert_wav"), 0);
    assert_eq!(http.count("/api/gen/"), 0);
    assert_eq!(
        manifest.get("a").unwrap().stems.get("voc"),
        Some(&ArtifactState {
            path: "a.stems/a - Vocals [voc].mp3".to_owned(),
            hash: "vh".to_owned(),
        })
    );
}

#[test]
fn write_stem_wav_renders_via_convert_wav_and_stores_raw() {
    // A WAV stem (the default) renders the stem clip's lossless WAV through the
    // free convert_wav flow keyed on the stem id, then downloads and stores it
    // RAW as `.wav` — it is NEVER transcoded to FLAC, even for a FLAC song.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac));
    let plan = Plan {
        actions: vec![Action::WriteStem {
            clip_id: "a".to_owned(),
            key: "voc".to_owned(),
            stem_id: "stemvoc".to_owned(),
            path: "a.stems/a - Vocals [stemvoc].wav".to_owned(),
            source_url: "https://cdn1.suno.ai/stemvoc.mp3".to_owned(),
            format: StemFormat::Wav,
            hash: "vh".to_owned(),
        }],
    };
    // wav_file is not ready on the first poll, so the flow POSTs convert_wav
    // (free) and polls again — exactly the main FLAC/WAV render path.
    let http = ScriptedHttp::new()
        .with_auth()
        .route_seq(
            "stemvoc/wav_file/",
            vec![
                Reply::json("{}"),
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/stemvoc.wav"}"#),
            ],
        )
        .route("stemvoc/convert_wav/", Reply::status(200))
        .route("stemvoc.wav", Reply::ok(b"RIFFwav-bytes".to_vec()));
    let fs = MemFs::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &small_poll(),
    );

    assert_eq!(outcome.artifacts_written, 1);
    assert_eq!(outcome.failed(), 0);
    // The rendered WAV is stored verbatim; ffmpeg (WAV->FLAC) is never invoked,
    // so the stored bytes are the raw WAV, not a FLAC transcode.
    assert_eq!(
        fs.read_file("a.stems/a - Vocals [stemvoc].wav").unwrap(),
        b"RIFFwav-bytes"
    );
    assert!(!fs.exists("a.stems/a - Vocals [stemvoc].flac"));
    // The free WAV render ran; no credit-spending generation endpoint did.
    assert_eq!(http.count("convert_wav"), 1);
    assert_eq!(http.count("stem_task"), 0);
    assert_eq!(http.count("separate"), 0);
    assert_eq!(
        manifest.get("a").unwrap().stems.get("voc").unwrap().path,
        "a.stems/a - Vocals [stemvoc].wav"
    );
}

#[test]
fn write_stem_is_skipped_when_owner_audio_is_absent() {
    // No owning manifest entry (audio failed or never existed) => skip with
    // no fetch and no write, so a stem is never stranded without its song.
    let mut manifest = Manifest::new();
    let plan = Plan {
        actions: vec![Action::WriteStem {
            clip_id: "ghost".to_owned(),
            key: "voc".to_owned(),
            stem_id: "voc".to_owned(),
            path: "ghost.stems/voc.mp3".to_owned(),
            source_url: "https://cdn1.suno.ai/voc.mp3".to_owned(),
            format: StemFormat::Mp3,
            hash: "vh".to_owned(),
        }],
    };
    // Empty HTTP script: any fetch would error, proving none happens.
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

    assert_eq!(outcome.skipped, 1);
    assert_eq!(outcome.artifacts_written, 0);
    assert_eq!(outcome.failed(), 0);
    assert!(!fs.exists("ghost.stems/voc.mp3"));
}

#[test]
fn write_stem_relocates_the_old_file_on_a_path_move() {
    // The song was renamed, so the stem moves: the new file is written and the
    // stale copy at the previously tracked path is removed (moved, not orphaned).
    let fs = MemFs::new().with_file("old.stems/voc.mp3", b"old".to_vec());
    let mut manifest = Manifest::new();
    let mut e = entry("new.flac", AudioFormat::Flac);
    e.stems.insert(
        "voc".to_owned(),
        ArtifactState {
            path: "old.stems/voc.mp3".to_owned(),
            hash: "vh".to_owned(),
        },
    );
    manifest.insert("a", e);
    let plan = Plan {
        actions: vec![Action::WriteStem {
            clip_id: "a".to_owned(),
            key: "voc".to_owned(),
            stem_id: "voc".to_owned(),
            path: "new.stems/voc.mp3".to_owned(),
            source_url: "https://cdn1.suno.ai/voc.mp3".to_owned(),
            format: StemFormat::Mp3,
            hash: "vh".to_owned(),
        }],
    };
    let http = ScriptedHttp::new().route("voc.mp3", Reply::ok(b"new".to_vec()));

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
    assert!(fs.exists("new.stems/voc.mp3"));
    assert!(
        !fs.exists("old.stems/voc.mp3"),
        "the old stem is moved, not left behind"
    );
    assert_eq!(
        manifest.get("a").unwrap().stems.get("voc").unwrap().path,
        "new.stems/voc.mp3"
    );
}

#[test]
fn delete_stem_removes_file_and_clears_slot() {
    let fs = MemFs::new().with_file("a.stems/voc.mp3", b"stem".to_vec());
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac);
    e.stems.insert(
        "voc".to_owned(),
        ArtifactState {
            path: "a.stems/voc.mp3".to_owned(),
            hash: "vh".to_owned(),
        },
    );
    manifest.insert("a", e);
    let plan = Plan {
        actions: vec![Action::DeleteStem {
            clip_id: "a".to_owned(),
            key: "voc".to_owned(),
            path: "a.stems/voc.mp3".to_owned(),
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
    assert!(!fs.exists("a.stems/voc.mp3"));
    assert!(manifest.get("a").unwrap().stems.is_empty());
}

#[test]
fn co_deleting_the_last_stem_prunes_the_stems_folder() {
    // Deleting a song co-deletes its stems; the emptied `.stems` folder is
    // pruned by the end-of-run sweep, so it can never be orphaned.
    let fs = MemFs::new()
        .with_file("song.flac", b"DATA".to_vec())
        .with_file("song.stems/voc.mp3", b"stem".to_vec());
    assert!(fs.has_dir("song.stems"));
    let mut manifest = Manifest::new();
    let mut e = entry("song.flac", AudioFormat::Flac);
    e.stems.insert(
        "voc".to_owned(),
        ArtifactState {
            path: "song.stems/voc.mp3".to_owned(),
            hash: "vh".to_owned(),
        },
    );
    manifest.insert("a", e);
    let plan = Plan {
        actions: vec![
            Action::Delete {
                path: "song.flac".to_owned(),
                clip_id: "a".to_owned(),
            },
            Action::DeleteStem {
                clip_id: "a".to_owned(),
                key: "voc".to_owned(),
                path: "song.stems/voc.mp3".to_owned(),
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

    assert_eq!(outcome.deleted, 1);
    assert_eq!(outcome.artifacts_deleted, 1);
    assert!(!fs.exists("song.flac"));
    assert!(!fs.exists("song.stems/voc.mp3"));
    assert!(
        !fs.has_dir("song.stems"),
        "the emptied .stems folder is pruned"
    );
    assert!(manifest.get("a").is_none());
}

#[test]
fn full_stems_mirror_mp3_is_get_only_with_zero_gen_traffic() {
    // End-to-end #100 path with MP3 stems: list a clip's existing stems (free
    // GET over the live page-count + 0-indexed page shape), reconcile them into
    // WriteStem actions, and execute (download) them. With MP3 the whole flow
    // is GET-only and touches NO `/api/gen/` endpoint at all.
    let http = ScriptedHttp::new()
            .with_auth()
            .route("clip1/stems/pages", Reply::json(r#"{"pages": 1}"#))
            .route(
                "clip1/stems?page=0",
                Reply::json(
                    r#"{"stems":[
                        {"id":"s1","title":"Song (Vocals)","status":"complete","audio_url":"https://cdn1.suno.ai/s1.mp3"},
                        {"id":"s2","title":"Song (Drums)","status":"complete","audio_url":"https://cdn1.suno.ai/s2.mp3"}
                    ]}"#,
                ),
            )
            .route("s1.mp3", Reply::ok(b"vocals-bytes".to_vec()))
            .route("s2.mp3", Reply::ok(b"drums-bytes".to_vec()));

    // List the existing stems through the client (GET-only, free).
    let auth = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth.authenticate(&http)).unwrap();
    let client = SunoClient::new(auth, RecordingClock::new());
    let (stems, complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();
    assert!(complete);
    assert_eq!(stems.len(), 2);
    assert_eq!(stems[0].label, "Vocals");

    // Reconcile the listed MP3 stems into a plan (audio already present -> Skip).
    let mut manifest = Manifest::new();
    manifest.insert("clip1", entry("clip1.flac", AudioFormat::Flac));
    let desired_stems: Vec<crate::reconcile::DesiredStem> = stems
        .iter()
        .map(|s| crate::reconcile::DesiredStem {
            key: s.id.clone(),
            stem_id: s.id.clone(),
            path: format!("clip1.stems/{}.mp3", s.id),
            source_url: s.url.clone(),
            format: StemFormat::Mp3,
            hash: crate::art_url_hash(&s.url),
        })
        .collect();
    let d = Desired {
        path: "clip1.flac".to_owned(),
        stems: Some(desired_stems),
        ..desired(clip("clip1"), AudioFormat::Flac)
    };
    let local: HashMap<String, crate::reconcile::LocalFile> = [(
        "clip1".to_owned(),
        crate::reconcile::LocalFile {
            exists: true,
            size: 100,
        },
    )]
    .into_iter()
    .collect();
    let sources = [crate::reconcile::SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: true,
    }];
    let plan = crate::reconcile::reconcile(&manifest, std::slice::from_ref(&d), &local, &sources);
    assert_eq!(plan.stem_writes(), 2);

    let fs = MemFs::new();
    let outcome = run(
        &plan,
        &mut manifest,
        std::slice::from_ref(&d),
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.artifacts_written, 2, "both stems downloaded");
    assert_eq!(fs.read_file("clip1.stems/s1.mp3").unwrap(), b"vocals-bytes");
    assert_eq!(fs.read_file("clip1.stems/s2.mp3").unwrap(), b"drums-bytes");
    // The MP3 mirror path never touches any /api/gen/ endpoint (no render, no
    // generation, no separation).
    assert_eq!(http.count("/api/gen/"), 0);
    assert_eq!(http.count("stem_task"), 0);
    assert_eq!(http.count("separate"), 0);
    assert_eq!(http.count("generate"), 0);
    // No stem is ever written as FLAC.
    assert!(!fs.exists("clip1.stems/s1.flac"));
}

#[test]
fn full_stems_mirror_wav_default_renders_free_wav_and_no_generation() {
    // End-to-end #100 path with WAV stems (the default): each stem's lossless
    // WAV is rendered through the FREE convert_wav flow and stored RAW as
    // `.wav`. The mirror makes NO credit-spending generation POST.
    let http = ScriptedHttp::new()
            .with_auth()
            .route("clip1/stems/pages", Reply::json(r#"{"pages": 1}"#))
            .route(
                "clip1/stems?page=0",
                Reply::json(
                    r#"{"stems":[
                        {"id":"s1","title":"Song (Vocals)","status":"complete","audio_url":"https://cdn1.suno.ai/s1.mp3"},
                        {"id":"s2","title":"Song (Drums)","status":"complete","audio_url":"https://cdn1.suno.ai/s2.mp3"}
                    ]}"#,
                ),
            )
            // Each stem's WAV is already rendered, so wav_file returns the url and
            // no convert_wav POST is even needed (still free either way).
            .route(
                "s1/wav_file/",
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/s1.wav"}"#),
            )
            .route(
                "s2/wav_file/",
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/s2.wav"}"#),
            )
            .route("s1.wav", Reply::ok(b"RIFFvocals".to_vec()))
            .route("s2.wav", Reply::ok(b"RIFFdrums".to_vec()));

    let auth = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth.authenticate(&http)).unwrap();
    let client = SunoClient::new(auth, RecordingClock::new());
    let (stems, _complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();

    let mut manifest = Manifest::new();
    manifest.insert("clip1", entry("clip1.flac", AudioFormat::Flac));
    let desired_stems: Vec<crate::reconcile::DesiredStem> = stems
        .iter()
        .map(|s| crate::reconcile::DesiredStem {
            key: s.id.clone(),
            stem_id: s.id.clone(),
            path: format!("clip1.stems/{}.wav", s.id),
            source_url: s.url.clone(),
            format: StemFormat::Wav,
            hash: crate::art_url_hash(&s.url),
        })
        .collect();
    let d = Desired {
        path: "clip1.flac".to_owned(),
        stems: Some(desired_stems),
        ..desired(clip("clip1"), AudioFormat::Flac)
    };
    let local: HashMap<String, crate::reconcile::LocalFile> = [(
        "clip1".to_owned(),
        crate::reconcile::LocalFile {
            exists: true,
            size: 100,
        },
    )]
    .into_iter()
    .collect();
    let sources = [crate::reconcile::SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: true,
    }];
    let plan = crate::reconcile::reconcile(&manifest, std::slice::from_ref(&d), &local, &sources);

    let fs = MemFs::new();
    let outcome = run(
        &plan,
        &mut manifest,
        std::slice::from_ref(&d),
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &small_poll(),
    );

    assert_eq!(outcome.artifacts_written, 2);
    // Stems are stored RAW as WAV (no FLAC transcode, even for a FLAC song).
    assert_eq!(fs.read_file("clip1.stems/s1.wav").unwrap(), b"RIFFvocals");
    assert_eq!(fs.read_file("clip1.stems/s2.wav").unwrap(), b"RIFFdrums");
    assert!(!fs.exists("clip1.stems/s1.flac"));
    // No credit-spending generation/separation endpoint is ever hit.
    assert_eq!(http.count("stem_task"), 0);
    assert_eq!(http.count("separate"), 0);
    assert_eq!(http.count("generate"), 0);
}

#[test]
fn move_artifact_toctou_cover_fails_per_clip_without_delete_or_abort() {
    // #355: a synthesised MoveArtifact (empty source_url) whose old file has
    // vanished by commit time. The in-place rename fails, and the fetch fallback
    // has no URL: it is a per-clip Fail, never a delete, panic, or run abort. A
    // sibling clip's move still succeeds, proving the run continued.
    let mut manifest = Manifest::new();
    let mut a = entry("New.flac", AudioFormat::Flac);
    a.cover_jpg = Some(ArtifactState {
        path: "Old.jpg".to_owned(),
        hash: "ah".to_owned(),
    });
    manifest.insert("a", a);
    let mut b = entry("BNew.flac", AudioFormat::Flac);
    b.cover_jpg = Some(ArtifactState {
        path: "BOld.jpg".to_owned(),
        hash: "bh".to_owned(),
    });
    manifest.insert("b", b);
    // Only b's old cover is on disk, so b renames cleanly; a's is gone.
    let fs = MemFs::new().with_file("BOld.jpg", b"BCOVER".to_vec());
    let plan = Plan {
        actions: vec![
            Action::MoveArtifact {
                kind: ArtifactKind::CoverJpg,
                from: "Old.jpg".to_owned(),
                to: "New.jpg".to_owned(),
                source_url: String::new(),
                hash: "ah".to_owned(),
                owner_id: "a".to_owned(),
            },
            Action::MoveArtifact {
                kind: ArtifactKind::CoverJpg,
                from: "BOld.jpg".to_owned(),
                to: "BNew.jpg".to_owned(),
                source_url: String::new(),
                hash: "bh".to_owned(),
                owner_id: "b".to_owned(),
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
        &small_poll(),
    );

    assert_eq!(outcome.failed(), 1, "only clip a fails, per-clip");
    assert_eq!(outcome.failures[0].clip_id, "a");
    assert_eq!(
        outcome.status,
        RunStatus::Completed,
        "a vanished old file never aborts the run"
    );
    assert_eq!(outcome.artifacts_deleted, 0, "no delete on a failed move");
    // a's slot is untouched (still the old path); b relocated cleanly.
    assert_eq!(
        manifest.get("a").unwrap().cover_jpg.as_ref().unwrap().path,
        "Old.jpg"
    );
    assert_eq!(outcome.renamed, 1, "b's move still succeeded");
    assert_eq!(fs.read_file("BNew.jpg").unwrap(), b"BCOVER");
}

#[test]
fn move_artifact_toctou_text_sidecar_permanent_fails() {
    // A synthesised MoveArtifact for a text kind (details) whose old file
    // vanished: the fetch fallback rejects a text kind outright
    // (permanent_fail), so it is a per-clip Fail with no delete and no abort.
    let mut manifest = Manifest::new();
    let mut a = entry("New.flac", AudioFormat::Flac);
    a.details_txt = Some(ArtifactState {
        path: "Old.details.txt".to_owned(),
        hash: "dh".to_owned(),
    });
    manifest.insert("a", a);
    let fs = MemFs::new(); // Old.details.txt is absent.
    let plan = Plan {
        actions: vec![Action::MoveArtifact {
            kind: ArtifactKind::DetailsTxt,
            from: "Old.details.txt".to_owned(),
            to: "New.details.txt".to_owned(),
            source_url: String::new(),
            hash: "dh".to_owned(),
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
        &small_poll(),
    );

    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.artifacts_deleted, 0);
    assert!(!fs.exists("New.details.txt"), "nothing was written");
    assert_eq!(
        manifest
            .get("a")
            .unwrap()
            .details_txt
            .as_ref()
            .unwrap()
            .path,
        "Old.details.txt",
        "the slot is untouched after a failed move"
    );
}

#[test]
fn move_stem_toctou_fails_per_clip_without_delete_or_abort() {
    // #355: a synthesised MoveStem (empty source_url and stem_id) whose old file
    // has vanished. The rename fails and the fetch fallback has no URL: a
    // per-clip Fail, never a delete, panic, or run abort.
    let mut manifest = Manifest::new();
    let mut e = entry("New.flac", AudioFormat::Flac);
    e.stems.insert(
        "voc".to_owned(),
        ArtifactState {
            path: "Old.stems/voc.mp3".to_owned(),
            hash: "h1".to_owned(),
        },
    );
    manifest.insert("a", e);
    let fs = MemFs::new(); // Old.stems/voc.mp3 is absent.
    let plan = Plan {
        actions: vec![Action::MoveStem {
            clip_id: "a".to_owned(),
            key: "voc".to_owned(),
            stem_id: String::new(),
            from: "Old.stems/voc.mp3".to_owned(),
            to: "New.stems/voc.mp3".to_owned(),
            source_url: String::new(),
            format: StemFormat::Mp3,
            hash: "h1".to_owned(),
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
        &small_poll(),
    );

    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.failures[0].clip_id, "a");
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.artifacts_deleted, 0);
    assert!(!fs.exists("New.stems/voc.mp3"));
    assert_eq!(
        manifest.get("a").unwrap().stems.get("voc").unwrap().path,
        "Old.stems/voc.mp3",
        "the stem slot is untouched after a failed move"
    );
}
