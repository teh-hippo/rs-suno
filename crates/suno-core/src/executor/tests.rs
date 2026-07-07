use super::*;
use crate::ClerkAuth;
use crate::http::HttpResponse;
use crate::testutil::{MemFs, RecordingClock, Reply, ScriptedHttp, StubFfmpeg};

fn clip(id: &str) -> Clip {
    Clip {
        id: id.to_owned(),
        title: "Song".to_owned(),
        audio_url: format!("https://cdn1.suno.ai/{id}.mp3"),
        ..Default::default()
    }
}

fn art_clip(id: &str) -> Clip {
    Clip {
        image_large_url: format!("https://art.suno.ai/{id}/large.jpg"),
        image_url: format!("https://art.suno.ai/{id}/small.jpg"),
        ..clip(id)
    }
}

fn desired(clip: Clip, format: AudioFormat) -> Desired {
    Desired {
        path: format!("{}.{}", clip.id, format.ext()),
        lineage: LineageContext::own_root(&clip),
        clip,
        format,
        meta_hash: "m".to_owned(),
        art_hash: "art".to_owned(),
        modes: vec![SourceMode::Mirror],
        trashed: false,
        private: false,
        artifacts: Vec::new(),
        stems: None,
    }
}

fn entry(path: &str, format: AudioFormat) -> ManifestEntry {
    ManifestEntry {
        path: path.to_owned(),
        format,
        meta_hash: "old".to_owned(),
        art_hash: "old-art".to_owned(),
        size: 8,
        preserve: false,
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn run<G: Ffmpeg>(
    plan: &Plan,
    manifest: &mut Manifest,
    desired: &[Desired],
    http: &ScriptedHttp,
    fs: &MemFs,
    ffmpeg: &G,
    clock: &RecordingClock,
    opts: &ExecOptions,
) -> ExecOutcome {
    let mut albums = BTreeMap::new();
    run_with_albums(
        plan,
        manifest,
        &mut albums,
        desired,
        http,
        fs,
        ffmpeg,
        clock,
        opts,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_with_albums<G: Ffmpeg>(
    plan: &Plan,
    manifest: &mut Manifest,
    albums: &mut BTreeMap<String, AlbumArt>,
    desired: &[Desired],
    http: &ScriptedHttp,
    fs: &MemFs,
    ffmpeg: &G,
    clock: &RecordingClock,
    opts: &ExecOptions,
) -> ExecOutcome {
    let mut playlists = BTreeMap::new();
    run_full(
        plan,
        manifest,
        albums,
        &mut playlists,
        desired,
        http,
        fs,
        ffmpeg,
        clock,
        opts,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_full<G: Ffmpeg>(
    plan: &Plan,
    manifest: &mut Manifest,
    albums: &mut BTreeMap<String, AlbumArt>,
    playlists: &mut BTreeMap<String, PlaylistState>,
    desired: &[Desired],
    http: &ScriptedHttp,
    fs: &MemFs,
    ffmpeg: &G,
    clock: &RecordingClock,
    opts: &ExecOptions,
) -> ExecOutcome {
    let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
    let synced = HashMap::new();
    pollster::block_on(execute(
        plan,
        manifest,
        albums,
        playlists,
        desired,
        &synced,
        Ports {
            client: &client,
            http,
            fs,
            ffmpeg,
            clock,
        },
        opts,
    ))
}

fn small_poll() -> ExecOptions {
    ExecOptions {
        max_retries: 3,
        wav_poll_attempts: 2,
        wav_poll_interval: Duration::from_secs(5),
        concurrency: 4,
        embed_animated_cover: false,
        cover_webp: WebpEncodeSettings::default(),
    }
}

// ── Download: MP3 ───────────────────────────────────────────────

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
    assert_eq!(entry.art_hash, "art");
    assert_eq!(entry.size, written.len() as u64);
    assert!(!entry.preserve);
}

#[test]
fn download_mp3_embeds_sylt_and_lyrics_from_synced_map() {
    // A clip whose alignment was fetched this run gets a word-level SYLT frame
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
        AlignedLyrics::from_json(&serde_json::json!({
            "aligned_words": [],
            "aligned_lyrics": [
                {"text": "hi there", "start_s": 0.5, "end_s": 1.2, "section": "Verse 1",
                 "words": [
                     {"text": "hi", "start_s": 0.5, "end_s": 0.8},
                     {"text": "there", "start_s": 0.9, "end_s": 1.2}
                 ]}
            ]
        })),
    );
    let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
    let outcome = pollster::block_on(execute(
        &plan,
        &mut manifest,
        &mut albums,
        &mut playlists,
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
    assert_eq!(
        tag.synchronised_lyrics().count(),
        1,
        "a SYLT frame is embedded"
    );
    // The plain lyric text is populated from the alignment for the USLT frame.
    assert_eq!(
        tag.lyrics().next().map(|frame| frame.text.as_str()),
        Some("hi there")
    );
}

#[test]
fn download_mp3_embeds_no_sylt_when_synced_map_empty() {
    // The synced map is empty when the feature is off (no alignment fetched),
    // so no SYLT frame and no lyric text are embedded.
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
    let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
    let outcome = pollster::block_on(execute(
        &plan,
        &mut manifest,
        &mut albums,
        &mut playlists,
        &[d],
        &HashMap::new(),
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
    assert_eq!(tag.lyrics().count(), 0);
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

// ── Download: FLAC render + transcode ───────────────────────────

#[test]
fn download_flac_renders_transcodes_and_records() {
    let c = clip("b");
    let d = desired(c.clone(), AudioFormat::Flac);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Flac,
        }],
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
fn download_flac_requests_render_then_polls_until_ready() {
    let c = clip("c");
    let d = desired(c.clone(), AudioFormat::Flac);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Flac,
        }],
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
    let c = clip("d");
    let d = desired(c.clone(), AudioFormat::Flac);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Flac,
        }],
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
    let c = clip("t");
    let d = desired(c.clone(), AudioFormat::Flac);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Flac,
        }],
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

// ── Cover fallback ──────────────────────────────────────────────

#[test]
fn cover_falls_back_when_large_image_is_missing() {
    let c = art_clip("e");
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
        .route("e.mp3", Reply::ok(b"body".to_vec()))
        .route("e/large.jpg", Reply::status(404))
        .route("e/small.jpg", Reply::ok(b"the-art".to_vec()));
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
    let calls = http.calls();
    let large = calls
        .iter()
        .position(|u| u.contains("e/large.jpg"))
        .unwrap();
    let small = calls
        .iter()
        .position(|u| u.contains("e/small.jpg"))
        .unwrap();
    assert!(large < small, "large art tried before small");
}

// ── Cover reuse: embed + sidecar share one fetch (#89) ──────────

#[test]
fn download_reuses_the_embedded_cover_for_the_jpg_sidecar() {
    // The embedded tag and the `.jpg` sidecar want the same cover URL; it is
    // fetched once and the bytes serve both.
    let c = art_clip("a");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![
            Action::Download {
                clip: c.clone(),
                lineage: LineageContext::own_root(&c),
                path: d.path.clone(),
                format: AudioFormat::Mp3,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "a/cover.jpg".to_owned(),
                source_url: c.selected_image_url().unwrap().to_owned(),
                hash: "art".to_owned(),
                owner_id: "a".to_owned(),
                content: None,
            },
        ],
    };
    let http = ScriptedHttp::new()
        .route("a.mp3", Reply::ok(b"mp3-body".to_vec()))
        .route("a/large.jpg", Reply::ok(b"the-art".to_vec()));
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
    assert_eq!(outcome.artifacts_written, 1);
    assert_eq!(outcome.failed(), 0);
    // Fetched once, not twice.
    assert_eq!(http.count("a/large.jpg"), 1);
    // The sidecar carries the fetched bytes, and the audio was tagged.
    assert_eq!(fs.read_file("a/cover.jpg").unwrap(), b"the-art");
    assert_eq!(&fs.read_file("a.mp3").unwrap()[..3], b"ID3");
}

#[test]
fn concurrent_downloads_reuse_each_clips_own_cover() {
    // Two clips render concurrently; each `.jpg` sidecar gets its own cover
    // (no cross-contamination) and each cover URL is fetched exactly once.
    let a = art_clip("a");
    let b = art_clip("b");
    let da = desired(a.clone(), AudioFormat::Mp3);
    let db = desired(b.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![
            Action::Download {
                clip: a.clone(),
                lineage: LineageContext::own_root(&a),
                path: da.path.clone(),
                format: AudioFormat::Mp3,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "a/cover.jpg".to_owned(),
                source_url: a.selected_image_url().unwrap().to_owned(),
                hash: "art".to_owned(),
                owner_id: "a".to_owned(),
                content: None,
            },
            Action::Download {
                clip: b.clone(),
                lineage: LineageContext::own_root(&b),
                path: db.path.clone(),
                format: AudioFormat::Mp3,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "b/cover.jpg".to_owned(),
                source_url: b.selected_image_url().unwrap().to_owned(),
                hash: "art".to_owned(),
                owner_id: "b".to_owned(),
                content: None,
            },
        ],
    };
    let http = ScriptedHttp::new()
        .route("a.mp3", Reply::ok(b"a-mp3".to_vec()))
        .route("b.mp3", Reply::ok(b"b-mp3".to_vec()))
        .route("a/large.jpg", Reply::ok(b"art-a".to_vec()))
        .route("b/large.jpg", Reply::ok(b"art-b".to_vec()));
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[da, db],
        &http,
        &fs,
        &StubFfmpeg::flac(),
        &RecordingClock::new(),
        &small_poll(),
    );

    assert_eq!(outcome.downloaded, 2);
    assert_eq!(outcome.artifacts_written, 2);
    assert_eq!(http.count("a/large.jpg"), 1);
    assert_eq!(http.count("b/large.jpg"), 1);
    assert_eq!(fs.read_file("a/cover.jpg").unwrap(), b"art-a");
    assert_eq!(fs.read_file("b/cover.jpg").unwrap(), b"art-b");
}

#[test]
fn cover_sidecar_refetches_when_embed_fell_back_to_another_url() {
    // The large image 404s so the embed falls back to the small image; the
    // sidecar still wants the (dead) large URL and must NOT be handed the
    // small bytes. Reuse is keyed on the exact URL, so nothing is cached and
    // the sidecar fetches the large URL itself (then fails on the 404).
    let c = art_clip("e");
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![
            Action::Download {
                clip: c.clone(),
                lineage: LineageContext::own_root(&c),
                path: d.path.clone(),
                format: AudioFormat::Mp3,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "e/cover.jpg".to_owned(),
                source_url: "https://art.suno.ai/e/large.jpg".to_owned(),
                hash: "art".to_owned(),
                owner_id: "e".to_owned(),
                content: None,
            },
        ],
    };
    let http = ScriptedHttp::new()
        .route("e.mp3", Reply::ok(b"body".to_vec()))
        .route("e/large.jpg", Reply::status(404))
        .route("e/small.jpg", Reply::ok(b"small-art".to_vec()));
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
    // The small image was fetched once (the embed fallback) and never reused
    // for the large-keyed sidecar; the sidecar went to the network itself.
    assert_eq!(http.count("e/small.jpg"), 1);
    assert!(
        http.count("e/large.jpg") >= 2,
        "sidecar refetched the large URL"
    );
    assert_eq!(manifest.get("e").unwrap().cover_jpg, None);
    assert!(!fs.exists("e/cover.jpg"));
}

// ── Atomic write and size verification (SYNC-13/14) ─────────────

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

// ── Reliability policy (SYNC-16/17) ─────────────────────────────

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

// ── Disk-full aborts the run (issue #17) ────────────────────────

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

// ── preserve marker (SYNC-8) ────────────────────────────────────

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

// ── Reformat / Retag / Rename / Delete / Skip ───────────────────

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

// ── Pure helpers ────────────────────────────────────────────────

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

fn fs_new() -> MemFs {
    MemFs::new()
}

// ── Skip refreshes the preserve marker (SYNC-8 cross-run) ────────

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

#[test]
fn flac_render_retries_a_rate_limited_wav_lookup() {
    let c = clip("rl");
    let d = desired(c.clone(), AudioFormat::Flac);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Flac,
        }],
    };
    let http = ScriptedHttp::new()
        .with_auth()
        .route_seq(
            "/wav_file/",
            vec![
                Reply::status(429),
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/rl.wav"}"#),
            ],
        )
        .route("rl.wav", Reply::ok(b"wav".to_vec()));
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
    // The render was ready on retry, so no fresh convert_wav was needed.
    assert_eq!(http.count("/convert_wav/"), 0);
    // One transient backoff (1s base), not the 5s poll interval.
    assert_eq!(clock.sleeps(), vec![Duration::from_secs(1)]);
}

// ── Phase 6: artifact actions ───────────────────────────────────

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

#[test]
fn download_embeds_animated_webp_cover_when_enabled() {
    // With animated covers on and a video preview present, the audio embeds
    // the transcoded WebP (image/webp) as its front cover, not the static JPEG.
    let c = Clip {
        video_cover_url: "https://cdn.suno.ai/a/video.mp4".to_owned(),
        ..art_clip("a")
    };
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
        .route("a/video.mp4", Reply::ok(b"mp4-bytes".to_vec()))
        .route("a/large.jpg", Reply::ok(b"static-jpg".to_vec()));
    let fs = MemFs::new();
    let opts = ExecOptions {
        embed_animated_cover: true,
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
        &RecordingClock::new(),
        &opts,
    );

    assert_eq!(outcome.downloaded, 1);
    assert_eq!(outcome.failed(), 0);
    let written = fs.read_file("a.mp3").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(&written)).unwrap();
    let pic = tag.pictures().next().expect("embedded cover");
    assert_eq!(pic.mime_type, "image/webp");
    assert!(
        pic.data.starts_with(b"RIFF"),
        "the embedded cover is the transcoded WebP"
    );
    // The MP4 preview was fetched and transcoded; the static JPEG was not needed.
    assert_eq!(http.count("a/video.mp4"), 1);
    assert_eq!(http.count("a/large.jpg"), 0);
}

#[test]
fn download_keeps_static_jpeg_cover_when_embed_disabled() {
    // With the feature off (default), even a clip with a video preview embeds
    // the static JPEG and never fetches or transcodes the MP4.
    let c = Clip {
        video_cover_url: "https://cdn.suno.ai/a/video.mp4".to_owned(),
        ..art_clip("a")
    };
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
        .route("a/large.jpg", Reply::ok(b"static-jpg".to_vec()));
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

    assert_eq!(outcome.failed(), 0);
    let written = fs.read_file("a.mp3").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(&written)).unwrap();
    let pic = tag.pictures().next().expect("embedded cover");
    assert_eq!(pic.mime_type, "image/jpeg");
    assert_eq!(pic.data, b"static-jpg");
    assert_eq!(http.count("a/video.mp4"), 0);
}

#[test]
fn oversized_animated_cover_falls_back_to_jpeg_embed() {
    // A transcoded WebP that would overflow the FLAC picture cap is not
    // embedded; the audio falls back to the static JPEG so the file stays
    // valid (and no re-tag loop, since the intent hash is unchanged).
    let c = Clip {
        video_cover_url: "https://cdn.suno.ai/a/video.mp4".to_owned(),
        ..art_clip("a")
    };
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
        .route("a/video.mp4", Reply::ok(b"mp4-bytes".to_vec()))
        .route("a/large.jpg", Reply::ok(b"static-jpg".to_vec()));
    let fs = MemFs::new();
    let oversize = vec![b'R'; flac_picture_data_budget("image/webp") + 1];
    let ffmpeg = StubFfmpeg::flac().with_webp(oversize);
    let opts = ExecOptions {
        embed_animated_cover: true,
        ..ExecOptions::default()
    };
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &ffmpeg,
        &RecordingClock::new(),
        &opts,
    );

    assert_eq!(outcome.failed(), 0);
    let written = fs.read_file("a.mp3").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(&written)).unwrap();
    let pic = tag.pictures().next().expect("embedded cover");
    assert_eq!(pic.mime_type, "image/jpeg");
    assert_eq!(pic.data, b"static-jpg");
}

#[test]
fn cover_transcode_failure_falls_back_to_jpeg_embed() {
    // A non-systemic MP4 fetch/transcode failure never fails the audio: the
    // embed falls back to the static JPEG, best-effort like a failed cover
    // fetch, and the run completes.
    let c = Clip {
        video_cover_url: "https://cdn.suno.ai/a/video.mp4".to_owned(),
        ..art_clip("a")
    };
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
        .route("a/video.mp4", Reply::ok(b"mp4-bytes".to_vec()))
        .route("a/large.jpg", Reply::ok(b"static-jpg".to_vec()));
    let fs = MemFs::new();
    let opts = ExecOptions {
        embed_animated_cover: true,
        ..ExecOptions::default()
    };
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::failing(),
        &RecordingClock::new(),
        &opts,
    );

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.failed(), 0);
    let written = fs.read_file("a.mp3").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(&written)).unwrap();
    assert_eq!(tag.pictures().next().unwrap().mime_type, "image/jpeg");
}

#[test]
fn disk_full_cover_transcode_aborts_the_run() {
    // A full scratch disk during the cover transcode is systemic: it aborts
    // the run (exit 9) rather than silently skipping the cover.
    let c = Clip {
        video_cover_url: "https://cdn.suno.ai/a/video.mp4".to_owned(),
        ..art_clip("a")
    };
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
        .route("a/video.mp4", Reply::ok(b"mp4-bytes".to_vec()));
    let fs = MemFs::new();
    let opts = ExecOptions {
        embed_animated_cover: true,
        ..ExecOptions::default()
    };
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::out_of_space(),
        &RecordingClock::new(),
        &opts,
    );

    assert_eq!(outcome.status, RunStatus::DiskFull);
}

#[test]
fn video_only_clip_never_embeds_the_mp4_as_a_cover() {
    // A clip with a video preview but no static image must never embed the
    // MP4 bytes as a picture: when the WebP transcode fails and there is no
    // static image to fall back to, the audio is written with no cover.
    let c = Clip {
        video_cover_url: "https://cdn.suno.ai/a/video.mp4".to_owned(),
        ..clip("a")
    };
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
        .route("a/video.mp4", Reply::ok(b"mp4-bytes".to_vec()));
    let fs = MemFs::new();
    let opts = ExecOptions {
        embed_animated_cover: true,
        ..ExecOptions::default()
    };
    let mut manifest = Manifest::new();

    let outcome = run(
        &plan,
        &mut manifest,
        &[d],
        &http,
        &fs,
        &StubFfmpeg::failing(),
        &RecordingClock::new(),
        &opts,
    );

    assert_eq!(outcome.failed(), 0);
    let written = fs.read_file("a.mp3").unwrap();
    let tag = id3::Tag::read_from2(std::io::Cursor::new(&written)).unwrap();
    assert!(
        tag.pictures().next().is_none(),
        "no cover embedded, never the MP4"
    );
    assert!(
        !written
            .windows(b"mp4-bytes".len())
            .any(|w| w == b"mp4-bytes"),
        "the MP4 bytes must not be embedded as artwork"
    );
}

#[test]
fn embed_uses_configured_webp_settings() {
    use std::sync::{Arc, Mutex};

    struct RecordingWebpFfmpeg {
        seen: Arc<Mutex<Vec<WebpEncodeSettings>>>,
    }

    impl Ffmpeg for RecordingWebpFfmpeg {
        async fn wav_to_lossless(
            &self,
            _wav: &[u8],
            _format: AudioFormat,
        ) -> Result<Vec<u8>, crate::ffmpeg::FfmpegError> {
            Ok(Vec::new())
        }

        async fn mp4_to_webp(
            &self,
            _mp4: &[u8],
            settings: WebpEncodeSettings,
        ) -> Result<Vec<u8>, crate::ffmpeg::FfmpegError> {
            let seen = Arc::clone(&self.seen);
            seen.lock().unwrap().push(settings);
            Ok(b"RIFF\x00\x00\x00\x00WEBP".to_vec())
        }
    }

    let c = Clip {
        video_cover_url: "https://cdn.suno.ai/a/video.mp4".to_owned(),
        ..art_clip("a")
    };
    let d = desired(c.clone(), AudioFormat::Mp3);
    let plan = Plan {
        actions: vec![Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format: AudioFormat::Mp3,
        }],
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let ffmpeg = RecordingWebpFfmpeg {
        seen: Arc::clone(&seen),
    };
    let opts = ExecOptions {
        embed_animated_cover: true,
        cover_webp: WebpEncodeSettings {
            quality: 88,
            max_fps: 12,
            max_width: Some(720),
            lossless: false,
            compression_level: 4,
        },
        ..ExecOptions::default()
    };

    let _ = run(
        &plan,
        &mut Manifest::new(),
        &[d],
        &ScriptedHttp::new()
            .route("a.mp3", Reply::ok(b"mp3-body".to_vec()))
            .route("a/video.mp4", Reply::ok(b"mp4-bytes".to_vec())),
        &MemFs::new(),
        &ffmpeg,
        &RecordingClock::new(),
        &opts,
    );

    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[WebpEncodeSettings {
            quality: 88,
            max_fps: 12,
            max_width: Some(720),
            lossless: false,
            compression_level: 4,
        }]
    );
}

// ── Phase 8: folder art routes to the album store ───────────────

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

// ── Phase 9: playlist artifacts ─────────────────────────────────

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

// ── Phase 10: old-sidecar cleanup on move + empty-dir prune ──────

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

// ── Concurrency (issue #22) ─────────────────────────────────────

mod concurrency {
    use super::*;
    use crate::ffmpeg::FfmpegError;
    use crate::fs::{FileStat, FsError};
    use crate::http::{HttpRequest, TransportError};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    /// A future that pends exactly once before resolving, waking itself so a
    /// single-threaded executor re-polls. It forces the [`Http`] port to
    /// yield, so [`buffer_unordered`](futures_util::stream::StreamExt) parks
    /// each in-flight request and the true overlap becomes observable.
    #[derive(Default)]
    struct YieldOnce {
        yielded: bool,
    }

    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.yielded {
                Poll::Ready(())
            } else {
                self.yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    /// An [`Http`] double that wraps [`ScriptedHttp`] and records the peak
    /// number of concurrently in-flight requests. Each `send` bumps a live
    /// counter, yields once (so peers can start), then delegates.
    struct GatedHttp {
        inner: ScriptedHttp,
        inflight: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl GatedHttp {
        fn new(inner: ScriptedHttp) -> Self {
            Self {
                inner,
                inflight: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }

        fn count(&self, needle: &str) -> usize {
            self.inner.count(needle)
        }
    }

    impl Http for GatedHttp {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            YieldOnce::default().await;
            let out = self.inner.send(request).await;
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            out
        }
    }

    fn download(id: &str, format: AudioFormat) -> (Clip, Desired, Action) {
        let c = clip(id);
        let d = desired(c.clone(), format);
        let action = Action::Download {
            clip: c.clone(),
            lineage: LineageContext::own_root(&c),
            path: d.path.clone(),
            format,
        };
        (c, d, action)
    }

    fn opts_with(concurrency: u32) -> ExecOptions {
        ExecOptions {
            concurrency,
            ..small_poll()
        }
    }

    #[test]
    fn concurrency_never_exceeds_the_configured_bound() {
        let count = 6;
        let concurrency = 3;
        let mut scripted = ScriptedHttp::new().with_auth();
        let mut actions = Vec::new();
        let mut desireds = Vec::new();
        for i in 0..count {
            let id = format!("c{i}");
            scripted = scripted.route(&format!("{id}.mp3"), Reply::ok(b"mp3-body".to_vec()));
            let (_c, d, action) = download(&id, AudioFormat::Mp3);
            actions.push(action);
            desireds.push(d);
        }
        let http = GatedHttp::new(scripted);
        let fs = MemFs::new();
        let plan = Plan { actions };
        let mut manifest = Manifest::new();

        let outcome = run_gated_fs(
            &plan,
            &mut manifest,
            &desireds,
            &http,
            &fs,
            &opts_with(concurrency),
        );

        assert_eq!(outcome.downloaded, count);
        assert!(
            http.peak() <= concurrency as usize,
            "peak {} exceeded the bound {concurrency}",
            http.peak()
        );
        assert_eq!(
            http.peak(),
            concurrency as usize,
            "expected the run to saturate the bound"
        );
    }

    /// Run a gated plan against a caller-supplied [`MemFs`], returning the
    /// outcome. The client is built here so the limiter can be inspected by
    /// the caller-facing variant below.
    fn run_gated_fs(
        plan: &Plan,
        manifest: &mut Manifest,
        desired: &[Desired],
        http: &GatedHttp,
        fs: &MemFs,
        opts: &ExecOptions,
    ) -> ExecOutcome {
        let ffmpeg = StubFfmpeg::flac();
        let clock = RecordingClock::new();
        let mut albums = BTreeMap::new();
        let mut playlists = BTreeMap::new();
        let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
        pollster::block_on(execute(
            plan,
            manifest,
            &mut albums,
            &mut playlists,
            desired,
            &HashMap::new(),
            Ports {
                client: &client,
                http,
                fs,
                ffmpeg: &ffmpeg,
                clock: &clock,
            },
            opts,
        ))
    }

    #[test]
    fn a_failing_clip_does_not_abort_the_others() {
        let mut scripted = ScriptedHttp::new().with_auth();
        scripted = scripted
            .route("ok1.mp3", Reply::ok(b"one".to_vec()))
            .route("bad.mp3", Reply::status(404))
            .route("ok2.mp3", Reply::ok(b"two".to_vec()));
        let (_a, d1, a1) = download("ok1", AudioFormat::Mp3);
        let (_b, d2, a2) = download("bad", AudioFormat::Mp3);
        let (_c, d3, a3) = download("ok2", AudioFormat::Mp3);
        let http = GatedHttp::new(scripted);
        let fs = MemFs::new();
        let plan = Plan {
            actions: vec![a1, a2, a3],
        };
        let mut manifest = Manifest::new();

        let outcome = run_gated_fs(
            &plan,
            &mut manifest,
            &[d1, d2, d3],
            &http,
            &fs,
            &opts_with(3),
        );

        assert_eq!(outcome.downloaded, 2);
        assert_eq!(outcome.failed(), 1);
        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(outcome.failures[0].clip_id, "bad");
        assert!(manifest.get("ok1").is_some());
        assert!(manifest.get("ok2").is_some());
        assert!(manifest.get("bad").is_none());
    }

    #[test]
    fn outcome_is_identical_across_concurrency_levels() {
        // A plan mixing successful and failing downloads with serial phase-2
        // actions (a skip and a delete), so both phases contribute.
        fn build() -> (Plan, Vec<Desired>) {
            let mut actions = Vec::new();
            let mut desireds = Vec::new();
            for id in ["a", "b", "c", "d"] {
                let (_c, d, action) = download(id, AudioFormat::Mp3);
                actions.push(action);
                desireds.push(d);
            }
            // A failing download in the middle of the audio set.
            let (_e, de, ae) = download("fail", AudioFormat::Mp3);
            actions.insert(2, ae);
            desireds.push(de);
            // Phase-2 actions.
            actions.push(Action::Skip {
                clip_id: "gone".to_owned(),
            });
            actions.push(Action::Delete {
                path: "old.mp3".to_owned(),
                clip_id: "old".to_owned(),
            });
            (Plan { actions }, desireds)
        }

        fn http() -> ScriptedHttp {
            ScriptedHttp::new()
                .with_auth()
                .route("a.mp3", Reply::ok(b"a".to_vec()))
                .route("b.mp3", Reply::ok(b"b".to_vec()))
                .route("c.mp3", Reply::ok(b"c".to_vec()))
                .route("d.mp3", Reply::ok(b"d".to_vec()))
                .route("fail.mp3", Reply::status(404))
        }

        fn seed_manifest() -> Manifest {
            let mut m = Manifest::new();
            m.insert("old".to_owned(), entry("old.mp3", AudioFormat::Mp3));
            m
        }

        let (plan, desireds) = build();

        let mut m1 = seed_manifest();
        let fs1 = MemFs::new().with_file("old.mp3", b"x".to_vec());
        let out1 = run_gated_fs(
            &plan,
            &mut m1,
            &desireds,
            &GatedHttp::new(http()),
            &fs1,
            &opts_with(1),
        );

        let mut m8 = seed_manifest();
        let fs8 = MemFs::new().with_file("old.mp3", b"x".to_vec());
        let out8 = run_gated_fs(
            &plan,
            &mut m8,
            &desireds,
            &GatedHttp::new(http()),
            &fs8,
            &opts_with(8),
        );

        assert_eq!(out1, out8, "outcome must not depend on concurrency");
        assert_eq!(m1, m8, "final manifest must not depend on concurrency");
        assert_eq!(out8.downloaded, 4);
        assert_eq!(out8.deleted, 1);
        assert_eq!(out8.skipped, 1);
        assert_eq!(out8.failed(), 1);
    }

    #[test]
    fn a_systemic_disk_full_aborts_promptly() {
        let count = 8;
        let concurrency = 2;
        let mut scripted = ScriptedHttp::new().with_auth();
        let mut actions = Vec::new();
        let mut desireds = Vec::new();
        for i in 0..count {
            let id = format!("d{i}");
            scripted = scripted.route(&format!("{id}.mp3"), Reply::ok(b"mp3-body".to_vec()));
            let (_c, d, action) = download(&id, AudioFormat::Mp3);
            actions.push(action);
            desireds.push(d);
        }
        // The very first clip's write hits ENOSPC, a systemic failure.
        let fs = MemFs::new().fail_write_out_of_space("d0.mp3");
        let http = GatedHttp::new(scripted);
        let plan = Plan { actions };
        let mut manifest = Manifest::new();

        let outcome = run_gated_fs(
            &plan,
            &mut manifest,
            &desireds,
            &http,
            &fs,
            &opts_with(concurrency),
        );

        assert_eq!(outcome.status, RunStatus::DiskFull);
        assert!(
            outcome.downloaded < count,
            "a systemic abort must stop remaining work, downloaded {}",
            outcome.downloaded
        );
    }

    #[test]
    fn limiter_records_a_rate_limit_under_concurrent_calls() {
        // Three concurrent FLAC renders; exactly one clip is throttled once
        // on its wav_file read. The shared limiter must record that single
        // 429 (halving 2.0 -> 1.0) with no lost or duplicated update, proving
        // the mutex keeps the AIMD state correct under concurrency.
        let scripted = ScriptedHttp::new()
            .with_auth()
            .route_seq(
                "/gen/x/wav_file/",
                vec![
                    Reply::status(429),
                    Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/x.wav"}"#),
                ],
            )
            .route(
                "/gen/y/wav_file/",
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/y.wav"}"#),
            )
            .route(
                "/gen/z/wav_file/",
                Reply::json(r#"{"wav_file_url": "https://cdn1.suno.ai/z.wav"}"#),
            )
            .route("x.wav", Reply::ok(b"wav-x".to_vec()))
            .route("y.wav", Reply::ok(b"wav-y".to_vec()))
            .route("z.wav", Reply::ok(b"wav-z".to_vec()));

        let mut actions = Vec::new();
        let mut desireds = Vec::new();
        for id in ["x", "y", "z"] {
            let (_c, d, action) = download(id, AudioFormat::Flac);
            actions.push(action);
            desireds.push(d);
        }
        let plan = Plan { actions };
        let fs = MemFs::new();
        let ffmpeg = StubFfmpeg::flac();
        let clock = RecordingClock::new();
        let mut albums = BTreeMap::new();
        let mut playlists = BTreeMap::new();
        let mut manifest = Manifest::new();
        let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());

        let outcome = pollster::block_on(execute(
            &plan,
            &mut manifest,
            &mut albums,
            &mut playlists,
            &desireds,
            &HashMap::new(),
            Ports {
                client: &client,
                http: &scripted,
                fs: &fs,
                ffmpeg: &ffmpeg,
                clock: &clock,
            },
            &opts_with(3),
        ));

        assert_eq!(outcome.downloaded, 3);
        assert_eq!(outcome.failed(), 0);
        assert!(
            (client.limiter_rate() - 1.0).abs() < 1e-9,
            "one 429 must halve the rate to 1.0, got {}",
            client.limiter_rate()
        );
    }

    #[test]
    fn a_download_is_committed_in_plan_order_around_a_rename() {
        // Plan order: rename "orig" away from shared.mp3 first, then download
        // a new clip into shared.mp3. A parallel executor that performed the
        // download's destination write off plan order would write shared.mp3
        // before the rename ran, letting the rename carry those fresh bytes
        // to moved.mp3 and stranding shared.mp3 - corrupting both clips.
        // Committing every destination effect serially in plan order keeps
        // moved.mp3 = the original and shared.mp3 = the new download.
        let c_new = clip("new");
        let mut d_new = desired(c_new.clone(), AudioFormat::Mp3);
        d_new.path = "shared.mp3".to_owned();
        let plan = Plan {
            actions: vec![
                Action::Rename {
                    from: "shared.mp3".to_owned(),
                    to: "moved.mp3".to_owned(),
                },
                Action::Download {
                    clip: c_new.clone(),
                    lineage: LineageContext::own_root(&c_new),
                    path: "shared.mp3".to_owned(),
                    format: AudioFormat::Mp3,
                },
            ],
        };
        let scripted = ScriptedHttp::new()
            .with_auth()
            .route("new.mp3", Reply::ok(b"NEW-BODY".to_vec()));
        let http = GatedHttp::new(scripted);
        let fs = MemFs::new().with_file("shared.mp3", b"ORIGINAL".to_vec());
        let mut manifest = Manifest::new();
        manifest.insert("orig", entry("shared.mp3", AudioFormat::Mp3));

        let outcome = run_gated_fs(&plan, &mut manifest, &[d_new], &http, &fs, &opts_with(4));

        assert_eq!(outcome.renamed, 1);
        assert_eq!(outcome.downloaded, 1);
        assert_eq!(
            fs.read_file("moved.mp3").as_deref(),
            Some(&b"ORIGINAL"[..]),
            "the rename must carry the original bytes, untouched by the download"
        );
        let landed = fs.read_file("shared.mp3").expect("new download must land");
        assert_ne!(
            landed, b"ORIGINAL",
            "the new download must replace the moved original, not corrupt it"
        );
        assert_eq!(manifest.get("orig").unwrap().path, "moved.mp3");
        assert_eq!(manifest.get("new").unwrap().path, "shared.mp3");
    }

    #[test]
    fn an_aborted_reformat_leaves_the_old_file_and_manifest_consistent() {
        // A systemic disk-full abort strikes the download committed before the
        // reformat. Because the reformat's slow render is side-effect-free and
        // its destination write + old-file removal only happen in the serial
        // commit (which the abort skips), the old file survives and the
        // manifest still points at it: no removed-but-referenced file.
        let boom = clip("boom");
        let mut d_boom = desired(boom.clone(), AudioFormat::Mp3);
        d_boom.path = "boom.mp3".to_owned();
        let reformer = clip("r");
        let d_reformer = desired(reformer.clone(), AudioFormat::Mp3);
        let plan = Plan {
            actions: vec![
                Action::Download {
                    clip: boom.clone(),
                    lineage: LineageContext::own_root(&boom),
                    path: "boom.mp3".to_owned(),
                    format: AudioFormat::Mp3,
                },
                Action::Reformat {
                    clip: reformer.clone(),
                    path: "r_new.mp3".to_owned(),
                    from_path: "r_old.flac".to_owned(),
                    from: AudioFormat::Flac,
                    to: AudioFormat::Mp3,
                },
            ],
        };
        let scripted = ScriptedHttp::new()
            .with_auth()
            .route("boom.mp3", Reply::ok(b"boom-body".to_vec()))
            .route("r.mp3", Reply::ok(b"reformatted".to_vec()));
        let http = GatedHttp::new(scripted);
        // The download's write hits ENOSPC, a systemic abort.
        let fs = MemFs::new()
            .with_file("r_old.flac", b"OLD-FLAC".to_vec())
            .fail_write_out_of_space("boom.mp3");
        let mut manifest = Manifest::new();
        manifest.insert("r", entry("r_old.flac", AudioFormat::Flac));

        let outcome = run_gated_fs(
            &plan,
            &mut manifest,
            &[d_boom, d_reformer],
            &http,
            &fs,
            &opts_with(4),
        );

        assert_eq!(outcome.status, RunStatus::DiskFull);
        assert!(
            fs.exists("r_old.flac"),
            "the old file must survive the abort"
        );
        assert!(
            !fs.exists("r_new.mp3"),
            "no reformatted file may be written"
        );
        let still = manifest.get("r").expect("the manifest must still track r");
        assert_eq!(
            still.path, "r_old.flac",
            "the manifest must still point at the surviving old file"
        );
        assert_eq!(still.format, AudioFormat::Flac);
    }

    #[test]
    fn a_systemic_abort_leaves_no_untracked_destination_files() {
        // Two clips commit, the third's write hits ENOSPC (a systemic abort),
        // and the rest never commit. Every file remaining on disk must be one
        // the manifest tracks: producers write nothing, so an abort cannot
        // strand an untracked file from an in-flight or buffered render.
        let mut scripted = ScriptedHttp::new().with_auth();
        let mut actions = Vec::new();
        let mut desireds = Vec::new();
        for id in ["a0", "a1", "boom", "a3", "a4"] {
            scripted = scripted.route(&format!("{id}.mp3"), Reply::ok(b"body".to_vec()));
            let (_c, d, action) = download(id, AudioFormat::Mp3);
            actions.push(action);
            desireds.push(d);
        }
        let http = GatedHttp::new(scripted);
        let fs = MemFs::new().fail_write_out_of_space("boom.mp3");
        let plan = Plan { actions };
        let mut manifest = Manifest::new();

        let outcome = run_gated_fs(&plan, &mut manifest, &desireds, &http, &fs, &opts_with(2));

        assert_eq!(outcome.status, RunStatus::DiskFull);
        let tracked: std::collections::BTreeSet<String> = manifest
            .entries
            .values()
            .map(|entry| entry.path.clone())
            .collect();
        for path in fs.paths() {
            assert!(
                tracked.contains(&path),
                "found an untracked destination file: {path}"
            );
        }
        assert!(
            !fs.exists("a3.mp3"),
            "uncommitted renders must not be on disk"
        );
        assert!(
            !fs.exists("a4.mp3"),
            "uncommitted renders must not be on disk"
        );
    }

    /// An [`Ffmpeg`] double that counts how many rendered FLAC payloads are
    /// live: it bumps a shared counter (tracking the peak) when a transcode
    /// yields bytes, and [`CountingFs`] drops it back on the committing write.
    /// The [transcode, write] window is a superset of the true in-memory hold,
    /// so the observed peak upper-bounds the real one.
    struct CountingFfmpeg {
        inner: StubFfmpeg,
        held: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl Ffmpeg for CountingFfmpeg {
        fn wav_to_lossless(
            &self,
            wav: &[u8],
            format: AudioFormat,
        ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send {
            let fut = self.inner.wav_to_lossless(wav, format);
            let held = self.held.clone();
            let peak = self.peak.clone();
            async move {
                let out = fut.await;
                if out.is_ok() {
                    let now = held.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                }
                out
            }
        }

        fn mp4_to_webp(
            &self,
            mp4: &[u8],
            settings: WebpEncodeSettings,
        ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send {
            self.inner.mp4_to_webp(mp4, settings)
        }
    }

    /// A [`Filesystem`] double wrapping [`MemFs`] that decrements the live
    /// payload counter on each committing write, closing the window opened by
    /// [`CountingFfmpeg`].
    struct CountingFs {
        inner: MemFs,
        held: Arc<AtomicUsize>,
    }

    impl Filesystem for CountingFs {
        fn write_atomic(&self, path: &str, bytes: &[u8]) -> Result<(), FsError> {
            let out = self.inner.write_atomic(path, bytes);
            self.held.fetch_sub(1, Ordering::SeqCst);
            out
        }

        fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
            self.inner.rename(from, to)
        }

        fn remove(&self, path: &str) -> Result<(), FsError> {
            self.inner.remove(path)
        }

        fn prune_empty_dirs(&self, root: &str) -> Result<(), FsError> {
            self.inner.prune_empty_dirs(root)
        }

        fn read(&self, path: &str) -> Result<Vec<u8>, FsError> {
            self.inner.read(path)
        }

        fn metadata(&self, path: &str) -> Option<FileStat> {
            self.inner.metadata(path)
        }
    }

    #[test]
    fn rendered_payloads_in_memory_stay_bounded_by_concurrency() {
        // Far more FLAC clips than the concurrency bound. The ordered buffered
        // render keeps at most about `concurrency` transcoded payloads live at
        // once (never the whole library), so peak held <= concurrency + 1.
        let count = 12;
        let concurrency = 3;
        let mut scripted = ScriptedHttp::new().with_auth();
        let mut actions = Vec::new();
        let mut desireds = Vec::new();
        for i in 0..count {
            let id = format!("f{i}");
            scripted = scripted
                .route(
                    &format!("/gen/{id}/wav_file/"),
                    Reply::json(&format!(
                        r#"{{"wav_file_url": "https://cdn1.suno.ai/{id}.wav"}}"#
                    )),
                )
                .route(&format!("{id}.wav"), Reply::ok(b"wav-body".to_vec()));
            let (_c, d, action) = download(&id, AudioFormat::Flac);
            actions.push(action);
            desireds.push(d);
        }
        let http = GatedHttp::new(scripted);
        let held = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let ffmpeg = CountingFfmpeg {
            inner: StubFfmpeg::flac(),
            held: held.clone(),
            peak: peak.clone(),
        };
        let fs = CountingFs {
            inner: MemFs::new(),
            held: held.clone(),
        };
        let clock = RecordingClock::new();
        let mut albums = BTreeMap::new();
        let mut playlists = BTreeMap::new();
        let mut manifest = Manifest::new();
        let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
        let plan = Plan { actions };

        let outcome = pollster::block_on(execute(
            &plan,
            &mut manifest,
            &mut albums,
            &mut playlists,
            &desireds,
            &HashMap::new(),
            Ports {
                client: &client,
                http: &http,
                fs: &fs,
                ffmpeg: &ffmpeg,
                clock: &clock,
            },
            &opts_with(concurrency),
        ));

        assert_eq!(outcome.downloaded, count as usize);
        assert_eq!(
            held.load(Ordering::SeqCst),
            0,
            "every payload must be committed"
        );
        assert!(
            peak.load(Ordering::SeqCst) <= concurrency as usize + 1,
            "peak live payloads {} exceeded the bound {}",
            peak.load(Ordering::SeqCst),
            concurrency + 1
        );
        assert!(
            peak.load(Ordering::SeqCst) >= 2,
            "the render should genuinely overlap, peak was {}",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn artifact_fetches_run_concurrently() {
        // Four CoverJpg sidecars whose owning clips are already in the manifest.
        // With concurrency=2 the two HTTP fetches should overlap, so the peak
        // in-flight count must reach at least 2.
        let count = 4usize;
        let concurrency = 2u32;
        let mut scripted = ScriptedHttp::new().with_auth();
        let mut actions = Vec::new();
        let mut manifest = Manifest::new();
        for i in 0..count {
            let id = format!("a{i}");
            scripted = scripted.route(&format!("{id}.jpg"), Reply::ok(b"jpg-bytes".to_vec()));
            manifest.insert(&id, entry(&format!("{id}.mp3"), AudioFormat::Mp3));
            actions.push(Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: format!("{id}/cover.jpg"),
                source_url: format!("https://art.suno.ai/{id}.jpg"),
                hash: format!("h{i}"),
                owner_id: id,
                content: None,
            });
        }
        let http = GatedHttp::new(scripted);
        let fs = MemFs::new();
        let plan = Plan { actions };

        let outcome = run_gated_fs(
            &plan,
            &mut manifest,
            &[],
            &http,
            &fs,
            &opts_with(concurrency),
        );

        assert_eq!(outcome.artifacts_written, count);
        assert_eq!(outcome.failed(), 0);
        assert!(
            http.peak() >= concurrency as usize,
            "artifact fetches must overlap: peak {} < concurrency {}",
            http.peak(),
            concurrency,
        );
    }

    #[test]
    fn stem_fetches_run_concurrently() {
        // Four Mp3 stem fetches whose owning clips are in the manifest.
        // With concurrency=2 the peak in-flight HTTP count must reach at least 2.
        let count = 4usize;
        let concurrency = 2u32;
        let mut scripted = ScriptedHttp::new().with_auth();
        let mut actions = Vec::new();
        let mut manifest = Manifest::new();
        for i in 0..count {
            let id = format!("s{i}");
            scripted = scripted.route(&format!("{id}voc.mp3"), Reply::ok(b"stem-bytes".to_vec()));
            manifest.insert(&id, entry(&format!("{id}.mp3"), AudioFormat::Mp3));
            actions.push(Action::WriteStem {
                clip_id: id.clone(),
                key: "voc".to_owned(),
                stem_id: format!("{id}voc"),
                path: format!("{id}.stems/voc.mp3"),
                source_url: format!("https://cdn1.suno.ai/{id}voc.mp3"),
                format: StemFormat::Mp3,
                hash: format!("h{i}"),
            });
        }
        let http = GatedHttp::new(scripted);
        let fs = MemFs::new();
        let plan = Plan { actions };

        let outcome = run_gated_fs(
            &plan,
            &mut manifest,
            &[],
            &http,
            &fs,
            &opts_with(concurrency),
        );

        assert_eq!(outcome.artifacts_written, count);
        assert_eq!(outcome.failed(), 0);
        assert!(
            http.peak() >= concurrency as usize,
            "stem fetches must overlap: peak {} < concurrency {}",
            http.peak(),
            concurrency,
        );
    }

    #[test]
    fn prepareable_outcome_is_identical_across_concurrency_levels_with_artifacts_and_stems() {
        // A plan mixing downloads, artifact writes, and stem writes. Both a
        // failing clip and a serial-only action (delete) are included so all
        // code paths contribute. Outcome and final manifest must be the same
        // whether concurrency is 1 or 8, proving commits remain serial and
        // deterministic while preparation runs in parallel.
        fn build() -> (Plan, Vec<Desired>) {
            let mut actions = Vec::new();
            let mut desireds = Vec::new();
            for id in ["x", "y", "z"] {
                let (_c, d, action) = download(id, AudioFormat::Mp3);
                desireds.push(d);
                actions.push(action);
                // A CoverJpg sidecar for each clip.
                actions.push(Action::WriteArtifact {
                    kind: ArtifactKind::CoverJpg,
                    path: format!("{id}/cover.jpg"),
                    source_url: format!("https://art.suno.ai/{id}.jpg"),
                    hash: format!("art-{id}"),
                    owner_id: id.to_owned(),
                    content: None,
                });
                // An Mp3 stem for each clip.
                actions.push(Action::WriteStem {
                    clip_id: id.to_owned(),
                    key: "voc".to_owned(),
                    stem_id: format!("{id}voc"),
                    path: format!("{id}.stems/voc.mp3"),
                    source_url: format!("https://cdn1.suno.ai/{id}voc.mp3"),
                    format: StemFormat::Mp3,
                    hash: format!("stem-{id}"),
                });
            }
            // A failing download in the middle.
            let (_f, df, af) = download("fail", AudioFormat::Mp3);
            desireds.push(df);
            actions.insert(3, af);
            // A serial-only delete.
            actions.push(Action::Delete {
                path: "old.mp3".to_owned(),
                clip_id: "old".to_owned(),
            });
            (Plan { actions }, desireds)
        }

        fn http() -> ScriptedHttp {
            ScriptedHttp::new()
                .with_auth()
                .route("x.mp3", Reply::ok(b"x-audio".to_vec()))
                .route("y.mp3", Reply::ok(b"y-audio".to_vec()))
                .route("z.mp3", Reply::ok(b"z-audio".to_vec()))
                .route("fail.mp3", Reply::status(404))
                .route("x.jpg", Reply::ok(b"x-jpg".to_vec()))
                .route("y.jpg", Reply::ok(b"y-jpg".to_vec()))
                .route("z.jpg", Reply::ok(b"z-jpg".to_vec()))
                .route("xvoc.mp3", Reply::ok(b"x-voc".to_vec()))
                .route("yvoc.mp3", Reply::ok(b"y-voc".to_vec()))
                .route("zvoc.mp3", Reply::ok(b"z-voc".to_vec()))
        }

        fn seed_manifest() -> Manifest {
            let mut m = Manifest::new();
            m.insert("old".to_owned(), entry("old.mp3", AudioFormat::Mp3));
            m
        }

        let (plan, desireds) = build();

        let mut m1 = seed_manifest();
        let fs1 = MemFs::new().with_file("old.mp3", b"x".to_vec());
        let out1 = run_gated_fs(
            &plan,
            &mut m1,
            &desireds,
            &GatedHttp::new(http()),
            &fs1,
            &opts_with(1),
        );

        let mut m8 = seed_manifest();
        let fs8 = MemFs::new().with_file("old.mp3", b"x".to_vec());
        let out8 = run_gated_fs(
            &plan,
            &mut m8,
            &desireds,
            &GatedHttp::new(http()),
            &fs8,
            &opts_with(8),
        );

        assert_eq!(out1, out8, "outcome must not depend on concurrency");
        assert_eq!(m1, m8, "final manifest must not depend on concurrency");
        assert_eq!(out8.downloaded, 3);
        assert_eq!(out8.deleted, 1);
        assert_eq!(out8.failed(), 1);
        // Covers and stems for the 3 successful clips.
        assert_eq!(out8.artifacts_written, 6);
    }

    #[test]
    fn both_folder_covers_fetch_video_cover_once_under_concurrency() {
        // FolderWebp and FolderMp4 share a source_url (the `both` retention).
        // Even with other downloads running concurrently, they must stay serial
        // so the first fetch inserts into cover_cache and the second drains it
        // (#90), fetching the video_cover_url exactly once.
        let scripted = ScriptedHttp::new()
            .with_auth()
            .route("root/video.mp4", Reply::ok(b"mp4-bytes".to_vec()))
            .route("d0.mp3", Reply::ok(b"audio".to_vec()))
            .route("d1.mp3", Reply::ok(b"audio".to_vec()));
        let mut actions = vec![
            Action::WriteArtifact {
                kind: ArtifactKind::FolderWebp,
                path: "album/cover.webp".to_owned(),
                source_url: "https://cdn.suno.ai/root/video.mp4".to_owned(),
                hash: "wh".to_owned(),
                owner_id: "root".to_owned(),
                content: None,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::FolderMp4,
                path: "album/cover.mp4".to_owned(),
                source_url: "https://cdn.suno.ai/root/video.mp4".to_owned(),
                hash: "mh".to_owned(),
                owner_id: "root".to_owned(),
                content: None,
            },
        ];
        let mut desireds = vec![];
        for id in ["d0", "d1"] {
            let (_c, d, a) = download(id, AudioFormat::Mp3);
            actions.push(a);
            desireds.push(d);
        }
        let plan = Plan { actions };
        let http = GatedHttp::new(scripted);
        let ffmpeg = StubFfmpeg::webp();
        let clock = RecordingClock::new();
        let mut manifest = Manifest::new();
        let mut albums = BTreeMap::new();
        let mut playlists = BTreeMap::new();
        let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
        pollster::block_on(execute(
            &plan,
            &mut manifest,
            &mut albums,
            &mut playlists,
            &desireds,
            &HashMap::new(),
            Ports {
                client: &client,
                http: &http,
                fs: &MemFs::new(),
                ffmpeg: &ffmpeg,
                clock: &clock,
            },
            &opts_with(4),
        ));

        assert_eq!(
            http.count("root/video.mp4"),
            1,
            "video_cover_url must be fetched exactly once even under concurrency"
        );
    }

    #[test]
    fn existing_clip_audio_and_cover_sidecar_share_cover_fetch() {
        // Clip "e" is already in the manifest; this run reformats its audio
        // AND updates its CoverJpg sidecar. The audio producer caches the
        // cover; the sidecar drains it. Even under concurrency the cover must
        // be fetched exactly once and cover_cache must not accumulate a
        // leaked entry.
        let c = art_clip("e");
        let cover_url = c.image_large_url.clone();
        let d = desired(c.clone(), AudioFormat::Mp3);
        let scripted = ScriptedHttp::new()
            .with_auth()
            .route("e.mp3", Reply::ok(b"audio".to_vec()))
            .route("e/large.jpg", Reply::ok(b"cover-jpg".to_vec()));
        let plan = Plan {
            actions: vec![
                Action::Reformat {
                    clip: c,
                    path: "e.mp3".to_owned(),
                    from_path: "e-old.mp3".to_owned(),
                    from: AudioFormat::Mp3,
                    to: AudioFormat::Mp3,
                },
                Action::WriteArtifact {
                    kind: ArtifactKind::CoverJpg,
                    path: "e/cover.jpg".to_owned(),
                    source_url: cover_url,
                    hash: "new-art".to_owned(),
                    owner_id: "e".to_owned(),
                    content: None,
                },
            ],
        };
        let mut manifest = Manifest::new();
        manifest.insert("e".to_owned(), entry("e-old.mp3", AudioFormat::Mp3));
        let fs = MemFs::new().with_file("e-old.mp3", b"old-audio".to_vec());
        let http = GatedHttp::new(scripted);
        let outcome = run_gated_fs(&plan, &mut manifest, &[d], &http, &fs, &opts_with(4));

        assert_eq!(outcome.reformatted, 1);
        assert_eq!(outcome.failed(), 0);
        assert_eq!(
            http.count("e/large.jpg"),
            1,
            "cover must be fetched exactly once, not once per concurrent action"
        );
    }
}
