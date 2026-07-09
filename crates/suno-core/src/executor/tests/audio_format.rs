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
