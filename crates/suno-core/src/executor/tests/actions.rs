use super::*;
use crate::lyrics::{AlignedLine, AlignedLineWord};
use crate::reconcile::DesiredArtifact;

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

// ---- #354: embedded aligned-lyrics back-fill (embedded_lyrics_hash) ----

/// A two-line, word-timed alignment standing in for a run's fetched forced
/// alignment (the synced-map entry a #354 back-fill embeds from).
fn backfill_alignment() -> AlignedLyrics {
    AlignedLyrics {
        lines: vec![
            AlignedLine {
                text: "first line".to_owned(),
                start_s: 0.0,
                end_s: 1.0,
                section: "Verse 1".to_owned(),
                words: vec![
                    AlignedLineWord {
                        text: "first".to_owned(),
                        start_s: 0.0,
                        end_s: 0.5,
                    },
                    AlignedLineWord {
                        text: "line".to_owned(),
                        start_s: 0.5,
                        end_s: 1.0,
                    },
                ],
            },
            AlignedLine {
                text: "second line".to_owned(),
                start_s: 1.0,
                end_s: 2.0,
                section: "Verse 1".to_owned(),
                words: vec![
                    AlignedLineWord {
                        text: "second".to_owned(),
                        start_s: 1.0,
                        end_s: 1.5,
                    },
                    AlignedLineWord {
                        text: "line".to_owned(),
                        start_s: 1.5,
                        end_s: 2.0,
                    },
                ],
            },
        ],
        ..Default::default()
    }
}

/// The synced map (fetch result) for a single clip's alignment.
fn synced_of(id: &str) -> HashMap<String, AlignedLyrics> {
    HashMap::from([(id.to_owned(), backfill_alignment())])
}

/// A manifest entry in the #354 back-fill state: a downloaded clip with a
/// resolved timed `.lrc` slot (`hash = H`) whose audio tag carries no embed yet
/// (`embedded_lyrics_hash = ""`). `meta_hash`/`art_hash` match the test
/// `desired()` so only the lyrics sentinel drifts.
fn backfill_entry(path: &str, format: AudioFormat, hash: &str, size: u64) -> ManifestEntry {
    ManifestEntry {
        path: path.to_owned(),
        format,
        meta_hash: "m".to_owned(),
        art_hash: "art".to_owned(),
        embedded_lyrics_hash: String::new(),
        size,
        lrc: Some(ArtifactState {
            path: format!("{path}.lrc"),
            hash: hash.to_owned(),
        }),
        ..Default::default()
    }
}

fn retag_plan(c: &Clip, path: &str) -> Plan {
    Plan {
        actions: vec![Action::Retag {
            clip: c.clone(),
            lineage: LineageContext::own_root(c),
            path: path.to_owned(),
        }],
    }
}

fn present_local(path: &str, size: u64) -> HashMap<String, crate::reconcile::LocalFile> {
    HashMap::from([(
        path.to_owned(),
        crate::reconcile::LocalFile { exists: true, size },
    )])
}

fn mirror_ok() -> Vec<crate::reconcile::SourceStatus> {
    vec![crate::reconcile::SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: true,
    }]
}

#[test]
fn backfill_refetches_and_embeds_uslt_and_sylt() {
    // #354 core case (MP3): a clip whose `.lrc` is resolved but whose embed was
    // never written back-fills on the next run. The fetched alignment is folded
    // into USLT (plain text) and SYLT (word timings), and the sentinel is stamped
    // so reconcile settles.
    let c = clip("bk");
    let meta = TrackMetadata::from_clip(&c, &LineageContext::own_root(&c));
    let existing = tag_mp3(b"audio", &meta, None, None).unwrap();
    let fs = MemFs::new().with_file("bk.mp3", existing.clone());

    let mut manifest = Manifest::new();
    manifest.insert(
        "bk",
        backfill_entry("bk.mp3", AudioFormat::Mp3, "L", existing.len() as u64),
    );
    let mut d = desired(c.clone(), AudioFormat::Mp3);
    d.embedded_lyrics_hash = "L".to_owned();

    let outcome = run_with_synced(
        &retag_plan(&c, "bk.mp3"),
        &mut manifest,
        &[d],
        &synced_of("bk"),
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.retagged, 1, "exactly one back-fill retag");
    let written = fs.read_file("bk.mp3").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(written)).unwrap();
    assert_eq!(
        tag.lyrics().next().map(|f| f.text.clone()),
        Some(backfill_alignment().plain_text()),
        "USLT gains the plain aligned text"
    );
    assert_eq!(tag.synchronised_lyrics().count(), 1, "SYLT frame embedded");
    assert_eq!(
        manifest.get("bk").unwrap().embedded_lyrics_hash,
        "L",
        "the sentinel is stamped so reconcile settles"
    );
}

#[test]
fn backfill_flac_embeds_vorbis_lyrics() {
    // FLAC back-fill: the fetched alignment is embedded as the Vorbis `LYRICS`
    // comment (FLAC has no `SYLT` equivalent) and the sentinel is stamped.
    let c = clip("bf");
    let meta = TrackMetadata::from_clip(&c, &LineageContext::own_root(&c));
    let existing = tag_flac(&crate::testutil::minimal_flac(), &meta, None).unwrap();
    let fs = MemFs::new().with_file("bf.flac", existing.clone());

    let mut manifest = Manifest::new();
    manifest.insert(
        "bf",
        backfill_entry("bf.flac", AudioFormat::Flac, "L", existing.len() as u64),
    );
    let mut d = desired(c.clone(), AudioFormat::Flac);
    d.embedded_lyrics_hash = "L".to_owned();

    let outcome = run_with_synced(
        &retag_plan(&c, "bf.flac"),
        &mut manifest,
        &[d],
        &synced_of("bf"),
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.retagged, 1);
    let written = fs.read_file("bf.flac").unwrap();
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&written)).unwrap();
    assert_eq!(
        tag.vorbis_comments().unwrap().get("LYRICS").unwrap(),
        &[backfill_alignment().plain_text()],
        "Vorbis LYRICS gains the plain aligned text"
    );
    assert_eq!(manifest.get("bf").unwrap().embedded_lyrics_hash, "L");
}

#[test]
fn backfill_wav_embeds_uslt_and_sylt() {
    // WAV back-fill: `tag_wav` delegates to the same ID3 path as MP3, so the
    // fetch fills USLT and SYLT and the sentinel is stamped.
    let c = clip("bw");
    let meta = TrackMetadata::from_clip(&c, &LineageContext::own_root(&c));
    let existing = tag_wav(&crate::testutil::minimal_wav(), &meta, None, None).unwrap();
    let fs = MemFs::new().with_file("bw.wav", existing.clone());

    let mut manifest = Manifest::new();
    manifest.insert(
        "bw",
        backfill_entry("bw.wav", AudioFormat::Wav, "L", existing.len() as u64),
    );
    let mut d = desired(c.clone(), AudioFormat::Wav);
    d.embedded_lyrics_hash = "L".to_owned();

    let outcome = run_with_synced(
        &retag_plan(&c, "bw.wav"),
        &mut manifest,
        &[d],
        &synced_of("bw"),
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.retagged, 1);
    let written = fs.read_file("bw.wav").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(written)).unwrap();
    assert_eq!(
        tag.lyrics().next().map(|f| f.text.clone()),
        Some(backfill_alignment().plain_text())
    );
    assert_eq!(tag.synchronised_lyrics().count(), 1, "SYLT frame embedded");
    assert_eq!(manifest.get("bw").unwrap().embedded_lyrics_hash, "L");
}

/// ALAC back-fill: `tag_alac` writes `©lyr` whenever `meta.lyrics` is non-empty,
/// which the fetch supplies, so the back-fill lands with no preserve-branch gap.
/// Ignored because CI has no ffmpeg (a real MP4 container is required to parse);
/// run locally with `cargo test -p suno-core -- --ignored`.
#[test]
#[ignore = "requires ffmpeg"]
fn backfill_alac_embeds_lyrics() {
    use std::process::Command;

    let dir = std::path::Path::new("target").join("backfill-alac-smoke");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.m4a");
    let made = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:a",
            "alac",
            "-f",
            "ipod",
        ])
        .arg(&src)
        .status()
        .unwrap();
    assert!(made.success());
    let c = clip("ba");
    let meta = TrackMetadata::from_clip(&c, &LineageContext::own_root(&c));
    let existing = tag_alac(&std::fs::read(&src).unwrap(), &meta, None).unwrap();
    let _ = std::fs::remove_file(&src);
    let fs = MemFs::new().with_file("ba.m4a", existing.clone());

    let mut manifest = Manifest::new();
    manifest.insert(
        "ba",
        backfill_entry("ba.m4a", AudioFormat::Alac, "L", existing.len() as u64),
    );
    let mut d = desired(c.clone(), AudioFormat::Alac);
    d.embedded_lyrics_hash = "L".to_owned();

    let outcome = run_with_synced(
        &retag_plan(&c, "ba.m4a"),
        &mut manifest,
        &[d],
        &synced_of("ba"),
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.retagged, 1);
    let written = fs.read_file("ba.m4a").unwrap();
    let tag = mp4ameta::Tag::read_from(&mut std::io::Cursor::new(written)).unwrap();
    assert_eq!(
        tag.lyrics(),
        Some(backfill_alignment().plain_text().as_str()),
        "the ALAC ©lyr atom gains the plain aligned text"
    );
    assert_eq!(manifest.get("ba").unwrap().embedded_lyrics_hash, "L");
}

#[test]
fn backfill_then_reconcile_is_stable() {
    // After the back-fill stamps the sentinel, reconciling the same desired state
    // against the updated manifest yields zero retags (loop-freedom, end to end).
    let c = clip("bs");
    let meta = TrackMetadata::from_clip(&c, &LineageContext::own_root(&c));
    let existing = tag_mp3(b"audio", &meta, None, None).unwrap();
    let fs = MemFs::new().with_file("bs.mp3", existing.clone());

    let mut manifest = Manifest::new();
    manifest.insert(
        "bs",
        backfill_entry("bs.mp3", AudioFormat::Mp3, "L", existing.len() as u64),
    );
    let mut d = desired(c.clone(), AudioFormat::Mp3);
    d.embedded_lyrics_hash = "L".to_owned();

    run_with_synced(
        &retag_plan(&c, "bs.mp3"),
        &mut manifest,
        std::slice::from_ref(&d),
        &synced_of("bs"),
        &ScriptedHttp::new(),
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );
    assert_eq!(manifest.get("bs").unwrap().embedded_lyrics_hash, "L");

    // Reconcile the same desired state against the now-stamped manifest.
    let size = fs.read_file("bs.mp3").unwrap().len() as u64;
    let plan = crate::reconcile::reconcile(
        &manifest,
        std::slice::from_ref(&d),
        &present_local("bs.mp3", size),
        &mirror_ok(),
    );
    assert_eq!(
        plan.retags(),
        0,
        "no further retag once the embed is stamped"
    );
}

#[test]
fn backfill_fetch_failure_no_retag_no_stamp_retries() {
    // A failed alignment fetch (the clip is absent from the synced successes)
    // must not retag or stamp: the sentinel carries forward the empty embed, so
    // reconcile sees no drift, the manifest is unchanged, and the clip stays a
    // fetch target for the next run. No loop, no masking of the missing embed.
    let c = clip("bx");
    let mut manifest = Manifest::new();
    manifest.insert("bx", backfill_entry("bx.flac", AudioFormat::Flac, "L", 100));

    let mut d = desired(c.clone(), AudioFormat::Flac);
    d.artifacts = vec![DesiredArtifact {
        kind: ArtifactKind::Lrc,
        path: "bx.flac.lrc".to_owned(),
        source_url: String::new(),
        hash: "L".to_owned(),
        content: None,
    }];

    // The failed fetch leaves `successes` empty; the seam carries the embed
    // sentinel forward (still "") rather than adopting the `.lrc` slot hash.
    crate::synced::apply_synced_lrc(std::slice::from_mut(&mut d), &manifest, &HashMap::new());
    assert_eq!(
        d.embedded_lyrics_hash, "",
        "carry-forward, not the slot hash"
    );

    let plan = crate::reconcile::reconcile(
        &manifest,
        std::slice::from_ref(&d),
        &present_local("bx.flac", 100),
        &mirror_ok(),
    );
    assert_eq!(plan.retags(), 0, "a failed fetch does not retag");
    assert_eq!(
        manifest.get("bx").unwrap().embedded_lyrics_hash,
        "",
        "the sentinel is not stamped"
    );

    // Still a fetch target next run, so the back-fill is retried, not masked.
    let targets =
        crate::synced::synced_lyrics_targets(std::slice::from_ref(&d), &manifest, 2_000, true);
    assert!(targets.contains("bx"), "the back-fill is retried next run");
}

#[test]
fn lrc_disabled_after_embed_carries_forward_and_does_not_retag() {
    // Tripwire for the carry-forward-above-`continue` ordering (#354 loop-freedom).
    // A clip previously embedded (embedded_lyrics_hash = "H") whose `.lrc` sidecar
    // is now OFF (no desired `.lrc` artifact) and which is not fetched this run must
    // keep its persisted sentinel and not retag. If the baseline in `apply_synced_lrc`
    // moved below the `.lrc`-artifact `continue`, the sentinel would drift to the
    // default and this clip would spuriously retag with an empty synced map.
    let c = clip("off");
    let mut manifest = Manifest::new();
    let mut e = backfill_entry("off.flac", AudioFormat::Flac, "H", 100);
    e.embedded_lyrics_hash = "H".to_owned(); // a prior embed is present
    e.lrc = None; // the sidecar was turned off: no `.lrc` slot tracked
    manifest.insert("off", e);

    // The lrc sidecar is disabled: the desired carries no `.lrc` artifact.
    let mut d = desired(c.clone(), AudioFormat::Flac);
    assert!(d.artifacts.is_empty(), "no `.lrc` desired this run");

    // Not fetched this run (empty successes): the seam must carry the embed forward.
    crate::synced::apply_synced_lrc(std::slice::from_mut(&mut d), &manifest, &HashMap::new());
    assert_eq!(
        d.embedded_lyrics_hash, "H",
        "the persisted embed sentinel carries forward when no `.lrc` is desired"
    );

    // Never a fetch target (no desired `.lrc`) and never a retag (sentinels match).
    assert!(
        crate::synced::synced_lyrics_targets(std::slice::from_ref(&d), &manifest, 2_000, true)
            .is_empty(),
        "a clip with no desired `.lrc` is never fetched"
    );
    let plan = crate::reconcile::reconcile(
        &manifest,
        std::slice::from_ref(&d),
        &present_local("off.flac", 100),
        &mirror_ok(),
    );
    assert_eq!(
        plan.retags(),
        0,
        "a disabled-lrc clip with a prior embed never spuriously retags"
    );
}

#[test]
fn reformat_and_retitle_with_stale_embed_backfills() {
    // A stale embed plus a format+title change: the reformat re-encodes to MP3
    // and embeds the fresh alignment (USLT + SYLT), and the sentinel is stamped
    // on the newly written file (the scenario a disk parser would have masked).
    let c = clip("gg");
    let fs = MemFs::new().with_file("old-gg.flac", b"OLD-FLAC".to_vec());

    let mut manifest = Manifest::new();
    let mut start = backfill_entry("old-gg.flac", AudioFormat::Flac, "L", 8);
    start.meta_hash = "old".to_owned();
    start.embedded_lyrics_hash = "stale".to_owned();
    manifest.insert("gg", start);

    let mut d = desired(c.clone(), AudioFormat::Mp3);
    d.path = "renamed-gg.mp3".to_owned();
    d.meta_hash = "new".to_owned();
    d.embedded_lyrics_hash = "L".to_owned();

    let plan = Plan {
        actions: vec![Action::Reformat {
            clip: c.clone(),
            path: "renamed-gg.mp3".to_owned(),
            from_path: "old-gg.flac".to_owned(),
            from: AudioFormat::Flac,
            to: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new().route("gg.mp3", Reply::ok(b"mp3-body".to_vec()));

    let outcome = run_with_synced(
        &plan,
        &mut manifest,
        &[d],
        &synced_of("gg"),
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.reformatted, 1);
    assert!(!fs.exists("old-gg.flac"), "the superseded file is removed");
    let written = fs.read_file("renamed-gg.mp3").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(written)).unwrap();
    assert_eq!(
        tag.lyrics().next().map(|f| f.text.clone()),
        Some(backfill_alignment().plain_text()),
        "the re-encoded file carries the fresh lyrics"
    );
    assert_eq!(tag.synchronised_lyrics().count(), 1);
    let updated = manifest.get("gg").unwrap();
    assert_eq!(updated.meta_hash, "new");
    assert_eq!(updated.embedded_lyrics_hash, "L", "the sentinel is stamped");
}

#[test]
fn reformat_on_migrated_clip_refetches_and_reembeds() {
    // Residual B: an already-migrated clip (embed == lrc.hash == H, FLAC) with a
    // pending FLAC->MP3 reformat and no other drift. The reformat drops the embed
    // as it re-encodes, so the clip must become a fetch target and re-embed; the
    // sentinel is (re)stamped, not silently dropped.
    let c = clip("mig");
    let fs = MemFs::new().with_file("mig.flac", b"OLD-FLAC".to_vec());

    let mut manifest = Manifest::new();
    let mut migrated = backfill_entry("mig.flac", AudioFormat::Flac, "H", 8);
    migrated.embedded_lyrics_hash = "H".to_owned(); // already embedded (migrated)
    migrated.synced_lyrics = Some(crate::manifest::SyncedLyricsCheck {
        version: crate::hash::SYNCED_LRC_VERSION,
        checked_unix: 1_000,
        empty: false,
        timed: true,
    });
    manifest.insert("mig", migrated);

    let lrc = DesiredArtifact {
        kind: ArtifactKind::Lrc,
        path: "mig.flac.lrc".to_owned(),
        source_url: String::new(),
        hash: "H".to_owned(),
        content: None,
    };
    let mut d = desired(c.clone(), AudioFormat::Mp3);
    d.embedded_lyrics_hash = "H".to_owned();
    d.artifacts = vec![lrc.clone()];

    // The reformat re-embed trigger makes the migrated clip a fetch target.
    assert!(
        crate::synced::synced_lyrics_targets(std::slice::from_ref(&d), &manifest, 2_000, true)
            .contains("mig"),
        "a pending reformat re-embeds a migrated clip"
    );

    let plan = Plan {
        actions: vec![Action::Reformat {
            clip: c.clone(),
            path: "mig.mp3".to_owned(),
            from_path: "mig.flac".to_owned(),
            from: AudioFormat::Flac,
            to: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new().route("mig.mp3", Reply::ok(b"mp3-body".to_vec()));

    let outcome = run_with_synced(
        &plan,
        &mut manifest,
        &[d],
        &synced_of("mig"),
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.reformatted, 1);
    let written = fs.read_file("mig.mp3").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(written)).unwrap();
    assert_eq!(
        tag.lyrics().next().map(|f| f.text.clone()),
        Some(backfill_alignment().plain_text()),
        "the re-encoded MP3 re-embeds USLT"
    );
    assert_eq!(tag.synchronised_lyrics().count(), 1, "SYLT re-embedded");
    assert_eq!(
        manifest.get("mig").unwrap().embedded_lyrics_hash,
        "H",
        "the sentinel is re-stamped, not dropped"
    );
}

#[test]
fn migrated_clip_without_reformat_is_not_a_target_and_never_retags() {
    // Negative mirror of the Residual B case: a fully-migrated clip with no
    // format change is neither a fetch target nor a retag, so the one-time
    // upgrade never loops.
    let c = clip("st");
    let mut manifest = Manifest::new();
    let mut migrated = backfill_entry("st.mp3", AudioFormat::Mp3, "H", 100);
    migrated.embedded_lyrics_hash = "H".to_owned();
    migrated.synced_lyrics = Some(crate::manifest::SyncedLyricsCheck {
        version: crate::hash::SYNCED_LRC_VERSION,
        checked_unix: 1_000,
        empty: false,
        timed: true,
    });
    manifest.insert("st", migrated);

    let mut d = desired(c.clone(), AudioFormat::Mp3);
    d.embedded_lyrics_hash = "H".to_owned();
    d.artifacts = vec![DesiredArtifact {
        kind: ArtifactKind::Lrc,
        path: "st.mp3.lrc".to_owned(),
        source_url: String::new(),
        hash: "H".to_owned(),
        content: None,
    }];

    assert!(
        crate::synced::synced_lyrics_targets(std::slice::from_ref(&d), &manifest, 2_000, true)
            .is_empty(),
        "no reformat, no back-fill -> no fetch"
    );
    let plan = crate::reconcile::reconcile(
        &manifest,
        std::slice::from_ref(&d),
        &present_local("st.mp3", 100),
        &mirror_ok(),
    );
    assert_eq!(plan.retags(), 0, "a migrated clip never retags");
}
