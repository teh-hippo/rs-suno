use super::*;

#[test]
fn preserve_is_set_for_copy_held_and_private_clips() {
    let mut mirror = desired(clip("m1"), AudioFormat::Mp3);
    mirror.modes = vec![SourceMode::Mirror];
    let mut copy_held = desired(clip("m2"), AudioFormat::Mp3);
    copy_held.modes = vec![SourceMode::Mirror, SourceMode::Copy];
    let mut private = desired(clip("m3"), AudioFormat::Mp3);
    private.private = true;

    let plan = Plan {
        actions: vec![
            Action::Download {
                clip: mirror.clip.clone(),
                lineage: LineageContext::own_root(&mirror.clip),
                path: mirror.path.clone(),
                format: AudioFormat::Mp3,
            },
            Action::Download {
                clip: copy_held.clip.clone(),
                lineage: LineageContext::own_root(&copy_held.clip),
                path: copy_held.path.clone(),
                format: AudioFormat::Mp3,
            },
            Action::Download {
                clip: private.clip.clone(),
                lineage: LineageContext::own_root(&private.clip),
                path: private.path.clone(),
                format: AudioFormat::Mp3,
            },
        ],
    };
    let http = ScriptedHttp::new()
        .route("m1.mp3", Reply::ok(b"a".to_vec()))
        .route("m2.mp3", Reply::ok(b"b".to_vec()))
        .route("m3.mp3", Reply::ok(b"c".to_vec()));
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[mirror, copy_held, private],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.downloaded, 3);
    assert!(!manifest.get("m1").unwrap().preserve);
    assert!(manifest.get("m2").unwrap().preserve);
    assert!(manifest.get("m3").unwrap().preserve);
}

#[test]
fn reformat_writes_new_format_and_removes_old_file() {
    let c = clip("n");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Reformat {
            clip: c.clone(),
            path: "n.mp3".to_owned(),
            from_path: "n.flac".to_owned(),
            from: AudioFormat::Flac,
            to: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new().route("n.mp3", Reply::ok(b"body".to_vec()));
    let fs = MemFs::new().with_file("n.flac", b"OLD-FLAC".to_vec());
    let mut manifest = Manifest::new();
    manifest.insert("n", entry("n.flac", AudioFormat::Flac));

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

    assert_eq!(outcome.reformatted, 1);
    assert!(fs.exists("n.mp3"));
    assert!(!fs.exists("n.flac"));
    let updated = manifest.get("n").unwrap();
    assert_eq!(updated.path, "n.mp3");
    assert_eq!(updated.format, AudioFormat::Mp3);
    assert_eq!(updated.meta_hash, "m");
}

#[test]
fn retag_rewrites_file_and_updates_hashes() {
    let c = clip("o");
    let mut d = desired(c.clone(), AudioFormat::Mp3);
    d.meta_hash = "new".to_owned();
    d.art_hash = "new-art".to_owned();
    let existing = tag_mp3(
        b"audio",
        &TrackMetadata::from_clip(&c, &LineageContext::own_root(&c)),
        None,
        None,
    )
    .unwrap();
    let fs = MemFs::new().with_file("o.mp3", existing.clone());
    let mut manifest = Manifest::new();
    let mut start = entry("o.mp3", AudioFormat::Mp3);
    start.size = existing.len() as u64;
    manifest.insert("o", start);
    let plan = Plan {
        actions: vec![Action::Retag {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: "o.mp3".to_owned(),
        }],
    };

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.retagged, 1);
    let updated = manifest.get("o").unwrap();
    assert_eq!(updated.meta_hash, "new");
    assert_eq!(updated.art_hash, "new-art");
    assert_eq!(&fs.read_file("o.mp3").unwrap()[..3], b"ID3");
}

#[test]
fn rename_moves_file_and_updates_manifest_path() {
    let c = clip("p");
    let mut d = desired(c.clone(), AudioFormat::Mp3);
    d.path = "new/p.mp3".to_owned();
    let fs = MemFs::new().with_file("old/p.mp3", b"DATA".to_vec());
    let mut manifest = Manifest::new();
    manifest.insert("p", entry("old/p.mp3", AudioFormat::Mp3));
    let plan = Plan {
        actions: vec![Action::Rename {
            from: "old/p.mp3".to_owned(),
            to: "new/p.mp3".to_owned(),
        }],
    };

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.renamed, 1);
    assert!(fs.exists("new/p.mp3"));
    assert!(!fs.exists("old/p.mp3"));
    assert_eq!(manifest.get("p").unwrap().path, "new/p.mp3");
}

#[test]
fn disk_full_rename_aborts_the_run() {
    // A move onto a full disk is systemic like a full-disk write: the run
    // aborts with DiskFull and the source file is left untouched.
    let c = clip("p");
    let mut d = desired(c.clone(), AudioFormat::Mp3);
    d.path = "new/p.mp3".to_owned();
    let fs = MemFs::new()
        .with_file("old/p.mp3", b"DATA".to_vec())
        .fail_rename_out_of_space("new/p.mp3");
    let mut manifest = Manifest::new();
    manifest.insert("p", entry("old/p.mp3", AudioFormat::Mp3));
    let plan = Plan {
        actions: vec![Action::Rename {
            from: "old/p.mp3".to_owned(),
            to: "new/p.mp3".to_owned(),
        }],
    };

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.status, RunStatus::DiskFull);
    assert_eq!(outcome.renamed, 0);
    assert_eq!(outcome.failed(), 1);
    assert!(outcome.failures[0].reason.contains("disk full"));
    // The source is untouched: the move never happened.
    assert!(fs.exists("old/p.mp3"));
    assert!(!fs.exists("new/p.mp3"));
    assert_eq!(manifest.get("p").unwrap().path, "old/p.mp3");
}

#[test]
fn delete_removes_file_and_manifest_entry() {
    let fs = MemFs::new().with_file("q.mp3", b"DATA".to_vec());
    let mut manifest = Manifest::new();
    manifest.insert("q", entry("q.mp3", AudioFormat::Mp3));
    let plan = Plan {
        actions: vec![Action::Delete {
            path: "q.mp3".to_owned(),
            clip_id: "q".to_owned(),
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

    assert_eq!(outcome.deleted, 1);
    assert!(!fs.exists("q.mp3"));
    assert!(manifest.get("q").is_none());
}

#[test]
fn failed_delete_keeps_the_manifest_entry() {
    let fs = MemFs::new()
        .with_file("s.mp3", b"DATA".to_vec())
        .fail_remove("s.mp3");
    let mut manifest = Manifest::new();
    manifest.insert("s", entry("s.mp3", AudioFormat::Mp3));
    let plan = Plan {
        actions: vec![Action::Delete {
            path: "s.mp3".to_owned(),
            clip_id: "s".to_owned(),
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

    assert_eq!(outcome.deleted, 0);
    assert_eq!(outcome.failed(), 1);
    assert!(manifest.get("s").is_some());
    assert!(fs.exists("s.mp3"));
}

#[test]
fn skip_is_a_noop() {
    let mut manifest = Manifest::new();
    let plan = Plan {
        actions: vec![Action::Skip {
            clip_id: "r".to_owned(),
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
    assert_eq!(outcome.skipped, 1);
    assert_eq!(outcome.failed(), 0);
}

#[test]
fn header_helpers_parse_or_ignore() {
    let resp = HttpResponse {
        status: 200,
        headers: vec![("Content-Length".to_owned(), "42".to_owned())],
        body: Vec::new(),
    };
    assert_eq!(content_length(&resp), Some(42));

    let bare = HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: Vec::new(),
    };
    assert_eq!(content_length(&bare), None);
}

#[test]
fn preserve_rule_covers_copy_and_private() {
    let base = desired(clip("x"), AudioFormat::Mp3);
    assert!(!preserve_for(&base));
    let mut copy_held = base.clone();
    copy_held.modes = vec![SourceMode::Copy];
    assert!(preserve_for(&copy_held));
    let mut private = base.clone();
    private.private = true;
    assert!(preserve_for(&private));
}

#[test]
fn skip_sets_preserve_when_a_clip_becomes_copy_held() {
    let c = clip("s1");
    let mut d = desired(c.clone(), AudioFormat::Mp3);
    d.modes = vec![SourceMode::Copy];
    let plan = Plan {
        actions: vec![Action::Skip {
            clip_id: "s1".to_owned(),
        }],
    };
    let mut manifest = Manifest::new();
    manifest.insert("s1".to_owned(), entry("s1.mp3", AudioFormat::Mp3));
    assert!(!manifest.get("s1").unwrap().preserve);

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &ScriptedHttp::new(),
        &fs_new(),
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.skipped, 1);
    assert!(
        manifest.get("s1").unwrap().preserve,
        "a copy-held skip must mark the entry preserved"
    );
}

#[test]
fn skip_clears_stale_preserve_when_a_clip_returns_to_mirror_only() {
    let c = clip("s2");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Skip {
            clip_id: "s2".to_owned(),
        }],
    };
    let mut manifest = Manifest::new();
    let mut stale = entry("s2.mp3", AudioFormat::Mp3);
    stale.preserve = true;
    manifest.insert("s2".to_owned(), stale);

    run(
        &plan,
        &mut manifest,
        &[d],
        &ScriptedHttp::new(),
        &fs_new(),
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert!(
        !manifest.get("s2").unwrap().preserve,
        "a mirror-only skip must clear a stale preserve marker"
    );
}
