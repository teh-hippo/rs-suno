//! The download executor: it applies a reconcile [`Plan`] to disk through ports.
//!
//! Reconcile decides *what* to do; the executor does it. It is async and pure
//! orchestration: every side effect goes through a port ([`Http`] for the
//! network, [`Filesystem`] for disk, [`Ffmpeg`] for transcoding, [`Clock`] for
//! waiting), so the whole pipeline is exercised in tests with in-memory doubles
//! and no real IO, network, or sleeping.
//!
//! Safety is the point of this module. A wrong write or delete damages the
//! user's library, so the executor:
//!
//! - writes only atomically (SYNC-13): a failed write leaves the prior file
//!   intact, because the [`Filesystem`] adapter stages a temp file and renames;
//! - verifies size (SYNC-14): a download whose body disagrees with the
//!   provider's `Content-Length` is treated as truncated and retried, and a
//!   written file whose on-disk size disagrees with the bytes written is a
//!   failure, never a recorded success;
//! - classifies errors (SYNC-17): an auth failure stops the account run with an
//!   auth status and is never retried; transient failures (timeouts, 5xx,
//!   transport, 429) are retried a bounded number of times then recorded and
//!   skipped; permanent failures are recorded and skipped; and a single clip's
//!   failure never aborts the run;
//! - backs off on rate limits (SYNC-16) through the injected [`Clock`], honouring
//!   a `Retry-After` hint.
//!
//! The executor only ever sets the manifest's [`preserve`](ManifestEntry::preserve)
//! marker on an entry it writes, and only deletes a path whose removal the
//! [`Filesystem`] confirms. Higher-level safety (empty-listing abort, the
//! destructive-sync confirmation, exit codes) is the caller's job.

use std::collections::HashMap;
use std::time::Duration;

use crate::client::SunoClient;
use crate::clock::Clock;
use crate::config::AudioFormat;
use crate::error::Error;
use crate::ffmpeg::Ffmpeg;
use crate::fs::Filesystem;
use crate::http::{Http, HttpRequest, Method};
use crate::manifest::{Manifest, ManifestEntry};
use crate::model::Clip;
use crate::reconcile::{Action, Desired, Plan, SourceMode};
use crate::tag::{TrackMetadata, tag_flac, tag_mp3};

/// First backoff step; doubles each retry, capped at [`BACKOFF_CAP`].
const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Hard ceiling on any single backoff, matching the reference integration.
const BACKOFF_CAP: Duration = Duration::from_secs(300);

/// Tunables for one [`execute`] run.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// How many times a transient failure is retried before record-and-skip.
    pub max_retries: u32,
    /// How many times to poll for a server-side WAV render before giving up.
    pub wav_poll_attempts: u32,
    /// How long to wait between WAV render polls.
    pub wav_poll_interval: Duration,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            max_retries: 3,
            wav_poll_attempts: 24,
            wav_poll_interval: Duration::from_secs(5),
        }
    }
}

/// How an [`execute`] run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunStatus {
    /// Every action was attempted; some may have failed and been skipped.
    #[default]
    Completed,
    /// An auth failure stopped the run early; remaining actions were not tried.
    AuthAborted,
}

/// One action that could not be applied, for the run summary and failure log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The clip the failed action concerned (or a path when no id applies).
    pub clip_id: String,
    /// A short, secret-free reason.
    pub reason: String,
}

/// The result of applying a [`Plan`]: per-action counts and the failure list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutcome {
    pub downloaded: usize,
    pub reformatted: usize,
    pub retagged: usize,
    pub renamed: usize,
    pub deleted: usize,
    pub skipped: usize,
    /// Actions that failed and were skipped (auth, transient-exhausted, or
    /// permanent). The run continued past each one unless it was an auth abort.
    pub failures: Vec<Failure>,
    /// How the run ended.
    pub status: RunStatus,
}

impl ExecOutcome {
    /// Number of failed actions.
    pub fn failed(&self) -> usize {
        self.failures.len()
    }

    fn record(&mut self, effect: Effect) {
        match effect {
            Effect::Downloaded => self.downloaded += 1,
            Effect::Reformatted => self.reformatted += 1,
            Effect::Retagged => self.retagged += 1,
            Effect::Renamed => self.renamed += 1,
            Effect::Deleted => self.deleted += 1,
            Effect::Skipped => self.skipped += 1,
        }
    }
}

/// The IO ports the executor drives, grouped so one value threads them through.
///
/// `client` is the only `&mut` port: it performs the authenticated WAV render
/// flow and so mutates its cached session. The rest are shared references.
pub struct Ports<'a, H, F, G, C> {
    /// Performs the authenticated WAV render and poll flow.
    pub client: &'a mut SunoClient,
    /// The public network port (CDN audio, rendered WAV, cover art).
    pub http: &'a H,
    /// The disk port.
    pub fs: &'a F,
    /// The transcode port (WAV to FLAC).
    pub ffmpeg: &'a G,
    /// The backoff and poll delay port.
    pub clock: &'a C,
}

/// Apply `plan` to disk, updating `manifest` in place, and return the outcome.
///
/// `desired` carries the per-clip metadata and art hashes plus the source modes
/// that decide the [`preserve`](ManifestEntry::preserve) marker; it is indexed
/// by clip id (and by target path, for renames) so each written entry records
/// the right hashes and protection. `ports` bundles the authenticated client
/// and the network, disk, transcode, and backoff ports. A single clip's failure
/// never aborts the run, except an auth failure, which stops it with
/// [`RunStatus::AuthAborted`].
pub async fn execute<H, F, G, C>(
    plan: &Plan,
    manifest: &mut Manifest,
    desired: &[Desired],
    ports: Ports<'_, H, F, G, C>,
    opts: &ExecOptions,
) -> ExecOutcome
where
    H: Http,
    F: Filesystem,
    G: Ffmpeg,
    C: Clock,
{
    let Ports {
        client,
        http,
        fs,
        ffmpeg,
        clock,
    } = ports;
    let by_id: HashMap<&str, &Desired> = desired.iter().map(|d| (d.clip.id.as_str(), d)).collect();
    let by_path: HashMap<&str, &Desired> = desired.iter().map(|d| (d.path.as_str(), d)).collect();
    let ctx = Ctx {
        http,
        fs,
        ffmpeg,
        clock,
        opts,
        by_id: &by_id,
        by_path: &by_path,
    };

    let mut outcome = ExecOutcome::default();
    for action in &plan.actions {
        match ctx.apply(action, client, manifest).await {
            Ok(effect) => outcome.record(effect),
            Err(fail) => {
                let aborts = matches!(fail.class, Class::Auth);
                outcome.failures.push(Failure {
                    clip_id: fail.clip_id,
                    reason: fail.reason,
                });
                if aborts {
                    outcome.status = RunStatus::AuthAborted;
                    return outcome;
                }
            }
        }
    }
    outcome
}

/// What an applied action did, for the outcome counters.
enum Effect {
    Downloaded,
    Reformatted,
    Retagged,
    Renamed,
    Deleted,
    Skipped,
}

/// How a failure should be handled (SYNC-17).
#[derive(Debug, Clone, Copy)]
enum Class {
    /// Stop the account run; do not retry.
    Auth,
    /// Retry a bounded number of times, then record and skip.
    Transient,
    /// Record and skip immediately.
    Permanent,
}

/// A classified action failure attributed to a clip.
struct Fail {
    class: Class,
    clip_id: String,
    reason: String,
}

fn auth_fail(clip_id: impl Into<String>, reason: impl Into<String>) -> Fail {
    Fail {
        class: Class::Auth,
        clip_id: clip_id.into(),
        reason: reason.into(),
    }
}

fn transient_fail(clip_id: impl Into<String>, reason: impl Into<String>) -> Fail {
    Fail {
        class: Class::Transient,
        clip_id: clip_id.into(),
        reason: reason.into(),
    }
}

fn permanent_fail(clip_id: impl Into<String>, reason: impl Into<String>) -> Fail {
    Fail {
        class: Class::Permanent,
        clip_id: clip_id.into(),
        reason: reason.into(),
    }
}

/// A classified fetch failure, not yet attributed to a clip.
struct FetchError {
    class: Class,
    reason: String,
    retry_after: Option<Duration>,
}

impl FetchError {
    fn auth(reason: impl Into<String>) -> Self {
        Self {
            class: Class::Auth,
            reason: reason.into(),
            retry_after: None,
        }
    }

    fn transient(reason: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self {
            class: Class::Transient,
            reason: reason.into(),
            retry_after,
        }
    }

    fn permanent(reason: impl Into<String>) -> Self {
        Self {
            class: Class::Permanent,
            reason: reason.into(),
            retry_after: None,
        }
    }

    fn attribute(self, clip_id: &str) -> Fail {
        Fail {
            class: self.class,
            clip_id: clip_id.to_owned(),
            reason: self.reason,
        }
    }
}

/// The shared, read-only context threaded through every action handler.
struct Ctx<'a, H, F, G, C> {
    http: &'a H,
    fs: &'a F,
    ffmpeg: &'a G,
    clock: &'a C,
    opts: &'a ExecOptions,
    by_id: &'a HashMap<&'a str, &'a Desired>,
    by_path: &'a HashMap<&'a str, &'a Desired>,
}

impl<H, F, G, C> Ctx<'_, H, F, G, C>
where
    H: Http,
    F: Filesystem,
    G: Ffmpeg,
    C: Clock,
{
    /// Apply one action, returning what it did or why it failed.
    async fn apply(
        &self,
        action: &Action,
        client: &mut SunoClient,
        manifest: &mut Manifest,
    ) -> Result<Effect, Fail> {
        match action {
            Action::Download { clip, path, format } => {
                self.download(client, manifest, clip, path, *format).await
            }
            Action::Reformat {
                clip,
                path,
                from_path,
                from: _,
                to,
            } => {
                self.reformat(client, manifest, clip, path, from_path, *to)
                    .await
            }
            Action::Retag { clip, path } => self.retag(manifest, clip, path).await,
            Action::Rename { from, to } => self.rename(manifest, from, to),
            Action::Delete { path, clip_id } => self.delete(manifest, path, clip_id),
            Action::Skip { clip_id } => {
                self.refresh_preserve(manifest, clip_id);
                Ok(Effect::Skipped)
            }
        }
    }

    /// Fetch, tag, and write a new file, then record the manifest entry.
    async fn download(
        &self,
        client: &mut SunoClient,
        manifest: &mut Manifest,
        clip: &Clip,
        path: &str,
        format: AudioFormat,
    ) -> Result<Effect, Fail> {
        let tagged = self.produce_audio(client, clip, format).await?;
        let size = self.write_verify(&clip.id, path, &tagged)?;
        manifest.insert(clip.id.clone(), self.entry(&clip.id, path, format, size));
        Ok(Effect::Downloaded)
    }

    /// Re-encode to a new format at the new path, then remove the old file.
    async fn reformat(
        &self,
        client: &mut SunoClient,
        manifest: &mut Manifest,
        clip: &Clip,
        path: &str,
        from_path: &str,
        to: AudioFormat,
    ) -> Result<Effect, Fail> {
        let tagged = self.produce_audio(client, clip, to).await?;
        let size = self.write_verify(&clip.id, path, &tagged)?;
        // The new file is safely in place; only now drop the old rendering.
        self.fs
            .remove(from_path)
            .map_err(|err| permanent_fail(&clip.id, format!("could not remove old file: {err}")))?;
        manifest.insert(clip.id.clone(), self.entry(&clip.id, path, to, size));
        Ok(Effect::Reformatted)
    }

    /// Re-tag the existing file in place to match current metadata and art.
    async fn retag(
        &self,
        manifest: &mut Manifest,
        clip: &Clip,
        path: &str,
    ) -> Result<Effect, Fail> {
        let Some(format) = manifest.get(&clip.id).map(|entry| entry.format) else {
            return Err(permanent_fail(
                &clip.id,
                "retag target missing from manifest",
            ));
        };

        if format == AudioFormat::Wav {
            // WAV carries no embedded tags; just record the new hashes so the
            // next run sees them as current and stops retagging.
            self.refresh_hashes(manifest, &clip.id, None);
            return Ok(Effect::Retagged);
        }

        let meta = TrackMetadata::from_clip(clip);
        let cover = self.fetch_cover(clip).await;
        let existing = self
            .fs
            .read(path)
            .map_err(|err| permanent_fail(&clip.id, format!("could not read for retag: {err}")))?;
        let tagged = match format {
            AudioFormat::Mp3 => tag_mp3(&existing, &meta, cover.as_deref()),
            AudioFormat::Flac => tag_flac(&existing, &meta, cover.as_deref()),
            AudioFormat::Wav => unreachable!("WAV handled above"),
        }
        .map_err(|err| permanent_fail(&clip.id, err.to_string()))?;
        let size = self.write_verify(&clip.id, path, &tagged)?;
        self.refresh_hashes(manifest, &clip.id, Some(size));
        Ok(Effect::Retagged)
    }

    /// Move the file and update the entry's path (and protection).
    fn rename(&self, manifest: &mut Manifest, from: &str, to: &str) -> Result<Effect, Fail> {
        let label = self
            .by_path
            .get(to)
            .map(|d| d.clip.id.clone())
            .unwrap_or_else(|| to.to_owned());
        self.fs
            .rename(from, to)
            .map_err(|err| permanent_fail(label, format!("rename failed: {err}")))?;

        let clip_id = self.by_path.get(to).map(|d| d.clip.id.clone()).or_else(|| {
            manifest
                .entries
                .iter()
                .find(|(_, entry)| entry.path == from)
                .map(|(id, _)| id.clone())
        });
        if let Some(id) = clip_id
            && let Some(entry) = manifest.entries.get_mut(&id)
        {
            entry.path = to.to_owned();
            if let Some(d) = self.by_path.get(to) {
                entry.preserve = preserve_for(d);
            }
        }
        Ok(Effect::Renamed)
    }

    /// Remove the file and drop the manifest entry.
    fn delete(&self, manifest: &mut Manifest, path: &str, clip_id: &str) -> Result<Effect, Fail> {
        self.fs
            .remove(path)
            .map_err(|err| permanent_fail(clip_id, format!("delete failed: {err}")))?;
        manifest.remove(clip_id);
        Ok(Effect::Deleted)
    }

    /// Download (and transcode/tag) the audio for `clip` in `format`.
    async fn produce_audio(
        &self,
        client: &mut SunoClient,
        clip: &Clip,
        format: AudioFormat,
    ) -> Result<Vec<u8>, Fail> {
        let meta = TrackMetadata::from_clip(clip);
        match format {
            AudioFormat::Mp3 => {
                let url = clip.mp3_url();
                let audio = self
                    .fetch_bytes(&url)
                    .await
                    .map_err(|err| err.attribute(&clip.id))?;
                let cover = self.fetch_cover(clip).await;
                tag_mp3(&audio, &meta, cover.as_deref())
                    .map_err(|err| permanent_fail(&clip.id, err.to_string()))
            }
            AudioFormat::Flac => {
                let wav = self.fetch_wav(client, clip).await?;
                let flac =
                    self.ffmpeg.wav_to_flac(&wav).await.map_err(|err| {
                        permanent_fail(&clip.id, format!("transcode failed: {err}"))
                    })?;
                let cover = self.fetch_cover(clip).await;
                tag_flac(&flac, &meta, cover.as_deref())
                    .map_err(|err| permanent_fail(&clip.id, err.to_string()))
            }
            AudioFormat::Wav => self.fetch_wav(client, clip).await,
        }
    }

    /// Resolve the rendered WAV URL and download it.
    async fn fetch_wav(&self, client: &mut SunoClient, clip: &Clip) -> Result<Vec<u8>, Fail> {
        let url = match self.resolve_wav_url(client, &clip.id).await? {
            Some(url) => url,
            None => return Err(transient_fail(&clip.id, "WAV render was not ready")),
        };
        self.fetch_bytes(&url)
            .await
            .map_err(|err| err.attribute(&clip.id))
    }

    /// Read the WAV URL, requesting a render and polling if it is not ready.
    ///
    /// `None` means the render did not become ready within the poll budget; the
    /// caller treats that as a non-fatal transient failure, never a silent skip.
    async fn resolve_wav_url(
        &self,
        client: &mut SunoClient,
        id: &str,
    ) -> Result<Option<String>, Fail> {
        if let Some(url) = self.wav_url_retrying(client, id).await? {
            return Ok(Some(url));
        }
        self.request_wav_retrying(client, id).await?;
        for _ in 0..self.opts.wav_poll_attempts {
            self.clock.sleep(self.opts.wav_poll_interval).await;
            if let Some(url) = self.wav_url_retrying(client, id).await? {
                return Ok(Some(url));
            }
        }
        Ok(None)
    }

    /// Read the rendered WAV URL, retrying transient API failures with backoff
    /// (SYNC-16/17), so the default FLAC path is as resilient as the CDN path.
    async fn wav_url_retrying(
        &self,
        client: &mut SunoClient,
        id: &str,
    ) -> Result<Option<String>, Fail> {
        let mut attempt: u32 = 0;
        loop {
            match client.wav_url(self.http, id).await {
                Ok(url) => return Ok(url),
                Err(err) => match self.retry_core(id, err, &mut attempt).await {
                    Some(fail) => return Err(fail),
                    None => continue,
                },
            }
        }
    }

    /// Ask Suno to render a WAV, retrying transient API failures with backoff.
    async fn request_wav_retrying(&self, client: &mut SunoClient, id: &str) -> Result<(), Fail> {
        let mut attempt: u32 = 0;
        loop {
            match client.request_wav(self.http, id).await {
                Ok(()) => return Ok(()),
                Err(err) => match self.retry_core(id, err, &mut attempt).await {
                    Some(fail) => return Err(fail),
                    None => continue,
                },
            }
        }
    }

    /// Classify a core error from the authenticated WAV flow. On a transient
    /// class within budget, back off through the [`Clock`] and return `None` to
    /// retry; otherwise return the terminal [`Fail`].
    async fn retry_core(&self, id: &str, err: Error, attempt: &mut u32) -> Option<Fail> {
        let fail = classify_core(id, err);
        if matches!(fail.class, Class::Transient) && *attempt < self.opts.max_retries {
            self.clock.sleep(backoff_delay(*attempt, None)).await;
            *attempt += 1;
            None
        } else {
            Some(fail)
        }
    }

    /// GET `url`, retrying transient failures with backoff, verifying size.
    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        let mut attempt: u32 = 0;
        loop {
            let result = self.http.send(get(url)).await;
            match classify_response(result) {
                Ok(body) => return Ok(body),
                Err(err) => {
                    if matches!(err.class, Class::Transient) && attempt < self.opts.max_retries {
                        let delay = backoff_delay(attempt, err.retry_after);
                        self.clock.sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    /// Download cover art, trying each candidate URL in order; `None` is fine.
    async fn fetch_cover(&self, clip: &Clip) -> Option<Vec<u8>> {
        for url in clip.cover_candidates() {
            if let Ok(response) = self.http.send(get(url)).await
                && (200..=299).contains(&response.status)
                && !response.body.is_empty()
            {
                return Some(response.body);
            }
        }
        None
    }

    /// Write `bytes` atomically, then confirm the on-disk size (SYNC-13/14).
    fn write_verify(&self, clip_id: &str, path: &str, bytes: &[u8]) -> Result<u64, Fail> {
        self.fs
            .write_atomic(path, bytes)
            .map_err(|err| permanent_fail(clip_id, format!("write failed: {err}")))?;
        match self.fs.metadata(path) {
            Some(stat) if stat.size == bytes.len() as u64 => Ok(stat.size),
            Some(stat) => Err(permanent_fail(
                clip_id,
                format!("wrote {} bytes, expected {}", stat.size, bytes.len()),
            )),
            None => Ok(bytes.len() as u64),
        }
    }

    /// Build the manifest entry for a freshly written file.
    fn entry(&self, clip_id: &str, path: &str, format: AudioFormat, size: u64) -> ManifestEntry {
        match self.by_id.get(clip_id) {
            Some(d) => manifest_entry(d, size),
            None => ManifestEntry {
                path: path.to_owned(),
                format,
                size,
                ..ManifestEntry::default()
            },
        }
    }

    /// Refresh an existing entry's hashes, protection, and (optionally) size.
    fn refresh_hashes(&self, manifest: &mut Manifest, clip_id: &str, size: Option<u64>) {
        let desired = self.by_id.get(clip_id).copied();
        if let Some(entry) = manifest.entries.get_mut(clip_id) {
            if let Some(d) = desired {
                entry.meta_hash = d.meta_hash.clone();
                entry.art_hash = d.art_hash.clone();
                entry.preserve = preserve_for(d);
            }
            if let Some(size) = size {
                entry.size = size;
            }
        }
    }

    /// Refresh only an entry's preserve marker from the current desired state.
    ///
    /// A clip can gain or lose copy/private protection with no file change, which
    /// reconcile emits as a [`Skip`](Action::Skip). Refreshing here keeps the
    /// persisted marker a faithful image of live protection, so the cross-run
    /// delete guard (SYNC-8) never reads it stale.
    fn refresh_preserve(&self, manifest: &mut Manifest, clip_id: &str) {
        if let Some(d) = self.by_id.get(clip_id).copied()
            && let Some(entry) = manifest.entries.get_mut(clip_id)
        {
            entry.preserve = preserve_for(d);
        }
    }
}

/// Build a manifest entry from the desired record (SYNC-8 preserve rule).
fn manifest_entry(d: &Desired, size: u64) -> ManifestEntry {
    ManifestEntry {
        path: d.path.clone(),
        format: d.format,
        meta_hash: d.meta_hash.clone(),
        art_hash: d.art_hash.clone(),
        size,
        preserve: preserve_for(d),
    }
}

/// Whether a written entry must be preserved across runs: held by any copy
/// source, or private. The reconcile delete guard reads this marker later.
fn preserve_for(d: &Desired) -> bool {
    d.private || d.modes.contains(&SourceMode::Copy)
}

/// A bare GET for a public (unauthenticated) URL.
fn get(url: &str) -> HttpRequest {
    HttpRequest {
        method: Method::Get,
        url: url.to_owned(),
        headers: Vec::new(),
    }
}

/// Classify one HTTP result into bytes or a [`FetchError`] (SYNC-14/17).
fn classify_response(
    result: Result<crate::http::HttpResponse, crate::http::TransportError>,
) -> Result<Vec<u8>, FetchError> {
    let response = match result {
        Ok(response) => response,
        Err(err) => {
            return Err(FetchError::transient(
                format!("transport error: {err}"),
                None,
            ));
        }
    };
    match response.status {
        200..=299 => {
            if let Some(expected) = content_length(&response) {
                let actual = response.body.len() as u64;
                if actual != expected {
                    return Err(FetchError::transient(
                        format!("truncated download: {actual} of {expected} bytes"),
                        None,
                    ));
                }
            }
            Ok(response.body)
        }
        401 | 403 => Err(FetchError::auth("download rejected (auth)")),
        408 => Err(FetchError::transient("request timed out", None)),
        429 => Err(FetchError::transient(
            "rate limited",
            retry_after(&response),
        )),
        500..=599 => Err(FetchError::transient(
            format!("server error {}", response.status),
            None,
        )),
        status => Err(FetchError::permanent(format!(
            "download failed: status {status}"
        ))),
    }
}

/// Map a core [`Error`] from the authenticated WAV flow to a [`Fail`].
fn classify_core(id: &str, err: Error) -> Fail {
    let reason = err.to_string();
    match err {
        Error::Auth(_) => auth_fail(id, reason),
        Error::RateLimited | Error::Connection(_) => transient_fail(id, reason),
        Error::Api(_) | Error::Tag(_) | Error::Config(_) => permanent_fail(id, reason),
    }
}

/// The provider-reported body size from `Content-Length`, if present and valid.
fn content_length(response: &crate::http::HttpResponse) -> Option<u64> {
    response.header("content-length")?.trim().parse().ok()
}

/// The `Retry-After` delay in whole seconds, if present and valid.
fn retry_after(response: &crate::http::HttpResponse) -> Option<Duration> {
    let seconds: u64 = response.header("retry-after")?.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

/// Exponential backoff with a `Retry-After` floor, capped at [`BACKOFF_CAP`].
fn backoff_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let base = BACKOFF_BASE.checked_mul(factor).unwrap_or(BACKOFF_CAP);
    let delay = retry_after.map_or(base, |hint| hint.max(base));
    delay.min(BACKOFF_CAP)
}

#[cfg(test)]
mod tests {
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

    fn ext(format: AudioFormat) -> &'static str {
        match format {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Flac => "flac",
            AudioFormat::Wav => "wav",
        }
    }

    fn desired(clip: Clip, format: AudioFormat) -> Desired {
        Desired {
            path: format!("{}.{}", clip.id, ext(format)),
            clip,
            format,
            meta_hash: "m".to_owned(),
            art_hash: "art".to_owned(),
            modes: vec![SourceMode::Mirror],
            trashed: false,
            private: false,
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
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        plan: &Plan,
        manifest: &mut Manifest,
        desired: &[Desired],
        http: &ScriptedHttp,
        fs: &MemFs,
        ffmpeg: &StubFfmpeg,
        clock: &RecordingClock,
        opts: &ExecOptions,
    ) -> ExecOutcome {
        let mut client = SunoClient::new(ClerkAuth::new("eyJtoken"));
        pollster::block_on(execute(
            plan,
            manifest,
            desired,
            Ports {
                client: &mut client,
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
    fn download_mp3_uses_cdn_fallback_when_audio_url_empty() {
        let mut c = clip("a");
        c.audio_url = String::new();
        let d = desired(c.clone(), AudioFormat::Mp3);
        let plan = Plan {
            actions: vec![Action::Download {
                clip: c.clone(),
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

    // ── Atomic write and size verification (SYNC-13/14) ─────────────

    #[test]
    fn failed_write_leaves_the_prior_file_intact() {
        let c = clip("f");
        let d = desired(c.clone(), AudioFormat::Mp3);
        let plan = Plan {
            actions: vec![Action::Download {
                clip: c.clone(),
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
        let d1 = desired(c1.clone(), AudioFormat::Mp3);
        let d2 = desired(c2.clone(), AudioFormat::Mp3);
        let plan = Plan {
            actions: vec![
                Action::Download {
                    clip: c1.clone(),
                    path: d1.path.clone(),
                    format: AudioFormat::Mp3,
                },
                Action::Download {
                    clip: c2.clone(),
                    path: d2.path.clone(),
                    format: AudioFormat::Mp3,
                },
            ],
        };
        let http = ScriptedHttp::new()
            .route("k1.mp3", Reply::status(401))
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

        assert_eq!(outcome.status, RunStatus::AuthAborted);
        assert_eq!(outcome.failed(), 1);
        assert_eq!(outcome.failures[0].clip_id, "k1");
        assert_eq!(outcome.downloaded, 0);
        assert_eq!(http.count("k2.mp3"), 0);
        assert!(!fs.exists("k2.mp3"));
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
                    path: d1.path.clone(),
                    format: AudioFormat::Mp3,
                },
                Action::Download {
                    clip: c2.clone(),
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
                    path: mirror.path.clone(),
                    format: AudioFormat::Mp3,
                },
                Action::Download {
                    clip: copy_held.clip.clone(),
                    path: copy_held.path.clone(),
                    format: AudioFormat::Mp3,
                },
                Action::Download {
                    clip: private.clip.clone(),
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
        let existing = tag_mp3(b"audio", &TrackMetadata::from_clip(&c), None).unwrap();
        let fs = MemFs::new().with_file("o.mp3", existing.clone());
        let mut manifest = Manifest::new();
        let mut start = entry("o.mp3", AudioFormat::Mp3);
        start.size = existing.len() as u64;
        manifest.insert("o", start);
        let plan = Plan {
            actions: vec![Action::Retag {
                clip: c.clone(),
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
    fn backoff_honours_retry_after_and_cap() {
        assert_eq!(backoff_delay(0, None), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, None), Duration::from_secs(4));
        assert_eq!(
            backoff_delay(0, Some(Duration::from_secs(9))),
            Duration::from_secs(9)
        );
        assert_eq!(backoff_delay(40, None), BACKOFF_CAP);
    }

    #[test]
    fn header_helpers_parse_or_ignore() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![
                ("Content-Length".to_owned(), "42".to_owned()),
                ("Retry-After".to_owned(), "5".to_owned()),
            ],
            body: Vec::new(),
        };
        assert_eq!(content_length(&resp), Some(42));
        assert_eq!(retry_after(&resp), Some(Duration::from_secs(5)));

        let bare = HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert_eq!(content_length(&bare), None);
        assert_eq!(retry_after(&bare), None);
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
}
