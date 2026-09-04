use super::*;
use crate::lyrics::{AlignedLine, AlignedLineWord};

#[test]
fn download_mp3_writes_tagged_file_and_records_manifest() {
    let c = art_clip("a");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new()
        .route("a.mp3", Reply::ok(b"mp3-body".to_vec()))
        .route("a/large.jpg", Reply::ok(b"art-bytes".to_vec()));
    let fs = MemFs::new();
    let ffmpeg = StubFfmpeg::flac();
    let clock = RecordingClock::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &ffmpeg,
        &clock,
        &ExecOptions::default(),
    );

    assert_eq!(outcome.downloaded, 1);
    assert_eq!(outcome.failed(), 0);
    assert_eq!(outcome.status, RunStatus::Completed);
    let written = fs.read_file("a.mp3").unwrap();
    assert_eq!(&written[..3], b"ID3");
    assert!(written.ends_with(b"mp3-body"));
    let entry = manifest.get("a").unwrap();
    assert_eq!(entry.path, "a.mp3");
    assert_eq!(entry.format, AudioFormat::Mp3);
    assert_eq!(entry.meta_hash, "m");
    assert_eq!(entry.art_source_hash(), "art");
    assert_eq!(entry.size, written.len() as u64);
    assert!(!entry.preserve);
}

#[test]
fn download_mp3_embeds_sylt_and_lyrics_from_synced_map() {
    // A clip whose alignment was fetched this run gets the default line-level SYLT
    // and its plain lyric text embedded (USLT), end to end through execute.
    let c = art_clip("a");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new()
        .route("a.mp3", Reply::ok(b"mp3-body".to_vec()))
        .route("a/large.jpg", Reply::ok(b"art-bytes".to_vec()));
    let fs = MemFs::new();
    let ffmpeg = StubFfmpeg::flac();
    let clock = RecordingClock::new();
    let mut manifest = Manifest::new();
    let mut albums = BTreeMap::new();
    let mut playlists = BTreeMap::new();
    let mut synced = HashMap::new();
    synced.insert(
        "a".to_string(),
        AlignedLyrics {
            lines: vec![AlignedLine {
                text: "hi there".to_owned(),
                start_s: 0.5,
                end_s: 1.2,
                section: "Verse 1".to_owned(),
                words: vec![
                    AlignedLineWord {
                        text: "hi".to_owned(),
                        start_s: 0.5,
                        end_s: 0.8,
                    },
                    AlignedLineWord {
                        text: "there".to_owned(),
                        start_s: 0.9,
                        end_s: 1.2,
                    },
                ],
            }],
            ..Default::default()
        },
    );
    let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
    let opts = ExecOptions {
        embed_synced_lyrics: true,
        ..ExecOptions::default()
    };
    let outcome = pollster::block_on(execute(
        &plan,
        Stores {
            manifest: &mut manifest,
            albums: &mut albums,
            playlists: &mut playlists,
        },
        &[d],
        &synced,
        Ports {
            client: &client,
            http: &http,
            fs: &fs,
            ffmpeg: &ffmpeg,
            clock: &clock,
        },
        &opts,
    ));

    assert_eq!(outcome.downloaded, 1);
    let written = fs.read_file("a.mp3").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(written)).unwrap();
    let sylt = tag.synchronised_lyrics().next().unwrap();
    assert_eq!(sylt.content, vec![(500, "hi there".to_owned())]);
    // The plain lyric text is populated from the alignment for the USLT frame.
    assert_eq!(
        tag.lyrics().next().map(|frame| frame.text.as_str()),
        Some("hi there")
    );
}

#[test]
fn download_mp3_embeds_plain_lyrics_without_sylt_when_timing_is_disabled() {
    // Baseline alignment supplies USLT even with no `.lrc` feature, but must not
    // silently enable SYLT.
    let c = art_clip("a");
    let mut d = desired(c.clone(), AudioFormat::Mp3);
    d.embedded_lyrics_hash = crate::content_hash("plain words");
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new()
        .route("a.mp3", Reply::ok(b"mp3-body".to_vec()))
        .route("a/large.jpg", Reply::ok(b"art-bytes".to_vec()));
    let fs = MemFs::new();
    let ffmpeg = StubFfmpeg::flac();
    let clock = RecordingClock::new();
    let mut manifest = Manifest::new();
    let mut albums = BTreeMap::new();
    let mut playlists = BTreeMap::new();
    let synced = HashMap::from([(
        "a".to_string(),
        AlignedLyrics {
            lines: vec![AlignedLine {
                text: "plain words".to_owned(),
                start_s: 0.5,
                end_s: 1.2,
                section: String::new(),
                words: Vec::new(),
            }],
            ..Default::default()
        },
    )]);
    let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
    let outcome = pollster::block_on(execute(
        &plan,
        Stores {
            manifest: &mut manifest,
            albums: &mut albums,
            playlists: &mut playlists,
        },
        &[d],
        &synced,
        Ports {
            client: &client,
            http: &http,
            fs: &fs,
            ffmpeg: &ffmpeg,
            clock: &clock,
        },
        &ExecOptions::default(),
    ));
    assert_eq!(outcome.downloaded, 1);
    let written = fs.read_file("a.mp3").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(written)).unwrap();
    assert_eq!(tag.synchronised_lyrics().count(), 0);
    assert_eq!(
        tag.lyrics().next().map(|frame| frame.text.as_str()),
        Some("plain words")
    );
    assert_eq!(
        manifest.get("a").unwrap().embedded_lyrics_hash,
        crate::content_hash("plain words")
    );
}

#[test]
fn download_mp3_uses_cdn_fallback_when_audio_url_empty() {
    let mut c = clip("a");
    c.audio_url = String::new();
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Mp3,
        }],
    };
    let http = ScriptedHttp::new().route("cdn1.suno.ai/a.mp3", Reply::ok(b"body".to_vec()));
    let fs = MemFs::new();
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
    assert_eq!(outcome.downloaded, 1);
    assert_eq!(http.count("cdn1.suno.ai/a.mp3"), 1);
}

#[test]
fn download_flac_renders_transcodes_and_records() {
    let (_c, d, action) = download("b", AudioFormat::Flac);
    let plan = Plan {
        actions: vec![action],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route(
            "/wav_file/",
            Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/b.wav"}"#),
        )
        .route("b.wav", Reply::ok(b"wav-bytes".to_vec()));
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
    assert_eq!(outcome.failed(), 0);
    let written = fs.read_file("b.flac").unwrap();
    assert_eq!(&written[..4], b"fLaC");
    assert_eq!(manifest.get("b").unwrap().format, AudioFormat::Flac);
    // The URL was ready immediately, so no render request and no polling.
    assert_eq!(http.count("/convert_wav/"), 0);
    assert!(clock.sleeps().is_empty());
}

#[test]
fn download_flac_refreshes_a_rejected_signed_url() {
    let (_c, d, action) = download("expired", AudioFormat::Flac);
    let plan = Plan {
        actions: vec![action],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route_seq(
            "/wav_file/",
            vec![
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/expired.wav"}"#),
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/fresh.wav"}"#),
            ],
        )
        .route("/convert_wav/", Reply::status(200))
        .route("expired.wav", Reply::status(403))
        .route("fresh.wav", Reply::ok(b"wav".to_vec()));
    let clock = RecordingClock::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs_new(),
        &StubFfmpeg::flac(),
        &clock,
        &small_poll(),
    );

    assert_eq!(outcome.downloaded, 1);
    assert_eq!(outcome.failed(), 0);
    assert_eq!(http.count("expired.wav"), 1);
    assert_eq!(http.count("fresh.wav"), 1);
    assert_eq!(http.count("/convert_wav/"), 1);
    assert_eq!(http.count("/wav_file/"), 2);
    assert_eq!(clock.sleeps(), vec![Duration::from_secs(5)]);
}

#[test]
fn download_flac_retries_a_rejected_refreshed_url() {
    let (_c, d, action) = download("retry", AudioFormat::Flac);
    let plan = Plan {
        actions: vec![action],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route_seq(
            "/wav_file/",
            vec![
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/old.wav"}"#),
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/new.wav"}"#),
            ],
        )
        .route("/convert_wav/", Reply::status(200))
        .route("old.wav", Reply::status(403))
        .route_seq(
            "new.wav",
            vec![Reply::status(403), Reply::ok(b"wav".to_vec())],
        );
    let clock = RecordingClock::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs_new(),
        &StubFfmpeg::flac(),
        &clock,
        &small_poll(),
    );

    assert_eq!(outcome.downloaded, 1);
    assert_eq!(outcome.failed(), 0);
    assert_eq!(http.count("old.wav"), 1);
    assert_eq!(http.count("new.wav"), 2);
    assert_eq!(http.count("/convert_wav/"), 1);
    assert_eq!(
        clock.sleeps(),
        vec![Duration::from_secs(5), Duration::from_secs(1)]
    );
}

#[test]
fn download_flac_waits_for_the_rejected_url_to_change() {
    let (_c, d, action) = download("cached", AudioFormat::Flac);
    let plan = Plan {
        actions: vec![action],
    };
    let stale = r#"{"wav_file_url": "https://cdn1.suno.ai/stale.wav"}"#;
    let http = ScriptedHttp::new()
        .with_auth()
        .route_seq(
            "/wav_file/",
            vec![
                Reply::json(stale),
                Reply::json(stale),
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/fresh.wav"}"#),
            ],
        )
        .route("/convert_wav/", Reply::status(200))
        .route("stale.wav", Reply::status(403))
        .route("fresh.wav", Reply::ok(b"wav".to_vec()));
    let clock = RecordingClock::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs_new(),
        &StubFfmpeg::flac(),
        &clock,
        &small_poll(),
    );

    assert_eq!(outcome.downloaded, 1);
    assert_eq!(outcome.failed(), 0);
    assert_eq!(http.count("stale.wav"), 1);
    assert_eq!(http.count("fresh.wav"), 1);
    assert_eq!(http.count("/wav_file/"), 3);
    assert_eq!(
        clock.sleeps(),
        vec![Duration::from_secs(5), Duration::from_secs(5)]
    );
}

#[test]
fn rejected_original_and_fresh_wav_fall_back_only_that_clip_to_mp3() {
    let (_c1, d1, action1) = download("denied", AudioFormat::Flac);
    let (_c2, d2, action2) = download("healthy", AudioFormat::Flac);
    let plan = Plan {
        actions: vec![action1, action2],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route_seq(
            "/gen/denied/wav_file/",
            vec![
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/old.wav"}"#),
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/fresh.wav"}"#),
            ],
        )
        .route(
            "/gen/healthy/wav_file/",
            Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/healthy.wav"}"#),
        )
        .route("/convert_wav/", Reply::status(200))
        .route("old.wav", Reply::status(403))
        .route("fresh.wav", Reply::status(403))
        .route("denied.mp3", Reply::ok(b"mp3".to_vec()))
        .route("healthy.wav", Reply::ok(b"wav".to_vec()));
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    let mut opts = small_poll();
    opts.concurrency = 1;

    let outcome = run(
        &plan,
        &mut manifest,
        &[d1, d2],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &opts,
    );

    assert_eq!(outcome.downloaded, 2);
    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.fallbacks.len(), 1);
    assert!(outcome.fallbacks[0].reason.contains("status 403"));
    assert!(fs.exists("denied.mp3"));
    assert!(!fs.exists("denied.flac"));
    assert_eq!(manifest.get("denied").unwrap().format, AudioFormat::Mp3);
    assert!(fs.exists("healthy.flac"));
    assert_eq!(manifest.get("healthy").unwrap().format, AudioFormat::Flac);
}

#[test]
fn rejected_lossless_upgrade_preserves_the_existing_mp3() {
    let c = clip("preserve");
    let d = desired(c.clone(), AudioFormat::Flac);
    let plan = Plan {
        actions: vec![Action::Reformat {
            clip: c,
            path: "preserve.flac".to_owned(),
            from_path: "preserve.mp3".to_owned(),
            from: AudioFormat::Mp3,
            to: AudioFormat::Flac,
        }],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route_seq(
            "/wav_file/",
            vec![
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/old.wav"}"#),
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/fresh.wav"}"#),
            ],
        )
        .route("/convert_wav/", Reply::status(200))
        .route("old.wav", Reply::status(403))
        .route("fresh.wav", Reply::status(403));
    let fs = MemFs::new().with_file("preserve.mp3", b"EXISTING".to_vec());
    let mut manifest = Manifest::new();
    let before = entry("preserve.mp3", AudioFormat::Mp3);
    manifest.insert("preserve", before.clone());

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &small_poll(),
    );

    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.fallbacks.len(), 1);
    assert_eq!(outcome.skipped, 1);
    assert_eq!(fs.read_file("preserve.mp3"), Some(b"EXISTING".to_vec()));
    assert!(!fs.exists("preserve.flac"));
    assert_eq!(manifest.get("preserve"), Some(&before));
    assert_eq!(http.count("preserve.mp3"), 0);
}

#[test]
fn transient_lossless_upgrade_preserves_the_existing_mp3() {
    let c = clip("transient");
    let d = desired(c.clone(), AudioFormat::Flac);
    let plan = Plan {
        actions: vec![Action::Reformat {
            clip: c,
            path: "transient.flac".to_owned(),
            from_path: "transient.mp3".to_owned(),
            from: AudioFormat::Mp3,
            to: AudioFormat::Flac,
        }],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/wav_file/", Reply::json("{}"))
        .route("/convert_wav/", Reply::status(200));
    let fs = MemFs::new().with_file("transient.mp3", b"EXISTING".to_vec());
    let mut manifest = Manifest::new();
    let before = entry("transient.mp3", AudioFormat::Mp3);
    manifest.insert("transient", before.clone());

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &small_poll(),
    );

    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.fallbacks.len(), 1);
    assert_eq!(outcome.skipped, 1);
    assert_eq!(fs.read_file("transient.mp3"), Some(b"EXISTING".to_vec()));
    assert!(!fs.exists("transient.flac"));
    assert_eq!(manifest.get("transient"), Some(&before));
}

#[test]
fn download_flac_requests_render_then_polls_until_ready() {
    let (_c, d, action) = download("c", AudioFormat::Flac);
    let plan = Plan {
        actions: vec![action],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route_seq(
            "/wav_file/",
            vec![
                Reply::json("{}"),
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/c.wav"}"#),
            ],
        )
        .route("/convert_wav/", Reply::status(200))
        .route("c.wav", Reply::ok(b"wav".to_vec()));
    let clock = RecordingClock::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs_new(),
        &StubFfmpeg::flac(),
        &clock,
        &small_poll(),
    );

    assert_eq!(outcome.downloaded, 1);
    assert_eq!(http.count("/convert_wav/"), 1);
    assert_eq!(clock.sleeps(), vec![Duration::from_secs(5)]);
}

#[test]
fn download_flac_unavailable_render_is_a_nonfatal_failure() {
    let (_c, d, action) = download("d", AudioFormat::Flac);
    let plan = Plan {
        actions: vec![action],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/wav_file/", Reply::json("{}"))
        .route("/convert_wav/", Reply::status(200));
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
        &small_poll(),
    );

    assert_eq!(outcome.downloaded, 0);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(outcome.failures[0].clip_id, "d");
    assert_eq!(outcome.status, RunStatus::Completed);
    assert!(!fs.exists("d.flac"));
    assert_eq!(clock.sleeps().len(), 2);
}

#[test]
fn flac_transcode_failure_is_recorded_and_skipped() {
    let (_c, d, action) = download("t", AudioFormat::Flac);
    let plan = Plan {
        actions: vec![action],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route(
            "/wav_file/",
            Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/t.wav"}"#),
        )
        .route("t.wav", Reply::ok(b"wav".to_vec()));
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::failing(),
        &RecordingClock::new(),
        &ExecOptions::default(),
    );

    assert_eq!(outcome.downloaded, 0);
    assert_eq!(outcome.failed(), 1);
    assert!(!fs.exists("t.flac"));
    assert!(manifest.get("t").is_none());
}

#[test]
fn entitlement_refusal_falls_back_to_native_mp3_and_is_not_a_failure() {
    let (_c1, mut d1, a1) = download("e1", AudioFormat::Flac);
    let (_c2, mut d2, a2) = download("e2", AudioFormat::Flac);
    let plan = Plan {
        actions: vec![a1, a2],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/wav_file/", Reply::status(403))
        .route("e1.mp3", Reply::ok(b"one".to_vec()))
        .route("e2.mp3", Reply::ok(b"two".to_vec()));
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    let mut opts = small_poll();
    opts.concurrency = 1;
    let alignment = AlignedLyrics {
        lines: vec![AlignedLine {
            text: "fallback words".to_owned(),
            start_s: 0.0,
            end_s: 1.0,
            section: String::new(),
            words: Vec::new(),
        }],
        ..Default::default()
    };
    let synced = HashMap::from([
        ("e1".to_string(), alignment.clone()),
        ("e2".to_string(), alignment),
    ]);
    let lyrics_hash = crate::content_hash("fallback words");
    d1.embedded_lyrics_hash = lyrics_hash.clone();
    d2.embedded_lyrics_hash = lyrics_hash;

    let outcome = run_with_synced(
        &plan,
        &mut manifest,
        &[d1, d2],
        &synced,
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &opts,
    );

    assert_eq!(outcome.status, RunStatus::Completed);
    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.fallbacks.len(), 2);
    assert!(
        outcome.fallbacks[0]
            .reason
            .contains("entitlement unavailable")
    );
    assert_eq!(outcome.downloaded, 2);
    assert!(fs.exists("e1.mp3"));
    assert!(fs.exists("e2.mp3"));
    assert!(!fs.exists("e1.flac"));
    assert_eq!(manifest.get("e1").unwrap().format, AudioFormat::Mp3);
    assert_eq!(manifest.get("e1").unwrap().path, "e1.mp3");
    let written = fs.read_file("e1.mp3").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(written)).unwrap();
    assert_eq!(
        tag.lyrics().next().map(|frame| frame.text.as_str()),
        Some("fallback words")
    );
    assert_eq!(tag.synchronised_lyrics().count(), 0);
    assert_eq!(
        http.count("/wav_file/"),
        2,
        "the first refusal refreshes once; later clips use the cached fallback"
    );
}

#[test]
fn entitlement_refusal_preserves_an_existing_lossless_reformat_source() {
    let c = clip("keep");
    let d = desired(c.clone(), AudioFormat::Alac);
    let plan = Plan {
        actions: vec![Action::Reformat {
            clip: c,
            path: "keep.m4a".to_owned(),
            from_path: "keep.flac".to_owned(),
            from: AudioFormat::Flac,
            to: AudioFormat::Alac,
        }],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/wav_file/", Reply::status(403));
    let fs = MemFs::new().with_file("keep.flac", b"LOSSLESS".to_vec());
    let mut manifest = Manifest::new();
    let before = entry("keep.flac", AudioFormat::Flac);
    manifest.insert("keep", before.clone());

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &small_poll(),
    );

    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.fallbacks.len(), 1);
    assert_eq!(outcome.skipped, 1);
    assert!(fs.exists("keep.flac"));
    assert!(!fs.exists("keep.m4a"));
    assert!(!fs.exists("keep.mp3"));
    assert_eq!(manifest.get("keep"), Some(&before));
}

#[test]
fn entitlement_refusal_keeps_prior_fallback_mp3_as_the_upgrade_baseline() {
    let c = clip("upgrade");
    let d = desired(c.clone(), AudioFormat::Flac);
    let plan = Plan {
        actions: vec![Action::Reformat {
            clip: c,
            path: "upgrade.flac".to_owned(),
            from_path: "upgrade.mp3".to_owned(),
            from: AudioFormat::Mp3,
            to: AudioFormat::Flac,
        }],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route("/wav_file/", Reply::status(403))
        .route("upgrade.mp3", Reply::ok(b"mp3".to_vec()));
    let fs = MemFs::new().with_file("upgrade.mp3", b"OLD".to_vec());
    let mut manifest = Manifest::new();
    manifest.insert("upgrade", entry("upgrade.mp3", AudioFormat::Mp3));

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &small_poll(),
    );

    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.fallbacks.len(), 1);
    assert!(fs.exists("upgrade.mp3"));
    assert!(!fs.exists("upgrade.flac"));
    assert_eq!(fs.read_file("upgrade.mp3"), Some(b"OLD".to_vec()));
    assert_eq!(http.count("upgrade.mp3"), 0);
    assert_eq!(manifest.get("upgrade").unwrap().format, AudioFormat::Mp3);
    assert_eq!(manifest.get("upgrade").unwrap().path, "upgrade.mp3");
}
