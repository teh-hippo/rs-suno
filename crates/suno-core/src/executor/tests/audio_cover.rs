use super::*;

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
