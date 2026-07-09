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
