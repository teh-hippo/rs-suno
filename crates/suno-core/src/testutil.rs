//! Test-only in-memory doubles for the engine's ports.
//!
//! [`MockHttp`] is the original first-match HTTP double used by the client and
//! auth tests. The download executor needs more: binary bodies, response
//! headers (for `Content-Length` and `Retry-After`), scripted sequences (so a
//! request can fail then succeed), and a call log. [`ScriptedHttp`] provides
//! that, alongside an in-memory [`Filesystem`] ([`MemFs`]), a stub [`Ffmpeg`]
//! ([`StubFfmpeg`]), and a recording [`Clock`] ([`RecordingClock`]) that never
//! really sleeps, so executor tests stay deterministic.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::clock::Clock;
use crate::ffmpeg::{Ffmpeg, FfmpegError};
use crate::fs::{FileStat, Filesystem, FsError};
use crate::http::{Http, HttpRequest, HttpResponse, TransportError};
use crate::lineage::{EdgeType, LineageContext, ResolveStatus};
use crate::model::Clip;
use crate::vocab::{AudioFormat, WebpEncodeSettings};

/// A canned reply for any request whose URL contains `url_contains`.
pub(crate) struct Rule {
    url_contains: &'static str,
    status: u16,
    body: String,
}

impl Rule {
    pub(crate) fn new(url_contains: &'static str, status: u16, body: String) -> Self {
        Self {
            url_contains,
            status,
            body,
        }
    }
}

/// An [`Http`] double that replies from the first matching [`Rule`], in order.
pub(crate) struct MockHttp {
    rules: Vec<Rule>,
}

impl MockHttp {
    pub(crate) fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }
}

impl Http for MockHttp {
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send {
        let reply = self
            .rules
            .iter()
            .find(|rule| request.url.contains(rule.url_contains))
            .map(|rule| HttpResponse {
                status: rule.status,
                headers: Vec::new(),
                body: rule.body.clone().into_bytes(),
            })
            .ok_or_else(|| TransportError(format!("no rule matched {}", request.url)));
        async move { reply }
    }
}

/// A canned reply for [`ScriptedHttp`].
#[derive(Clone)]
pub(crate) struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    /// A `200 OK` carrying `body`.
    pub(crate) fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// A `200 OK` carrying a JSON string body.
    pub(crate) fn json(body: &str) -> Self {
        Self::ok(body.as_bytes().to_vec())
    }

    /// A bodyless reply with just `status`.
    pub(crate) fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A reply with `status` and `body`.
    pub(crate) fn with_body(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// Add a response header.
    pub(crate) fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Add a `Content-Length` header advertising `len` bytes.
    pub(crate) fn with_content_length(self, len: u64) -> Self {
        self.with_header("content-length", &len.to_string())
    }

    /// Add a `Retry-After` header of `seconds`.
    pub(crate) fn with_retry_after(self, seconds: u64) -> Self {
        self.with_header("retry-after", &seconds.to_string())
    }
}

/// One route: a URL substring and the queued replies for it.
struct Route {
    url_contains: String,
    replies: VecDeque<Reply>,
}

/// An [`Http`] double that replies from per-URL scripted sequences.
///
/// The first route whose substring is contained in the request URL answers. A
/// route with several queued replies pops one per call (the last repeats), so a
/// request can be made to fail then succeed. Every request URL is logged.
pub(crate) struct ScriptedHttp {
    routes: Mutex<Vec<Route>>,
    log: Mutex<Vec<String>>,
    bodies: Mutex<Vec<Vec<u8>>>,
}

impl ScriptedHttp {
    pub(crate) fn new() -> Self {
        Self {
            routes: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            bodies: Mutex::new(Vec::new()),
        }
    }

    /// Seed the Clerk auth routes so a [`SunoClient`](crate::SunoClient) built
    /// against this double can authenticate and mint JWTs. The sessions route
    /// is added first so it wins over the broader `/v1/client` match.
    pub(crate) fn with_auth(self) -> Self {
        let client_body = serde_json::json!({
            "response": {
                "last_active_session_id": "s",
                "sessions": [{"id": "s", "user": {"id": "u", "username": "h"}}]
            }
        })
        .to_string();
        self.route("/v1/client/sessions/", Reply::json(r#"{"jwt": "a.b.c"}"#))
            .route("/v1/client", Reply::json(&client_body))
    }

    /// Add a route that returns `reply` for every matching request.
    pub(crate) fn route(self, url_contains: &str, reply: Reply) -> Self {
        self.route_seq(url_contains, vec![reply])
    }

    /// Add a route that returns `replies` in order (the last one repeats).
    pub(crate) fn route_seq(self, url_contains: &str, replies: Vec<Reply>) -> Self {
        self.routes.lock().unwrap().push(Route {
            url_contains: url_contains.to_owned(),
            replies: replies.into(),
        });
        self
    }

    /// The URLs requested so far, in order.
    pub(crate) fn calls(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// The request bodies sent so far, in order, decoded as UTF-8 (empty for a
    /// GET or a bodyless POST). Feed pages all share one URL, so only the body
    /// proves the cursor was threaded from one page's `next_cursor` to the next.
    pub(crate) fn bodies(&self) -> Vec<String> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .map(|body| String::from_utf8_lossy(body).into_owned())
            .collect()
    }

    /// How many requested URLs contained `needle`.
    pub(crate) fn count(&self, needle: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|url| url.contains(needle))
            .count()
    }
}

impl Http for ScriptedHttp {
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send {
        self.log.lock().unwrap().push(request.url.clone());
        self.bodies.lock().unwrap().push(request.body.clone());
        let reply = {
            let mut routes = self.routes.lock().unwrap();
            routes
                .iter_mut()
                .find(|route| request.url.contains(&route.url_contains))
                .map(|route| {
                    if route.replies.len() > 1 {
                        route.replies.pop_front().expect("len checked")
                    } else {
                        route.replies.front().expect("route has no replies").clone()
                    }
                })
        };
        let out = match reply {
            Some(reply) => Ok(HttpResponse {
                status: reply.status,
                headers: reply.headers,
                body: reply.body,
            }),
            None => Err(TransportError(format!("no route matched {}", request.url))),
        };
        async move { out }
    }
}

/// An in-memory [`Filesystem`] double: a map of path to bytes, with optional
/// fault injection for the executor's safety paths.
///
/// Directories are modelled explicitly in `dirs` (a real filesystem tracks them
/// independently of files), so [`prune_empty_dirs`](Filesystem::prune_empty_dirs)
/// can be exercised: a write, rename, or seed registers the target's ancestor
/// directories, and an emptied one is a genuine prune candidate.
pub(crate) struct MemFs {
    files: Mutex<HashMap<String, Vec<u8>>>,
    dirs: Mutex<BTreeSet<String>>,
    fail_writes: Mutex<HashSet<String>>,
    fail_writes_oos: Mutex<HashSet<String>>,
    fail_renames_oos: Mutex<HashSet<String>>,
    corrupt_writes: Mutex<HashSet<String>>,
    fail_removes: Mutex<HashSet<String>>,
    fail_removes_oos: Mutex<HashSet<String>>,
}

impl MemFs {
    pub(crate) fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            dirs: Mutex::new(BTreeSet::new()),
            fail_writes: Mutex::new(HashSet::new()),
            fail_writes_oos: Mutex::new(HashSet::new()),
            fail_renames_oos: Mutex::new(HashSet::new()),
            corrupt_writes: Mutex::new(HashSet::new()),
            fail_removes: Mutex::new(HashSet::new()),
            fail_removes_oos: Mutex::new(HashSet::new()),
        }
    }

    /// Pre-seed a file, registering its ancestor directories.
    pub(crate) fn with_file(self, path: &str, bytes: impl Into<Vec<u8>>) -> Self {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), bytes.into());
        register_parent_dirs(&mut self.dirs.lock().unwrap(), path);
        self
    }

    /// Pre-seed an empty directory (and every ancestor), so a prune has a
    /// genuinely empty directory to consider.
    pub(crate) fn with_dir(self, path: &str) -> Self {
        register_dir_chain(&mut self.dirs.lock().unwrap(), path);
        self
    }

    /// Make `write_atomic` to `path` fail, leaving any prior file intact.
    pub(crate) fn fail_write(self, path: &str) -> Self {
        self.fail_writes.lock().unwrap().insert(path.to_owned());
        self
    }

    /// Make `write_atomic` to `path` fail with an out-of-space [`FsError`], so
    /// the executor classifies it as a disk-full run abort (not a per-clip skip).
    pub(crate) fn fail_write_out_of_space(self, path: &str) -> Self {
        self.fail_writes_oos.lock().unwrap().insert(path.to_owned());
        self
    }

    /// Make a `rename` onto `to` fail with an out-of-space [`FsError`], so the
    /// executor classifies the move as a disk-full run abort.
    pub(crate) fn fail_rename_out_of_space(self, to: &str) -> Self {
        self.fail_renames_oos.lock().unwrap().insert(to.to_owned());
        self
    }

    /// Make `write_atomic` to `path` store a wrong-sized file, so the executor's
    /// post-write size check (SYNC-14) sees a mismatch.
    pub(crate) fn corrupt_write(self, path: &str) -> Self {
        self.corrupt_writes.lock().unwrap().insert(path.to_owned());
        self
    }

    /// Make `remove` of `path` fail.
    pub(crate) fn fail_remove(self, path: &str) -> Self {
        self.fail_removes.lock().unwrap().insert(path.to_owned());
        self
    }

    /// Make `remove` of `path` fail with an out-of-space [`FsError`], so the
    /// executor classifies the unlink as a disk-full run abort (not a per-clip
    /// skip). Models ENOSPC striking a delete/supersede rather than a write.
    pub(crate) fn fail_remove_out_of_space(self, path: &str) -> Self {
        self.fail_removes_oos
            .lock()
            .unwrap()
            .insert(path.to_owned());
        self
    }

    /// Read a stored file, if present.
    pub(crate) fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(path).cloned()
    }

    /// Whether a file is present.
    pub(crate) fn exists(&self, path: &str) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }

    /// A sorted snapshot of every stored path, for whole-disk assertions.
    pub(crate) fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.files.lock().unwrap().keys().cloned().collect();
        paths.sort();
        paths
    }

    /// Whether a directory is currently modelled (present and not yet pruned).
    pub(crate) fn has_dir(&self, path: &str) -> bool {
        self.dirs.lock().unwrap().contains(path)
    }

    /// Number of stored files.
    pub(crate) fn file_count(&self) -> usize {
        self.files.lock().unwrap().len()
    }

    /// Arm a `write_atomic` failure for `path` on a live double (SYNC-13).
    ///
    /// The consuming [`fail_write`](Self::fail_write) builder seeds faults at
    /// construction; this `&self` variant lets a multi-run harness inject and
    /// clear faults between runs on one persistent disk.
    pub(crate) fn arm_fail_write(&self, path: &str) {
        self.fail_writes.lock().unwrap().insert(path.to_owned());
    }

    /// Clear a previously armed `write_atomic` failure for `path`.
    pub(crate) fn disarm_fail_write(&self, path: &str) {
        self.fail_writes.lock().unwrap().remove(path);
    }

    /// Arm a `remove` failure for `path` on a live double.
    pub(crate) fn arm_fail_remove(&self, path: &str) {
        self.fail_removes.lock().unwrap().insert(path.to_owned());
    }

    /// Clear a previously armed `remove` failure for `path`.
    pub(crate) fn disarm_fail_remove(&self, path: &str) {
        self.fail_removes.lock().unwrap().remove(path);
    }

    /// Arm a silent corrupting `write_atomic` for `path` on a live double: the
    /// next write stores a wrong-sized body (modelling a lying disk), which the
    /// post-write size check (SYNC-14) must catch. Unlike the consuming
    /// [`corrupt_write`](Self::corrupt_write) builder, this lets a harness arm
    /// the corruption only for a later in-place overwrite of an existing file.
    pub(crate) fn arm_corrupt_write(&self, path: &str) {
        self.corrupt_writes.lock().unwrap().insert(path.to_owned());
    }
}

impl Filesystem for MemFs {
    fn write_atomic(&self, path: &str, bytes: &[u8]) -> Result<(), FsError> {
        if self.fail_writes_oos.lock().unwrap().contains(path) {
            return Err(FsError::out_of_space(format!(
                "simulated out-of-space write: {path}"
            )));
        }
        if self.fail_writes.lock().unwrap().contains(path) {
            return Err(FsError::new(format!("simulated write failure: {path}")));
        }
        let stored = if self.corrupt_writes.lock().unwrap().contains(path) {
            vec![0u8; bytes.len() + 1]
        } else {
            bytes.to_vec()
        };
        self.files.lock().unwrap().insert(path.to_owned(), stored);
        register_parent_dirs(&mut self.dirs.lock().unwrap(), path);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        if self.fail_renames_oos.lock().unwrap().contains(to) {
            return Err(FsError::out_of_space(format!(
                "simulated out-of-space rename onto {to}"
            )));
        }
        let mut files = self.files.lock().unwrap();
        match files.remove(from) {
            Some(bytes) => {
                files.insert(to.to_owned(), bytes);
                register_parent_dirs(&mut self.dirs.lock().unwrap(), to);
                Ok(())
            }
            None => Err(FsError::new(format!("rename source missing: {from}"))),
        }
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        if self.fail_removes_oos.lock().unwrap().contains(path) {
            return Err(FsError::out_of_space(format!(
                "simulated out-of-space remove: {path}"
            )));
        }
        if self.fail_removes.lock().unwrap().contains(path) {
            return Err(FsError::new(format!("simulated remove failure: {path}")));
        }
        self.files.lock().unwrap().remove(path);
        Ok(())
    }

    fn prune_empty_dirs(&self, root: &str) -> Result<(), FsError> {
        // A directory is prunable when nothing lives strictly beneath it: no
        // file and no surviving child directory. Removing one may empty its
        // parent, so iterate to a fixpoint — the in-memory analogue of a
        // bottom-up rmdir walk. `root` itself is never a candidate.
        let file_paths: Vec<String> = self.files.lock().unwrap().keys().cloned().collect();
        let mut dirs = self.dirs.lock().unwrap();
        loop {
            let snapshot: Vec<String> = dirs.iter().cloned().collect();
            let victim = snapshot.iter().find(|d| {
                strictly_under(d, root)
                    && !file_paths.iter().any(|f| strictly_under(f, d))
                    && !snapshot.iter().any(|o| strictly_under(o, d))
            });
            match victim {
                Some(d) => {
                    dirs.remove(d);
                }
                None => return Ok(()),
            }
        }
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, FsError> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| FsError::new(format!("no such file: {path}")))
    }

    fn metadata(&self, path: &str) -> Option<FileStat> {
        self.files.lock().unwrap().get(path).map(|bytes| FileStat {
            exists: true,
            size: bytes.len() as u64,
        })
    }
}

/// Register every ancestor directory of a file `path` (e.g. `a/b/c.flac` yields
/// `a` and `a/b`, never the file itself).
fn register_parent_dirs(dirs: &mut BTreeSet<String>, path: &str) {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for i in 1..parts.len() {
        dirs.insert(parts[..i].join("/"));
    }
}

/// Register a directory `path` and every ancestor (e.g. `a/b/c` yields `a`,
/// `a/b`, and `a/b/c`).
fn register_dir_chain(dirs: &mut BTreeSet<String>, path: &str) {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for i in 1..=parts.len() {
        dirs.insert(parts[..i].join("/"));
    }
}

/// Whether `path` sits strictly inside directory `base`. An empty `base` is the
/// account root, so every non-empty path is under it; `base` itself never is.
fn strictly_under(path: &str, base: &str) -> bool {
    if base.is_empty() {
        !path.is_empty()
    } else {
        path.len() > base.len() && path.starts_with(base) && path.as_bytes()[base.len()] == b'/'
    }
}

/// A stub [`Ffmpeg`] that returns canned bytes (or a failure) for the FLAC and
/// animated-WebP transcode paths independently.
pub(crate) struct StubFfmpeg {
    lossless: Vec<u8>,
    webp: Vec<u8>,
    fault: Option<StubFault>,
}

/// The failure a [`StubFfmpeg`] injects, so tests can exercise both a generic
/// transcode error and a disk-full (out-of-space) one.
enum StubFault {
    Generic,
    OutOfSpace,
}

/// Canned animated-WebP bytes for the cover path, small enough to fit the FLAC
/// picture budget so the embed uses WebP unless a test overrides the size.
fn canned_webp() -> Vec<u8> {
    b"RIFF\x00\x00\x00\x00WEBP-canned-anim".to_vec()
}

impl StubFfmpeg {
    /// Returns a minimal, structurally valid FLAC for `wav_to_lossless` and small
    /// canned WebP bytes for `mp4_to_webp`, so the pure tagger can parse both.
    pub(crate) fn flac() -> Self {
        Self {
            lossless: minimal_flac(),
            webp: canned_webp(),
            fault: None,
        }
    }

    /// Alias of [`flac`](Self::flac) for tests whose focus is the WebP path.
    pub(crate) fn webp() -> Self {
        Self::flac()
    }

    /// Always fails, to exercise the transcode-failure path (FLAC or WebP).
    pub(crate) fn failing() -> Self {
        Self {
            lossless: Vec::new(),
            webp: Vec::new(),
            fault: Some(StubFault::Generic),
        }
    }

    /// Always fails out-of-space, to exercise the disk-full transcode abort.
    pub(crate) fn out_of_space() -> Self {
        Self {
            lossless: Vec::new(),
            webp: Vec::new(),
            fault: Some(StubFault::OutOfSpace),
        }
    }

    /// Override the WebP transcode output, e.g. an oversized cover to exercise
    /// the FLAC fit-guard's JPEG fallback.
    pub(crate) fn with_webp(mut self, webp: Vec<u8>) -> Self {
        self.webp = webp;
        self
    }

    fn outcome(&self, output: &[u8]) -> Result<Vec<u8>, FfmpegError> {
        match &self.fault {
            None => Ok(output.to_vec()),
            Some(StubFault::Generic) => Err(FfmpegError::new("simulated transcode failure")),
            Some(StubFault::OutOfSpace) => Err(FfmpegError::out_of_space(
                "simulated out-of-space transcode",
            )),
        }
    }
}

impl Ffmpeg for StubFfmpeg {
    fn wav_to_lossless(
        &self,
        _wav: &[u8],
        _format: AudioFormat,
    ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send {
        let out = self.outcome(&self.lossless);
        async move { out }
    }

    fn mp4_to_webp(
        &self,
        _mp4: &[u8],
        _settings: WebpEncodeSettings,
    ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send {
        let out = self.outcome(&self.webp);
        async move { out }
    }
}

/// A [`Clock`] that records requested sleeps and returns immediately.
///
/// Cloneable with shared state, so a test can keep a handle after moving one
/// into a [`SunoClient`](crate::SunoClient) and still read back its sleeps.
#[derive(Clone)]
pub(crate) struct RecordingClock {
    sleeps: Arc<Mutex<Vec<Duration>>>,
    now: i64,
}

impl RecordingClock {
    pub(crate) fn new() -> Self {
        Self {
            sleeps: Arc::new(Mutex::new(Vec::new())),
            now: 0,
        }
    }

    /// A clock fixed at `now` seconds since the Unix epoch.
    pub(crate) fn at(now: i64) -> Self {
        Self {
            sleeps: Arc::new(Mutex::new(Vec::new())),
            now,
        }
    }

    /// The durations the caller asked to sleep, in order.
    pub(crate) fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.lock().unwrap().clone()
    }
}

impl Clock for RecordingClock {
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send {
        self.sleeps.lock().unwrap().push(duration);
        async {}
    }

    fn now_unix(&self) -> i64 {
        self.now
    }
}

/// Build a minimal but structurally valid FLAC: signature, a STREAMINFO block,
/// then stand-in audio frames. Enough for the tagger to parse and round-trip
/// without invoking a real encoder.
pub(crate) fn minimal_flac() -> Vec<u8> {
    let mut streaminfo = vec![0u8; 34];
    streaminfo[0..2].copy_from_slice(&4096u16.to_be_bytes());
    streaminfo[2..4].copy_from_slice(&4096u16.to_be_bytes());
    let sample_rate: u64 = 44_100;
    let channels: u64 = 2;
    let bits_per_sample: u64 = 16;
    let total_samples: u64 = 44_100;
    let packed: u64 = (sample_rate << 44)
        | ((channels - 1) << 41)
        | ((bits_per_sample - 1) << 36)
        | total_samples;
    streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());

    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.push(0x80);
    out.extend_from_slice(&[0x00, 0x00, 0x22]);
    out.extend_from_slice(&streaminfo);
    out.extend_from_slice(b"\xFF\xF8audio-frame-payload");
    out
}

/// Build a minimal but structurally valid RIFF/WAVE container: a `fmt ` (PCM)
/// chunk and a `data` chunk. Enough for the ID3-based WAV tagger to parse and
/// round-trip without invoking a real encoder.
pub(crate) fn minimal_wav() -> Vec<u8> {
    const AUDIO_DATA: &[u8] = b"\x00\x01\x02wav-sample-payload";
    let audio_len = AUDIO_DATA.len() as u32;
    let riff_size = 4u32 + 8 + 16 + 8 + audio_len;

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&44_100u32.to_le_bytes());
    out.extend_from_slice(&88_200u32.to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&audio_len.to_le_bytes());
    out.extend_from_slice(AUDIO_DATA);
    out
}

/// A fully populated clip (title, tags, duration, art and audio URLs) — the
/// real-shape fixture the details and lyric renderers exercise. Its resolved
/// context is [`full_lineage`].
pub(crate) fn full_clip() -> Clip {
    Clip {
        id: "clip-1234abcd".to_owned(),
        title: "Electric Storm".to_owned(),
        tags: "ambient, cinematic".to_owned(),
        duration: 211.6,
        created_at: "2024-03-10T14:22:01Z".to_owned(),
        display_name: "alice".to_owned(),
        handle: "alice".to_owned(),
        prompt: "an orchestral storm".to_owned(),
        gpt_description_prompt: "a moody cinematic build".to_owned(),
        lyrics: "thunder rolls\nover the plains".to_owned(),
        model_name: "chirp-v4".to_owned(),
        major_model_version: "v4".to_owned(),
        image_large_url: "https://cdn1.suno.ai/signed?token=secret".to_owned(),
        audio_url: "https://cdn1.suno.ai/clip-1234abcd.mp3".to_owned(),
        ..Clip::default()
    }
}

/// A resolved extension context for [`full_clip`], rooted on the "Weather
/// Series" album.
pub(crate) fn full_lineage() -> LineageContext {
    LineageContext {
        root_id: "rootid567890".to_owned(),
        root_title: "Weather Series".to_owned(),
        root_date: String::new(),
        parent_id: "parentid1234".to_owned(),
        edge_type: Some(EdgeType::Extend),
        status: ResolveStatus::Resolved,
        track: 0,
        track_total: 0,
    }
}

/// One programmed outcome for a [`ChaosHttp`] route.
///
/// A [`Transport`](Outcome::Transport) models a request that never produces a
/// response (timeout, reset, DNS failure); the executor classifies it as a
/// transient transport error. A [`Reply`](Outcome::Reply) is a real HTTP
/// response, which may itself carry an error status (429, 5xx, 401) or a body
/// that disagrees with its advertised `Content-Length` (a truncated download).
#[derive(Clone)]
pub(crate) enum Outcome {
    /// The transport fails before any response is produced.
    Transport(String),
    /// A real HTTP response is returned.
    Reply(Reply),
}

impl Outcome {
    /// A `200 OK` carrying `body`.
    pub(crate) fn ok(body: impl Into<Vec<u8>>) -> Self {
        Outcome::Reply(Reply::ok(body))
    }

    /// A bodyless reply with just `status`.
    pub(crate) fn status(status: u16) -> Self {
        Outcome::Reply(Reply::status(status))
    }

    /// A transport-level failure carrying `reason`.
    pub(crate) fn transport(reason: &str) -> Self {
        Outcome::Transport(reason.to_owned())
    }

    /// A `200 OK` whose advertised length exceeds its body, i.e. a truncated
    /// download the executor's size check (SYNC-14) must reject.
    pub(crate) fn truncated(body: impl Into<Vec<u8>>, advertised: u64) -> Self {
        Outcome::Reply(Reply::ok(body).with_content_length(advertised))
    }
}

/// One [`ChaosHttp`] route: a URL substring and the program of outcomes for it.
struct ChaosRoute {
    url_contains: String,
    program: VecDeque<Outcome>,
}

/// A fault-injecting [`Http`] double for whole-pipeline chaos tests.
///
/// Like [`ScriptedHttp`] it answers from the first route whose substring is a
/// prefix-free match within the request URL, popping one outcome per call (the
/// last repeats), and logs every request. Unlike it, a route can fail at the
/// transport level, and an unmatched URL is a loud `404` rather than a silent
/// success, so a missing audio route surfaces as a real download failure while
/// an unregistered cover candidate simply yields no art. Seed a route with a
/// fault prefix then a good tail (`vec![transport, transport, ok]`) to model a
/// transient error that recovers, or a single error outcome for a permanent
/// fault. With no faults registered it behaves as a faithful, deterministic
/// origin server, so the same double powers the clean full-sync harness.
pub(crate) struct ChaosHttp {
    routes: Mutex<Vec<ChaosRoute>>,
    log: Mutex<Vec<String>>,
}

impl ChaosHttp {
    pub(crate) fn new() -> Self {
        Self {
            routes: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
        }
    }

    /// Seed the Clerk auth routes so a [`SunoClient`](crate::SunoClient) built
    /// against this double can mint JWTs. The sessions route is added first so
    /// it wins over the broader `/v1/client` match.
    pub(crate) fn with_auth(self) -> Self {
        let client_body = serde_json::json!({
            "response": {
                "last_active_session_id": "s",
                "sessions": [{"id": "s", "user": {"id": "u", "username": "h"}}]
            }
        })
        .to_string();
        self.serve("/v1/client/sessions/", br#"{"jwt": "a.b.c"}"#.to_vec())
            .serve("/v1/client", client_body.into_bytes())
    }

    /// Register a route that returns a steady `200` carrying `body`.
    pub(crate) fn serve(self, url_contains: &str, body: impl Into<Vec<u8>>) -> Self {
        self.program(url_contains, vec![Outcome::ok(body)])
    }

    /// Register a route that returns `outcomes` in order (the last repeats).
    pub(crate) fn program(self, url_contains: &str, outcomes: Vec<Outcome>) -> Self {
        self.routes.lock().unwrap().push(ChaosRoute {
            url_contains: url_contains.to_owned(),
            program: outcomes.into(),
        });
        self
    }

    /// Resolve the next outcome for `url`, advancing the matched route.
    fn next_outcome(&self, url: &str) -> Outcome {
        let mut routes = self.routes.lock().unwrap();
        match routes
            .iter_mut()
            .find(|route| url.contains(&route.url_contains))
        {
            Some(route) if route.program.len() > 1 => {
                route.program.pop_front().expect("len checked")
            }
            Some(route) => route
                .program
                .front()
                .cloned()
                .expect("route has at least one outcome"),
            None => Outcome::status(404),
        }
    }
}

impl Http for ChaosHttp {
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send {
        self.log.lock().unwrap().push(request.url.clone());
        let out = match self.next_outcome(&request.url) {
            Outcome::Transport(reason) => Err(TransportError(reason)),
            Outcome::Reply(reply) => Ok(HttpResponse {
                status: reply.status,
                headers: reply.headers,
                body: reply.body,
            }),
        };
        async move { out }
    }
}
