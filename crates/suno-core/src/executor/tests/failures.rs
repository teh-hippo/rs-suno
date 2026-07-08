use super::*;

#[test]
fn failed_write_leaves_the_prior_file_intact() {
    let c = clip("f");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new().route("f.mp3", Reply::ok(b"new-body".to_vec()));
    let fs = MemFs::new()
        .with_file("f.mp3", b"OLD-CONTENT".to_vec())
        .fail_write("f.mp3");
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.downloaded, 0);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(fs.read_file("f.mp3").unwrap(), b"OLD-CONTENT");
    assert!(manifest.get("f").is_none());
}

#[test]
fn size_mismatch_after_write_is_a_failure() {
    let c = clip("g");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new().route("g.mp3", Reply::ok(b"body".to_vec()));
    let fs = MemFs::new().corrupt_write("g.mp3");
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.downloaded, 0);
    assert_eq!(outcome.failed(), 1);
    assert!(outcome.failures[0].reason.contains("expected"));
    assert!(manifest.get("g").is_none());
}

#[test]
fn transient_failure_is_retried_then_skipped() {
    let c = clip("h");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new().route("h.mp3", Reply::status(500));
    let fs = MemFs::new();
    let clock = RecordingClock::new();
    let opts = ExecOptions {
        max_retries: 2,
        ..ExecOptions::default()
    };
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &clock,
        &opts,
    );

    assert_eq!(outcome.downloaded, 0);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(http.count("h.mp3"), 3);
    assert_eq!(clock.sleeps().len(), 2);
}

#[test]
fn truncated_download_is_retried_then_succeeds() {
    let c = clip("i");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new().route_seq(
        "i.mp3",
        vec![
            Reply::ok(b"short".to_vec()).with_content_length(999),
            Reply::ok(b"good-body".to_vec()),
        ],
    );
    let fs = MemFs::new();
    let clock = RecordingClock::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &clock,
        &ExecOptions::default(),
    );

    assert_eq!(outcome.downloaded, 1);
    assert_eq!(http.count("i.mp3"), 2);
    assert_eq!(clock.sleeps().len(), 1);
}

#[test]
fn rate_limit_backs_off_using_retry_after() {
    let c = clip("j");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new().route_seq(
        "j.mp3",
        vec![
            Reply::status(429).with_retry_after(7),
            Reply::ok(b"body".to_vec()),
        ],
    );
    let fs = MemFs::new();
    let clock = RecordingClock::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &clock,
        &ExecOptions::default(),
    );

    assert_eq!(outcome.downloaded, 1);
    assert_eq!(clock.sleeps(), vec![Duration::from_secs(7)]);
}

#[test]
fn auth_failure_aborts_the_run() {
    let c1 = clip("k1");
    let c2 = clip("k2");
    let d1 = desired(c1.clone(), AudioFormat::Flac);
    let d2 = desired(c2.clone(), AudioFormat::Flac);
    let plan = Plan {
        actions: vec![
            Action::Download {
                clip: c1.clone(),
                lineage: LineageContext::own_root(&c1),
                path: d1.path.clone(),
                format: AudioFormat::Flac,
            },
            Action::Download {
                clip: c2.clone(),
                lineage: LineageContext::own_root(&c2),
                path: d2.path.clone(),
                format: AudioFormat::Flac,
            },
        ],
    };
    // The authenticated WAV-render endpoint rejects auth even after a JWT
    // refresh: that is a bad token, so the whole run aborts rather than
    // hammering every clip. A CDN media rejection, by contrast, does not.
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/wav_file/", Reply::status(401));
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d1, d2],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &small_poll(),
    );

    assert_eq!(outcome.status, RunStatus::AuthAborted);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.failures[0].clip_id, "k1");
    assert_eq!(outcome.downloaded, 0);
}

#[test]
fn disk_full_primary_write_aborts_the_run() {
    // Two MP3 downloads; the first write is out of space. That is systemic,
    // so the run aborts before the second is even attempted: exactly one
    // failure is recorded and its reason names the disk-full cause.
    let c1 = clip("d1");
    let c2 = clip("d2");
    let d1 = desired(c1.clone(), AudioFormat::Mp3);
    let d2 = desired(c2.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![
            Action::Download {
                clip: c1.clone(),
                lineage: LineageContext::own_root(&c1),
                path: d1.path.clone(),
                format: AudioFormat::Mp3,
            },
            Action::Download {
                clip: c2.clone(),
                lineage: LineageContext::own_root(&c2),
                path: d2.path.clone(),
                format: AudioFormat::Mp3,
            },
        ],
    };
    let http = ScriptedHttp::new()
        .route("d1.mp3", Reply::ok(b"body-1".to_vec()))
        .route("d2.mp3", Reply::ok(b"body-2".to_vec()));
    let fs = MemFs::new().fail_write_out_of_space("d1.mp3");
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d1, d2],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.status, RunStatus::DiskFull);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.failures[0].clip_id, "d1");
    assert!(outcome.failures[0].reason.contains("disk full"));
    assert_eq!(outcome.downloaded, 0);
    // The second clip was never fetched: the run aborted first.
    assert_eq!(http.count("d2.mp3"), 0);
    assert!(!fs.exists("d2.mp3"));
}

#[test]
fn disk_full_flac_transcode_aborts_the_run() {
    // The scratch disk fills during the FLAC re-encode; a WAV rendered, but
    // there is nowhere to stage the transcode, so the run aborts.
    let c1 = clip("d1");
    let c2 = clip("d2");
    let d1 = desired(c1.clone(), AudioFormat::Flac);
    let d2 = desired(c2.clone(), AudioFormat::Flac);
    let plan = Plan {
        actions: vec![
            Action::Download {
                clip: c1.clone(),
                lineage: LineageContext::own_root(&c1),
                path: d1.path.clone(),
                format: AudioFormat::Flac,
            },
            Action::Download {
                clip: c2.clone(),
                lineage: LineageContext::own_root(&c2),
                path: d2.path.clone(),
                format: AudioFormat::Flac,
            },
        ],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route(
            "/wav_file/",
            Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/d1.wav"}"#),
        )
        .route(".wav", Reply::ok(b"wav".to_vec()));
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d1, d2],
        &http,
        &fs,
        &StubFfmpeg::out_of_space(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.status, RunStatus::DiskFull);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.failures[0].clip_id, "d1");
    assert!(outcome.failures[0].reason.contains("disk full"));
    assert_eq!(outcome.downloaded, 0);
}

#[test]
fn disk_full_artifact_write_aborts_the_run() {
    // A sidecar write (not a primary download) also aborts on a full disk:
    // the owning audio is present, the cover fetch succeeds, but the sidecar
    // cannot be written.
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
    let fs = MemFs::new().fail_write_out_of_space("a/cover.jpg");

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

    assert_eq!(outcome.status, RunStatus::DiskFull);
    assert_eq!(outcome.failed(), 1);
    assert!(outcome.failures[0].reason.contains("disk full"));
    assert_eq!(outcome.artifacts_written, 0);
    // The sidecar slot was never recorded: the write failed before it.
    assert_eq!(manifest.get("a").unwrap().cover_jpg, None);
}

#[test]
fn disk_full_leaves_the_failed_clips_manifest_entry_unchanged() {
    // write_verify fails before any manifest insert, so a re-download that
    // hits a full disk leaves the prior entry (and file) exactly as it was.
    let c = clip("m");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new().route("m.mp3", Reply::ok(b"new-body".to_vec()));
    let fs = MemFs::new()
        .with_file("m.mp3", b"OLD-CONTENT".to_vec())
        .fail_write_out_of_space("m.mp3");
    let mut manifest = Manifest::new();
    let before = entry("m.mp3", AudioFormat::Mp3);
    manifest.insert("m", before.clone());

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.status, RunStatus::DiskFull);
    assert_eq!(manifest.get("m"), Some(&before));
    assert_eq!(fs.read_file("m.mp3").unwrap(), b"OLD-CONTENT");
}

#[test]
fn disk_full_unlink_aborts_the_run_before_a_later_delete() {
    // A full disk striking an *unlink* is systemic, exactly like a full-disk
    // write or transcode: removing the superseded old file during a reformat
    // fails with out-of-space, so the run aborts (DiskFull) rather than
    // skipping this one clip and proceeding to delete later ones. Ordering the
    // reformat before the orphan delete proves the abort stops the loop: the
    // orphan's file and manifest entry must survive untouched.
    let r = clip("r");
    let d = desired(r.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![
            Action::Reformat {
                clip: r.clone(),
                path: "r.mp3".to_owned(),
                from_path: "r.flac".to_owned(),
                from: AudioFormat::Flac,
                to: AudioFormat::Mp3,
            },
            Action::Delete {
                path: "z.mp3".to_owned(),
                clip_id: "z".to_owned(),
            },
        ],
    };
    let http = ScriptedHttp::new().route("r.mp3", Reply::ok(b"reformatted".to_vec()));
    // The reformat's new file writes fine; only the old-file unlink is full.
    let fs = MemFs::new()
        .with_file("r.flac", b"OLD-FLAC".to_vec())
        .with_file("z.mp3", b"ORPHAN".to_vec())
        .fail_remove_out_of_space("r.flac");
    let mut manifest = Manifest::new();
    let r_before = entry("r.flac", AudioFormat::Flac);
    manifest.insert("r", r_before.clone());
    manifest.insert("z", entry("z.mp3", AudioFormat::Mp3));

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.status, RunStatus::DiskFull);
    assert_eq!(outcome.reformatted, 0);
    assert_eq!(outcome.deleted, 0);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.failures[0].clip_id, "r");
    assert!(outcome.failures[0].reason.contains("disk full"));
    // The failing action's old file stays and its manifest entry is unmutated:
    // the unlink precedes the manifest insert, so nothing was recorded.
    assert!(fs.exists("r.flac"));
    assert_eq!(manifest.get("r"), Some(&r_before));
    // The later orphan delete never ran: the run aborted at the unlink.
    assert!(fs.exists("z.mp3"));
    assert!(manifest.get("z").is_some());
}

#[test]
fn cdn_download_rejection_skips_the_clip_without_aborting() {
    let c1 = clip("k1");
    let c2 = clip("k2");
    let d1 = desired(c1.clone(), AudioFormat::Mp3);
    let d2 = desired(c2.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![
            Action::Download {
                clip: c1.clone(),
                lineage: LineageContext::own_root(&c1),
                path: d1.path.clone(),
                format: AudioFormat::Mp3,
            },
            Action::Download {
                clip: c2.clone(),
                lineage: LineageContext::own_root(&c2),
                path: d2.path.clone(),
                format: AudioFormat::Mp3,
            },
        ],
    };
    // A CDN media fetch is unauthenticated, so a 403 is a per-asset
    // rejection (often transient), not a bad token: the clip is retried
    // then recorded and skipped, and the run carries on to the rest.
    let http = ScriptedHttp::new()
        .route("k1.mp3", Reply::status(403))
        .route("k2.mp3", Reply::ok(b"body".to_vec()));
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d1, d2],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_ne!(outcome.status, RunStatus::AuthAborted);
    assert_eq!(outcome.downloaded, 1);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.failures[0].clip_id, "k1");
}

#[test]
fn one_clip_failure_does_not_abort_the_run() {
    let c1 = clip("l1");
    let c2 = clip("l2");
    let d1 = desired(c1.clone(), AudioFormat::Mp3);
    let d2 = desired(c2.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![
            Action::Download {
                clip: c1.clone(),
                lineage: LineageContext::own_root(&c1),
                path: d1.path.clone(),
                format: AudioFormat::Mp3,
            },
            Action::Download {
                clip: c2.clone(),
                lineage: LineageContext::own_root(&c2),
                path: d2.path.clone(),
                format: AudioFormat::Mp3,
            },
        ],
    };
    let http = ScriptedHttp::new()
        .route("l1.mp3", Reply::status(404))
        .route("l2.mp3", Reply::ok(b"body".to_vec()));
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d1, d2],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.downloaded, 1);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.failures[0].clip_id, "l1");
    assert!(fs.exists("l2.mp3"));
    assert!(manifest.get("l2").is_some());
    assert!(manifest.get("l1").is_none());
}
