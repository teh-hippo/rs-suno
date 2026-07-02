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
//! - classifies errors (SYNC-17): an auth failure or a full disk stops the
//!   account run (with an auth or disk-full status) and is never retried;
//!   transient failures (timeouts, 5xx,
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

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::time::Duration;

use crate::backoff::{backoff_delay, retry_after};
use crate::client::SunoClient;
use crate::clock::Clock;
use crate::config::AudioFormat;
use crate::error::Error;
use crate::ffmpeg::{Ffmpeg, WebpEncodeSettings};
use crate::fs::Filesystem;
use crate::graph::{AlbumArt, PlaylistState};
use crate::http::{Http, HttpRequest};
use crate::lineage::LineageContext;
use crate::manifest::{ArtifactState, Manifest, ManifestEntry};
use crate::model::Clip;
use crate::reconcile::{Action, ArtifactKind, Desired, Plan, SourceMode, set_manifest_artifact};
use crate::tag::{TrackMetadata, tag_flac, tag_mp3};

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
    /// The disk filled; the run stopped early rather than failing every
    /// remaining clip. Remaining actions were not tried.
    DiskFull,
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
    pub artifacts_written: usize,
    pub artifacts_deleted: usize,
    /// Actions that failed and were skipped (auth, transient-exhausted, or
    /// permanent). The run continued past each one unless it was an auth or
    /// disk-full abort.
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
            Effect::ArtifactWritten => self.artifacts_written += 1,
            Effect::ArtifactDeleted => self.artifacts_deleted += 1,
        }
    }
}

/// The IO ports the executor drives, grouped so one value threads them through.
///
/// `client` is the only `&mut` port: it performs the authenticated WAV render
/// flow and so mutates its cached session. The rest are shared references.
pub struct Ports<'a, H, F, G, C> {
    /// Performs the authenticated WAV render and poll flow.
    pub client: &'a mut SunoClient<C>,
    /// The public network port (CDN audio, rendered WAV, cover art).
    pub http: &'a H,
    /// The disk port.
    pub fs: &'a F,
    /// The transcode port (WAV to FLAC).
    pub ffmpeg: &'a G,
    /// The backoff and poll delay port.
    pub clock: &'a C,
}

/// Apply `plan` to disk, updating `manifest` and `albums` in place, and return
/// the outcome.
///
/// `desired` carries the per-clip metadata and art hashes plus the source modes
/// that decide the [`preserve`](ManifestEntry::preserve) marker; it is indexed
/// by clip id (and by target path, for renames) so each written entry records
/// the right hashes and protection. `albums` is the album-art store, keyed by
/// stable root id: folder-art writes and deletes record their state there rather
/// than on the per-clip `manifest`. `ports` bundles the authenticated client and
/// the network, disk, transcode, and backoff ports. A single clip's failure
/// never aborts the run, except an auth failure or a full disk, which stop it
/// with [`RunStatus::AuthAborted`] or [`RunStatus::DiskFull`].
pub async fn execute<H, F, G, C>(
    plan: &Plan,
    manifest: &mut Manifest,
    albums: &mut BTreeMap<String, AlbumArt>,
    playlists: &mut BTreeMap<String, PlaylistState>,
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
        match ctx.apply(action, client, manifest, albums, playlists).await {
            Ok(effect) => outcome.record(effect),
            Err(fail) => {
                let abort = abort_status(fail.class);
                outcome.failures.push(Failure {
                    clip_id: fail.clip_id,
                    reason: fail.reason,
                });
                if let Some(status) = abort {
                    outcome.status = status;
                    break;
                }
            }
        }
    }
    // Renames and deletes can leave an album directory empty; prune those ghost
    // directories bottom-up. This runs on both the completed and the aborted
    // paths, and is best-effort: a prune failure is only a missed tidy that the
    // next run repeats, never a reason to fail the run.
    let _ = fs.prune_empty_dirs("");
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
    ArtifactWritten,
    ArtifactDeleted,
}

/// How a failure should be handled (SYNC-17).
#[derive(Debug, Clone, Copy)]
enum Class {
    /// Stop the account run; do not retry.
    Auth,
    /// Stop the account run: a full disk is systemic, like auth, so aborting
    /// beats skipping every remaining clip (each of which would first burn a
    /// server-side WAV-render budget before failing the same way).
    Disk,
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

/// The run-ending status for a failure class, or `None` when the failure is
/// per-clip and the run continues.
fn abort_status(class: Class) -> Option<RunStatus> {
    match class {
        Class::Auth => Some(RunStatus::AuthAborted),
        Class::Disk => Some(RunStatus::DiskFull),
        Class::Transient | Class::Permanent => None,
    }
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

fn disk_fail(clip_id: impl Into<String>, reason: impl Into<String>) -> Fail {
    Fail {
        class: Class::Disk,
        clip_id: clip_id.into(),
        reason: reason.into(),
    }
}

/// Whether an artifact kind is album-scoped folder art (owned by a root id and
/// recorded on the album store) rather than a per-clip sidecar (recorded on the
/// manifest).
fn is_album_kind(kind: ArtifactKind) -> bool {
    matches!(kind, ArtifactKind::FolderJpg | ArtifactKind::FolderWebp)
}

/// True for the library-scoped playlist artifact, routed to the playlist store.
fn is_playlist_kind(kind: ArtifactKind) -> bool {
    matches!(kind, ArtifactKind::Playlist)
}

/// True for a per-song sidecar (`cover.jpg`/`cover.webp`), whose write requires
/// the owning clip's manifest entry. Album and playlist kinds are keyed by a
/// root/playlist id that is deliberately absent from the manifest.
fn is_per_clip_kind(kind: ArtifactKind) -> bool {
    matches!(
        kind,
        ArtifactKind::CoverJpg
            | ArtifactKind::CoverWebp
            | ArtifactKind::DetailsTxt
            | ArtifactKind::LyricsTxt
            | ArtifactKind::Lrc
            | ArtifactKind::VideoMp4
    )
}

/// Recover a playlist's display name from its `.m3u8` path's file stem.
///
/// The path is `<sanitised name>.m3u8` at the library root, so the stem is the
/// sanitised name. Reconcile only ever reads a playlist's `path` and `hash`, so
/// this recovered name is a convenience for humans and its lossiness (the
/// sanitiser is not reversible) never affects a decision.
fn playlist_name_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A classified fetch failure, not yet attributed to a clip.
struct FetchError {
    class: Class,
    reason: String,
    retry_after: Option<Duration>,
}

impl FetchError {
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
        client: &mut SunoClient<C>,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        playlists: &mut BTreeMap<String, PlaylistState>,
    ) -> Result<Effect, Fail> {
        match action {
            Action::Download {
                clip,
                lineage,
                path,
                format,
            } => {
                self.download(client, manifest, clip, lineage, path, *format)
                    .await
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
            Action::Retag {
                clip,
                lineage,
                path,
            } => self.retag(manifest, clip, lineage, path).await,
            Action::Rename { from, to } => self.rename(manifest, from, to),
            Action::Delete { path, clip_id } => self.delete(manifest, path, clip_id),
            Action::Skip { clip_id } => {
                self.refresh_preserve(manifest, clip_id);
                Ok(Effect::Skipped)
            }
            Action::WriteArtifact {
                kind,
                path,
                source_url,
                hash,
                owner_id,
                content,
            } => {
                self.write_artifact(
                    manifest,
                    albums,
                    playlists,
                    *kind,
                    path,
                    source_url,
                    hash,
                    owner_id,
                    content.as_deref(),
                )
                .await
            }
            Action::DeleteArtifact {
                kind,
                path,
                owner_id,
            } => self.delete_artifact(manifest, albums, playlists, *kind, path, owner_id),
        }
    }

    /// Fetch, tag, and write a new file, then record the manifest entry.
    async fn download(
        &self,
        client: &mut SunoClient<C>,
        manifest: &mut Manifest,
        clip: &Clip,
        lineage: &LineageContext,
        path: &str,
        format: AudioFormat,
    ) -> Result<Effect, Fail> {
        let tagged = self.produce_audio(client, clip, lineage, format).await?;
        let size = self.write_verify(&clip.id, path, &tagged)?;
        manifest.insert(clip.id.clone(), self.entry(&clip.id, path, format, size));
        Ok(Effect::Downloaded)
    }

    /// Re-encode to a new format at the new path, then remove the old file.
    async fn reformat(
        &self,
        client: &mut SunoClient<C>,
        manifest: &mut Manifest,
        clip: &Clip,
        path: &str,
        from_path: &str,
        to: AudioFormat,
    ) -> Result<Effect, Fail> {
        // A Reformat action carries no lineage, so recover it from the desired
        // set (the same context that drove naming and the hash), falling back to
        // a self-rooted context when the clip is not in the current selection.
        let lineage = self
            .by_id
            .get(clip.id.as_str())
            .map(|d| d.lineage.clone())
            .unwrap_or_else(|| LineageContext::own_root(clip));
        let tagged = self.produce_audio(client, clip, &lineage, to).await?;
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
        lineage: &LineageContext,
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

        let meta = TrackMetadata::from_clip(clip, lineage);
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
        self.fs.rename(from, to).map_err(|err| {
            if err.is_out_of_space() {
                disk_fail(label, "disk full: no space left to rename")
            } else {
                permanent_fail(label, format!("rename failed: {err}"))
            }
        })?;

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

    /// Fetch an artifact's bytes, write them atomically, then record the sidecar
    /// on the owning manifest entry.
    ///
    /// The fetch and write share the audio path's resilience: `fetch_bytes`
    /// retries transient failures and verifies `Content-Length`, and
    /// `write_verify` confirms the on-disk size. A failure is attributed to the
    /// owning clip and returned as a per-clip [`Fail`], so a bad sidecar never
    /// aborts the whole run (only an auth failure or a full disk does, matching
    /// audio).
    ///
    /// The bytes written depend on the kind: a static cover is the fetched image
    /// verbatim, while an animated cover is the clip's MP4 preview transcoded to
    /// WebP through the ffmpeg port (see [`artifact_bytes`](Self::artifact_bytes)).
    ///
    /// A sidecar is only ever written for a clip whose audio is present: a
    /// successful `Download`/`Reformat` creates the manifest entry earlier in
    /// this run, and a prior-run clip already has one. So an absent owning entry
    /// means the audio failed or never existed this run; we skip (no fetch, no
    /// write) rather than strand an untracked sidecar with no owning audio.
    ///
    /// Folder art ([`FolderJpg`](ArtifactKind::FolderJpg) /
    /// [`FolderWebp`](ArtifactKind::FolderWebp)) is album-scoped: its `owner_id`
    /// is the album's stable root id, not a manifest clip, so it skips the
    /// manifest presence guard and records its state on the album store instead.
    ///
    /// When a title or album change moves the audio, reconcile re-emits this
    /// write at the NEW path; this handler then removes the sidecar left at the
    /// artifact's previously tracked path, moving it rather than orphaning it.
    /// The removal happens only after the new file is safely written, and a
    /// remove failure returns before the state slot advances, so the next run
    /// re-plans the identical write and retries — self-healing, never an orphan.
    #[allow(clippy::too_many_arguments)]
    async fn write_artifact(
        &self,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        playlists: &mut BTreeMap<String, PlaylistState>,
        kind: ArtifactKind,
        path: &str,
        source_url: &str,
        hash: &str,
        owner_id: &str,
        content: Option<&str>,
    ) -> Result<Effect, Fail> {
        // A per-song sidecar needs its owning clip's manifest entry; album and
        // playlist kinds are keyed elsewhere and skip this guard.
        if is_per_clip_kind(kind) && manifest.get(owner_id).is_none() {
            return Ok(Effect::Skipped);
        }
        // Capture the path this artifact was last tracked at, BEFORE the slot is
        // overwritten below, so a path-changing write (a title/album rename that
        // moves the audio) can clean up the old sidecar it left behind. Cover
        // kinds live on the manifest, folder kinds on the album store; playlists
        // reconcile their own old-path delete and so opt out here.
        let old_path = match kind {
            ArtifactKind::CoverJpg => manifest
                .get(owner_id)
                .and_then(|e| e.cover_jpg.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::CoverWebp => manifest
                .get(owner_id)
                .and_then(|e| e.cover_webp.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::DetailsTxt => manifest
                .get(owner_id)
                .and_then(|e| e.details_txt.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::LyricsTxt => manifest
                .get(owner_id)
                .and_then(|e| e.lyrics_txt.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::Lrc => manifest
                .get(owner_id)
                .and_then(|e| e.lrc.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::VideoMp4 => manifest
                .get(owner_id)
                .and_then(|e| e.video_mp4.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::FolderJpg | ArtifactKind::FolderWebp => albums
                .get(owner_id)
                .and_then(|a| a.artifact(kind))
                .map(|s| s.path.clone()),
            ArtifactKind::Playlist => None,
        };
        // A generated artifact (a playlist) carries its body inline and never
        // touches the network; a fetched one pulls (and transcodes) its source.
        let bytes = match content {
            Some(text) => text.as_bytes().to_vec(),
            None => self.artifact_bytes(kind, source_url, owner_id).await?,
        };
        self.write_verify(owner_id, path, &bytes)?;
        // The new sidecar is safely in place; only now drop a stale copy left at
        // the previous path (the audio moved). `remove` is idempotent, so an
        // already-absent old file is fine. On a genuine remove failure we return
        // BEFORE updating the slot, leaving the manifest/album pointing at the
        // old path: the next run sees the same path drift, re-plans this write,
        // and retries the cleanup — convergent, no orphan persists.
        if let Some(old) = old_path.as_deref()
            && !old.is_empty()
            && old != path
        {
            self.fs.remove(old).map_err(|err| {
                permanent_fail(
                    owner_id,
                    format!("could not remove old sidecar {old}: {err}"),
                )
            })?;
        }
        if is_album_kind(kind) {
            albums.entry(owner_id.to_owned()).or_default().set(
                kind,
                Some(ArtifactState {
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                }),
            );
        } else if is_playlist_kind(kind) {
            playlists.insert(
                owner_id.to_owned(),
                PlaylistState {
                    name: playlist_name_from_path(path),
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                },
            );
        } else if let Some(entry) = manifest.entries.get_mut(owner_id) {
            set_manifest_artifact(
                entry,
                kind,
                Some(ArtifactState {
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                }),
            );
        }
        Ok(Effect::ArtifactWritten)
    }

    /// Produce a sidecar's bytes from its source, branching on kind.
    ///
    /// An animated cover — a per-clip [`CoverWebp`](ArtifactKind::CoverWebp) or an
    /// album [`FolderWebp`](ArtifactKind::FolderWebp) — fetches the clip's
    /// `video_cover` MP4 preview and transcodes it to an animated WebP through the
    /// ffmpeg port; every other kind is the fetched source verbatim (e.g. the
    /// static [`CoverJpg`](ArtifactKind::CoverJpg) or album
    /// [`FolderJpg`](ArtifactKind::FolderJpg) image). A fetch or transcode failure
    /// is attributed to the owning clip and is a per-clip [`Fail`], except a
    /// disk-full transcode, which aborts the run like the audio FLAC path.
    async fn artifact_bytes(
        &self,
        kind: ArtifactKind,
        source_url: &str,
        owner_id: &str,
    ) -> Result<Vec<u8>, Fail> {
        let source = self
            .fetch_bytes(source_url)
            .await
            .map_err(|err| err.attribute(owner_id))?;
        match kind {
            ArtifactKind::CoverWebp | ArtifactKind::FolderWebp => self
                .ffmpeg
                .mp4_to_webp(&source, WebpEncodeSettings::default())
                .await
                .map_err(|err| {
                    if err.is_out_of_space() {
                        disk_fail(owner_id, "disk full: no space left to transcode")
                    } else {
                        permanent_fail(owner_id, format!("cover transcode failed: {err}"))
                    }
                }),
            // The text sidecars are generated and always carry inline content, so
            // `write_artifact` never reaches this fetch path for them. Guard it so
            // a future miswiring fails loudly rather than fetching a URL.
            ArtifactKind::DetailsTxt | ArtifactKind::LyricsTxt | ArtifactKind::Lrc => Err(
                permanent_fail(owner_id, "text sidecar requires inline content"),
            ),
            ArtifactKind::CoverJpg
            | ArtifactKind::FolderJpg
            | ArtifactKind::Playlist
            | ArtifactKind::VideoMp4 => Ok(source),
        }
    }

    /// Remove a sidecar file and clear its slot on the owning manifest entry.
    ///
    /// `remove` is idempotent, so an already-absent sidecar is not a failure.
    /// When the owning entry is already gone (its audio was deleted earlier this
    /// run, co-deleting the sidecar), there is no slot to clear and that is fine.
    ///
    /// Folder art is album-scoped: its slot is cleared on the album store keyed by
    /// the album's root id, not on a manifest clip.
    ///
    /// The audio `Delete` is applied before its sidecar `DeleteArtifact`. If the
    /// sidecar removal fails after the audio is already gone, the sidecar lingers
    /// untracked, but the design stays convergent rather than transactional: the
    /// next run re-plans the same removal and retries, and any directory it would
    /// have emptied is pruned once the file finally clears.
    fn delete_artifact(
        &self,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        playlists: &mut BTreeMap<String, PlaylistState>,
        kind: ArtifactKind,
        path: &str,
        owner_id: &str,
    ) -> Result<Effect, Fail> {
        self.fs
            .remove(path)
            .map_err(|err| permanent_fail(owner_id, format!("artifact delete failed: {err}")))?;
        if is_album_kind(kind) {
            if let Some(art) = albums.get_mut(owner_id) {
                art.set(kind, None);
                if art.is_empty() {
                    albums.remove(owner_id);
                }
            }
        } else if is_playlist_kind(kind) {
            playlists.remove(owner_id);
        } else if let Some(entry) = manifest.entries.get_mut(owner_id) {
            set_manifest_artifact(entry, kind, None);
        }
        Ok(Effect::ArtifactDeleted)
    }

    /// Download (and transcode/tag) the audio for `clip` in `format`.
    async fn produce_audio(
        &self,
        client: &mut SunoClient<C>,
        clip: &Clip,
        lineage: &LineageContext,
        format: AudioFormat,
    ) -> Result<Vec<u8>, Fail> {
        let meta = TrackMetadata::from_clip(clip, lineage);
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
                let flac = self.ffmpeg.wav_to_flac(&wav).await.map_err(|err| {
                    if err.is_out_of_space() {
                        disk_fail(&clip.id, "disk full: no space left to transcode")
                    } else {
                        permanent_fail(&clip.id, format!("transcode failed: {err}"))
                    }
                })?;
                let cover = self.fetch_cover(clip).await;
                tag_flac(&flac, &meta, cover.as_deref())
                    .map_err(|err| permanent_fail(&clip.id, err.to_string()))
            }
            AudioFormat::Wav => self.fetch_wav(client, clip).await,
        }
    }

    /// Resolve the rendered WAV URL and download it.
    async fn fetch_wav(&self, client: &mut SunoClient<C>, clip: &Clip) -> Result<Vec<u8>, Fail> {
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
        client: &mut SunoClient<C>,
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
        client: &mut SunoClient<C>,
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
    async fn request_wav_retrying(&self, client: &mut SunoClient<C>, id: &str) -> Result<(), Fail> {
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
            let result = self.http.send(HttpRequest::get(url)).await;
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
            if let Ok(response) = self.http.send(HttpRequest::get(url)).await
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
        self.fs.write_atomic(path, bytes).map_err(|err| {
            if err.is_out_of_space() {
                disk_fail(clip_id, format!("disk full: no space left to write {path}"))
            } else {
                permanent_fail(clip_id, format!("write failed: {err}"))
            }
        })?;
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
        ..Default::default()
    }
}

/// Whether a written entry must be preserved across runs: held by any copy
/// source, or private. The reconcile delete guard reads this marker later.
fn preserve_for(d: &Desired) -> bool {
    d.private || d.modes.contains(&SourceMode::Copy)
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
        401 | 403 => Err(FetchError::transient(
            format!("download rejected: status {}", response.status),
            None,
        )),
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
        Error::RateLimited { .. } | Error::Connection(_) => transient_fail(id, reason),
        Error::Api(_) | Error::NotFound(_) | Error::Tag(_) | Error::Config(_) => {
            permanent_fail(id, reason)
        }
    }
}

/// The provider-reported body size from `Content-Length`, if present and valid.
fn content_length(response: &crate::http::HttpResponse) -> Option<u64> {
    response.header("content-length")?.trim().parse().ok()
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
            lineage: LineageContext::own_root(&clip),
            clip,
            format,
            meta_hash: "m".to_owned(),
            art_hash: "art".to_owned(),
            modes: vec![SourceMode::Mirror],
            trashed: false,
            private: false,
            artifacts: Vec::new(),
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
    fn run_with_albums(
        plan: &Plan,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        desired: &[Desired],
        http: &ScriptedHttp,
        fs: &MemFs,
        ffmpeg: &StubFfmpeg,
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
    fn run_full(
        plan: &Plan,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        playlists: &mut BTreeMap<String, PlaylistState>,
        desired: &[Desired],
        http: &ScriptedHttp,
        fs: &MemFs,
        ffmpeg: &StubFfmpeg,
        clock: &RecordingClock,
        opts: &ExecOptions,
    ) -> ExecOutcome {
        let mut client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
        pollster::block_on(execute(
            plan,
            manifest,
            albums,
            playlists,
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
    fn write_artifact_transcodes_animated_cover_to_webp() {
        // A CoverWebp fetches the clip's MP4 preview, runs it through the ffmpeg
        // port, and writes the transcoded WebP (not the fetched MP4), recording
        // the sidecar on the owning entry.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.mp3", AudioFormat::Mp3));
        let plan = Plan {
            actions: vec![Action::WriteArtifact {
                kind: ArtifactKind::CoverWebp,
                path: "a/cover.webp".to_owned(),
                source_url: "https://cdn.suno.ai/a/video.mp4".to_owned(),
                hash: "v1".to_owned(),
                owner_id: "a".to_owned(),
                content: None,
            }],
        };
        let http = ScriptedHttp::new().route("a/video.mp4", Reply::ok(b"mp4-bytes".to_vec()));
        let fs = MemFs::new();
        let ffmpeg = StubFfmpeg::webp();

        let outcome = run(
            &plan,
            &mut manifest,
            &[],
            &http,
            &fs,
            &ffmpeg,
            &RecordingClock::new(),
            &ExecOptions::default(),
        );

        assert_eq!(outcome.artifacts_written, 1);
        assert_eq!(outcome.failed(), 0);
        assert_eq!(outcome.status, RunStatus::Completed);
        // The fetched MP4 was transcoded: the file holds the ffmpeg WebP output.
        assert_eq!(http.count("a/video.mp4"), 1);
        let written = fs.read_file("a/cover.webp").unwrap();
        assert_ne!(written, b"mp4-bytes");
        assert!(written.starts_with(b"RIFF"));
        assert_eq!(
            manifest.get("a").unwrap().cover_webp,
            Some(ArtifactState {
                path: "a/cover.webp".to_owned(),
                hash: "v1".to_owned(),
            })
        );
    }

    #[test]
    fn write_artifact_webp_transcode_failure_is_per_clip() {
        // A transcode failure is attributed to the owning clip: it is a per-clip
        // failure, the run completes, no sidecar is written, and the slot stays
        // empty. A healthy static cover in the same run still succeeds.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.mp3", AudioFormat::Mp3));
        manifest.insert("b", entry("b.mp3", AudioFormat::Mp3));
        let plan = Plan {
            actions: vec![
                Action::WriteArtifact {
                    kind: ArtifactKind::CoverWebp,
                    path: "a/cover.webp".to_owned(),
                    source_url: "https://cdn.suno.ai/a/video.mp4".to_owned(),
                    hash: "v1".to_owned(),
                    owner_id: "a".to_owned(),
                    content: None,
                },
                Action::WriteArtifact {
                    kind: ArtifactKind::CoverJpg,
                    path: "b/cover.jpg".to_owned(),
                    source_url: "https://art.suno.ai/b/large.jpg".to_owned(),
                    hash: "h1".to_owned(),
                    owner_id: "b".to_owned(),
                    content: None,
                },
            ],
        };
        let http = ScriptedHttp::new()
            .route("a/video.mp4", Reply::ok(b"mp4-bytes".to_vec()))
            .route("b/large.jpg", Reply::ok(b"jpg-b".to_vec()));
        let fs = MemFs::new();

        let outcome = run(
            &plan,
            &mut manifest,
            &[],
            &http,
            &fs,
            &StubFfmpeg::failing(),
            &RecordingClock::new(),
            &ExecOptions::default(),
        );

        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(outcome.failed(), 1);
        assert_eq!(outcome.failures[0].clip_id, "a");
        // The animated cover failed to transcode: nothing written, slot empty.
        assert!(!fs.exists("a/cover.webp"));
        assert_eq!(manifest.get("a").unwrap().cover_webp, None);
        // The static cover in the same run still succeeded.
        assert_eq!(outcome.artifacts_written, 1);
        assert_eq!(fs.read_file("b/cover.jpg").unwrap(), b"jpg-b");
        assert!(manifest.get("b").unwrap().cover_jpg.is_some());
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
}
