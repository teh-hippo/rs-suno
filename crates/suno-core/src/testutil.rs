//! Test-only in-memory doubles for the engine's ports.
//!
//! [`MockHttp`] is the original first-match HTTP double used by the client and
//! auth tests. The download executor needs more: binary bodies, response
//! headers (for `Content-Length` and `Retry-After`), scripted sequences (so a
//! request can fail then succeed), and a call log. [`ScriptedHttp`] provides
//! that, alongside an in-memory [`Filesystem`] ([`MemFs`]), a stub [`Ffmpeg`]
//! ([`StubFfmpeg`]), and a recording [`Clock`] ([`RecordingClock`]) that never
//! really sleeps, so executor tests stay deterministic.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use crate::clock::Clock;
use crate::ffmpeg::{Ffmpeg, FfmpegError};
use crate::fs::{FileStat, Filesystem, FsError};
use crate::http::{Http, HttpRequest, HttpResponse, TransportError};

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
}

impl ScriptedHttp {
    pub(crate) fn new() -> Self {
        Self {
            routes: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
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
pub(crate) struct MemFs {
    files: Mutex<HashMap<String, Vec<u8>>>,
    fail_writes: Mutex<HashSet<String>>,
    corrupt_writes: Mutex<HashSet<String>>,
    fail_removes: Mutex<HashSet<String>>,
}

impl MemFs {
    pub(crate) fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            fail_writes: Mutex::new(HashSet::new()),
            corrupt_writes: Mutex::new(HashSet::new()),
            fail_removes: Mutex::new(HashSet::new()),
        }
    }

    /// Pre-seed a file.
    pub(crate) fn with_file(self, path: &str, bytes: impl Into<Vec<u8>>) -> Self {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), bytes.into());
        self
    }

    /// Make `write_atomic` to `path` fail, leaving any prior file intact.
    pub(crate) fn fail_write(self, path: &str) -> Self {
        self.fail_writes.lock().unwrap().insert(path.to_owned());
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

    /// Read a stored file, if present.
    pub(crate) fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(path).cloned()
    }

    /// Whether a file is present.
    pub(crate) fn exists(&self, path: &str) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }
}

impl Filesystem for MemFs {
    fn write_atomic(&self, path: &str, bytes: &[u8]) -> Result<(), FsError> {
        if self.fail_writes.lock().unwrap().contains(path) {
            return Err(FsError::new(format!("simulated write failure: {path}")));
        }
        let stored = if self.corrupt_writes.lock().unwrap().contains(path) {
            vec![0u8; bytes.len() + 1]
        } else {
            bytes.to_vec()
        };
        self.files.lock().unwrap().insert(path.to_owned(), stored);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        let mut files = self.files.lock().unwrap();
        match files.remove(from) {
            Some(bytes) => {
                files.insert(to.to_owned(), bytes);
                Ok(())
            }
            None => Err(FsError::new(format!("rename source missing: {from}"))),
        }
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        if self.fail_removes.lock().unwrap().contains(path) {
            return Err(FsError::new(format!("simulated remove failure: {path}")));
        }
        self.files.lock().unwrap().remove(path);
        Ok(())
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

/// A stub [`Ffmpeg`] that returns canned FLAC bytes (or a failure).
pub(crate) struct StubFfmpeg {
    output: Vec<u8>,
    fail: bool,
}

impl StubFfmpeg {
    /// Returns a minimal, structurally valid FLAC the pure tagger can parse.
    pub(crate) fn flac() -> Self {
        Self {
            output: minimal_flac(),
            fail: false,
        }
    }

    /// Always fails, to exercise the transcode-failure path.
    pub(crate) fn failing() -> Self {
        Self {
            output: Vec::new(),
            fail: true,
        }
    }
}

impl Ffmpeg for StubFfmpeg {
    fn wav_to_flac(
        &self,
        _wav: &[u8],
    ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send {
        let out = if self.fail {
            Err(FfmpegError::new("simulated transcode failure"))
        } else {
            Ok(self.output.clone())
        };
        async move { out }
    }
}

/// A [`Clock`] that records requested sleeps and returns immediately.
pub(crate) struct RecordingClock {
    sleeps: Mutex<Vec<Duration>>,
}

impl RecordingClock {
    pub(crate) fn new() -> Self {
        Self {
            sleeps: Mutex::new(Vec::new()),
        }
    }

    /// The durations the executor asked to sleep, in order.
    pub(crate) fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.lock().unwrap().clone()
    }
}

impl Clock for RecordingClock {
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send {
        self.sleeps.lock().unwrap().push(duration);
        async {}
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
