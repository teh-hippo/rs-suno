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
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::lock::Mutex as AsyncMutex;
use futures_util::stream::{self, StreamExt};

use crate::backoff::{backoff_delay, retry_after};
use crate::client::SunoClient;
use crate::clock::Clock;
use crate::config::{AudioFormat, StemFormat};
use crate::error::Error;
use crate::ffmpeg::{Ffmpeg, WebpEncodeSettings};
use crate::fs::Filesystem;
use crate::graph::{AlbumArt, PlaylistState};
use crate::http::{Http, HttpRequest};
use crate::lineage::LineageContext;
use crate::lyrics::AlignedLyrics;
use crate::manifest::{ArtifactState, Manifest, ManifestEntry};
use crate::model::Clip;
use crate::reconcile::{
    Action, ArtifactKind, Desired, Plan, SourceMode, set_manifest_artifact, set_manifest_stem,
};
use crate::tag::{TrackMetadata, tag_flac, tag_mp3, tag_wav};

/// The shared Suno client behind an async mutex, so concurrent audio work can
/// serialise its order-sensitive API calls (JWT refresh, adaptive limiter)
/// without a runtime-specific lock. Held only for the brief WAV-render calls;
/// the heavy CDN/transcode/tag work runs unlocked.
type ClientLock<'a, C> = AsyncMutex<&'a mut SunoClient<C>>;

/// Tunables for one [`execute`] run.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// How many times a transient failure is retried before record-and-skip.
    pub max_retries: u32,
    /// How many times to poll for a server-side WAV render before giving up.
    pub wav_poll_attempts: u32,
    /// How long to wait between WAV render polls.
    pub wav_poll_interval: Duration,
    /// How many clips' audio to fetch, transcode, and tag concurrently. Clamped
    /// to at least one, so a zero collapses to sequential rather than stalling.
    pub concurrency: u32,
    /// Settings used for animated WebP cover transcodes.
    pub cover_webp: WebpEncodeSettings,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            max_retries: 3,
            wav_poll_attempts: 24,
            wav_poll_interval: Duration::from_secs(5),
            concurrency: 4,
            cover_webp: WebpEncodeSettings::default(),
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
///
/// The audio-producing actions ([`Download`](Action::Download) and
/// [`Reformat`](Action::Reformat)) run concurrently, bounded by
/// [`ExecOptions::concurrency`]: their slow parts (WAV render, CDN download,
/// transcode, tag) overlap while the order-sensitive Suno API calls are
/// serialised behind an async mutex over the shared [`SunoClient`], keeping the
/// adaptive limiter and JWT refresh correct. The remaining actions (retag,
/// rename, delete, and artifact writes/deletes) then run serially in plan order.
///
/// The outcome is deterministic regardless of completion order: concurrent audio
/// results are committed to the manifest in plan-index order, so the same plan
/// always yields the same manifest and counts whatever the concurrency level. A
/// per-clip failure is recorded and the run continues; only an auth failure or a
/// full disk aborts, and it does so promptly by stopping further audio work.
///
/// `synced` carries this run's fetched aligned (synced) lyrics keyed by clip id;
/// it is the caller's IO result, not part of the pure plan. Audio tagging embeds
/// a clip's entry as an MP3 `SYLT` frame and as the plain `USLT`/`LYRICS` text
/// (FLAC), so a clip absent from the map (an instrumental, a WAV target, or a
/// run with the feature off) is tagged exactly as before. The synced `.lrc`
/// sidecar itself is a generated artifact whose body the caller has already
/// resolved into the plan, so it is written like any other text sidecar.
#[allow(clippy::too_many_arguments)]
pub async fn execute<H, F, G, C>(
    plan: &Plan,
    manifest: &mut Manifest,
    albums: &mut BTreeMap<String, AlbumArt>,
    playlists: &mut BTreeMap<String, PlaylistState>,
    desired: &[Desired],
    synced: &HashMap<String, AlignedLyrics>,
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
    // How many tracked artifact slots reference each path. The inline old-path
    // cleanup removes a path only once nothing else holds it: each slot that
    // moves away decrements its reference, and the removal fires only when the
    // count reaches zero and no action writes the path this run. This keeps a
    // live file a co-referencing slot still owns (a prior failed swap can leave
    // two clips sharing a path) while letting the last slot to leave reclaim it,
    // so nothing is orphaned either (#76).
    let mut tracked_paths: HashMap<String, u32> = HashMap::new();
    for (_, entry) in manifest.iter() {
        for path in entry.artifact_paths() {
            *tracked_paths.entry(path.to_owned()).or_default() += 1;
        }
    }
    for art in albums.values() {
        for state in [
            art.folder_jpg.as_ref(),
            art.folder_webp.as_ref(),
            art.folder_mp4.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            *tracked_paths.entry(state.path.clone()).or_default() += 1;
        }
    }
    for playlist in playlists.values() {
        *tracked_paths.entry(playlist.path.clone()).or_default() += 1;
    }
    // Static cover art is otherwise fetched twice per clip (#89): once to embed
    // in the audio tag and once for the per-song `.jpg` sidecar, both from the
    // same CDN URL. The audio producer caches each cover it embeds here, keyed by
    // URL, and the sidecar write drains it rather than re-fetching. Only URLs a
    // `CoverJpg` sidecar will fetch this run are cached, and the sidecar removes
    // its entry on use, so the map holds at most the covers for the clips in
    // flight (bounded by `concurrency`), never the whole library.
    let cover_wanted: HashSet<&str> = plan
        .actions
        .iter()
        .filter_map(|action| match action {
            Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                source_url,
                ..
            } if !source_url.is_empty() => Some(source_url.as_str()),
            _ => None,
        })
        .collect();
    let cover_cache: Mutex<HashMap<String, Vec<u8>>> = Mutex::new(HashMap::new());
    // The `both` video-cover retention keeps `cover.webp` (transcoded) and
    // `cover.mp4` (raw) for an album from the SAME `video_cover_url`. Cache that
    // source on its first fetch so the second folder artifact drains it rather
    // than fetching the same MP4 twice (#90 reuses the #89 fetch-once path).
    let mut folder_cover_uses: HashMap<&str, u32> = HashMap::new();
    for action in &plan.actions {
        if let Action::WriteArtifact {
            kind: ArtifactKind::FolderWebp | ArtifactKind::FolderMp4,
            source_url,
            ..
        } = action
            && !source_url.is_empty()
        {
            *folder_cover_uses.entry(source_url.as_str()).or_default() += 1;
        }
    }
    let shared_cover_urls: HashSet<&str> = folder_cover_uses
        .into_iter()
        .filter(|(_, uses)| *uses > 1)
        .map(|(url, _)| url)
        .collect();
    let ctx = Ctx {
        http,
        fs,
        ffmpeg,
        clock,
        opts,
        by_id: &by_id,
        by_path: &by_path,
        synced,
        cover_cache: &cover_cache,
        cover_wanted: &cover_wanted,
        shared_cover_urls: &shared_cover_urls,
    };

    let mut outcome = ExecOutcome::default();
    // Destinations whose write has actually committed this run, gating old-path
    // cleanup so a vacated sidecar/stem is kept only when a *successful* write
    // also targets it (#142). Serial commit order makes this a clean prefix.
    let mut committed: BTreeSet<String> = BTreeSet::new();

    // The audio-producing actions ([`Download`](Action::Download) /
    // [`Reformat`](Action::Reformat)) render concurrently, but their work is
    // deliberately split so that NO destination write, file removal, or manifest
    // update happens off the plan's order:
    //
    // - the parallel producers ([`prepare_audio`](Ctx::prepare_audio)) do only
    //   the slow, side-effect-free work (fetch the CDN/WAV bytes, transcode, and
    //   tag), returning the tagged bytes; and
    // - a single serial committer below writes those bytes to the destination,
    //   removes any superseded file, and records the manifest entry, in strict
    //   plan-index order, interleaved with the non-audio actions.
    //
    // The shared client is the only `&mut` port and its API calls must stay
    // ordered, so it rides behind an async mutex; each producer locks it only for
    // the brief WAV-render calls and runs the heavy work unlocked. Renders are
    // yielded in plan order and bounded to `concurrency` in flight (and buffered),
    // so at most about `concurrency` tagged payloads are ever held in memory -
    // never the whole library.
    let client_lock = AsyncMutex::new(client);
    let concurrency = opts.concurrency.max(1) as usize;
    let ctx_ref = &ctx;
    let client_lock_ref = &client_lock;
    let mut renders = stream::iter(
        plan.actions
            .iter()
            .filter(|action| is_audio_action(action))
            .map(|action| async move { ctx_ref.prepare_audio(client_lock_ref, action).await }),
    )
    .buffered(concurrency);

    for action in &plan.actions {
        // Audio actions pull their pre-rendered bytes (yielded in plan order) and
        // commit them here; every other action applies its own effect. Both the
        // audio commit and the non-audio apply run serially, so all destination
        // and manifest effects keep the plan's order exactly as the sequential
        // executor did.
        let result = if is_audio_action(action) {
            match renders.next().await {
                Some(Ok(rendered)) => ctx.commit_audio(manifest, rendered),
                Some(Err(fail)) => Err(fail),
                None => unreachable!("buffered yields one result per audio action"),
            }
        } else {
            ctx.apply(
                client_lock_ref,
                action,
                manifest,
                albums,
                playlists,
                &mut tracked_paths,
                &committed,
            )
            .await
        };
        match result {
            Ok(effect) => {
                outcome.record(effect);
                // Record this action's destination now that its write succeeded.
                // A later action vacating a path removes it only when no
                // *committed* write also targets it; commit is strictly serial in
                // plan order, so a planned-but-failed or not-yet-run write never
                // protects a stale file from cleanup (#142).
                if let Some(dest) = written_path(action) {
                    committed.insert(dest.to_owned());
                }
            }
            Err(fail) => {
                let abort = abort_status(fail.class);
                outcome.failures.push(Failure {
                    clip_id: fail.clip_id,
                    reason: fail.reason,
                });
                if let Some(status) = abort {
                    // A systemic abort stops the run. Dropping the render stream
                    // cancels any in-flight or completed-but-uncommitted producer;
                    // because producers touch nothing on disk, the destination and
                    // manifest are left exactly as the committed prefix wrote them,
                    // with no untracked files and no removed-but-referenced file.
                    outcome.status = status;
                    break;
                }
            }
        }
    }
    drop(renders);

    // Renames and deletes can leave an album directory empty; prune those ghost
    // directories bottom-up. This runs on both the completed and the aborted
    // paths, and is best-effort: a prune failure is only a missed tidy that the
    // next run repeats, never a reason to fail the run.
    let _ = fs.prune_empty_dirs("");
    outcome
}

/// Whether an action produces audio: it fetches, transcodes, and tags a clip's
/// file. Its slow render runs in the concurrent phase; its destination write and
/// manifest update are committed serially in plan order. Everything else touches
/// the manifest, album, or playlist stores directly and runs serially.
fn is_audio_action(action: &Action) -> bool {
    matches!(action, Action::Download { .. } | Action::Reformat { .. })
}

/// The destination path an action writes on success, or `None` for actions that
/// write no file (skips, deletes). The serial committer records this once the
/// action succeeds, so a later action vacating that same path keeps it rather
/// than removing a freshly written file (#142, #76).
fn written_path(action: &Action) -> Option<&str> {
    match action {
        Action::Download { path, .. }
        | Action::Reformat { path, .. }
        | Action::WriteArtifact { path, .. }
        | Action::WriteStem { path, .. } => Some(path),
        Action::Rename { to, .. }
        | Action::MoveArtifact { to, .. }
        | Action::MoveStem { to, .. } => Some(to),
        _ => None,
    }
}

/// A rendered-but-uncommitted audio result: the tagged bytes plus what the serial
/// committer needs to place them. Produced concurrently and side-effect-free (no
/// destination write, no removal, no manifest touch); [`commit_audio`] applies
/// all of those in plan order.
struct RenderedAudio {
    clip_id: String,
    path: String,
    format: AudioFormat,
    /// The superseded file to remove after the new one lands (a [`Reformat`]),
    /// or `None` for a plain [`Download`].
    from_path: Option<String>,
    effect: Effect,
    bytes: Vec<u8>,
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
    matches!(
        kind,
        ArtifactKind::FolderJpg | ArtifactKind::FolderWebp | ArtifactKind::FolderMp4
    )
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
    /// This run's fetched aligned (synced) lyrics, keyed by clip id. Audio
    /// tagging reads a clip's entry to embed an MP3 `SYLT` frame and the plain
    /// lyric text; a clip absent here is tagged exactly as before. Populated by
    /// the caller (the fetch is IO), so the engine stays free of direct IO.
    synced: &'a HashMap<String, AlignedLyrics>,
    /// Static cover art the audio producer already fetched to embed in the tag,
    /// keyed by CDN URL, so the matching per-song `.jpg` sidecar reuses it rather
    /// than fetching the same image again (#89). Only URLs a `CoverJpg` sidecar
    /// will fetch are inserted (see `cover_wanted`) and each is removed on use, so
    /// the map stays bounded to the clips in flight. A plain mutex guards it: the
    /// concurrent producers only ever insert, and the lock is never held across an
    /// await.
    cover_cache: &'a Mutex<HashMap<String, Vec<u8>>>,
    /// The cover URLs a `CoverJpg` sidecar will fetch this run. The producer caches
    /// a cover only when its URL is here, so a clip whose cover is embedded but
    /// never written as a sidecar leaves no bytes stranded in `cover_cache`.
    cover_wanted: &'a HashSet<&'a str>,
    /// Album video-cover source URLs fetched by more than one folder artifact
    /// this run. The `both` retention derives `cover.webp` (transcoded) and
    /// `cover.mp4` (raw) from the SAME `video_cover_url`; the first fetch caches
    /// the raw source here so the sibling drains it instead of re-fetching (#90
    /// reuses the #89 fetch-once path). `FolderWebp` sorts before `FolderMp4`, so
    /// the raw source is always cached before the raw sidecar reads it.
    shared_cover_urls: &'a HashSet<&'a str>,
}

impl<H, F, G, C> Ctx<'_, H, F, G, C>
where
    H: Http,
    F: Filesystem,
    G: Ffmpeg,
    C: Clock,
{
    /// Apply one non-audio action, returning what it did or why it failed.
    ///
    /// Audio actions ([`Download`](Action::Download) /
    /// [`Reformat`](Action::Reformat)) run in the concurrent phase through
    /// [`prepare_audio`](Self::prepare_audio) and never reach here.
    #[allow(clippy::too_many_arguments)]
    async fn apply(
        &self,
        client_lock: &ClientLock<'_, C>,
        action: &Action,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        playlists: &mut BTreeMap<String, PlaylistState>,
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        match action {
            Action::Download { .. } | Action::Reformat { .. } => {
                unreachable!("audio actions are applied in the concurrent phase")
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
                    tracked_paths,
                    committed,
                )
                .await
            }
            Action::DeleteArtifact {
                kind,
                path,
                owner_id,
            } => self.delete_artifact(manifest, albums, playlists, *kind, path, owner_id),
            Action::MoveArtifact {
                kind,
                from,
                to,
                source_url,
                hash,
                owner_id,
            } => {
                self.move_artifact(
                    manifest,
                    albums,
                    playlists,
                    *kind,
                    from,
                    to,
                    source_url,
                    hash,
                    owner_id,
                    tracked_paths,
                    committed,
                )
                .await
            }
            Action::WriteStem {
                clip_id,
                key,
                stem_id,
                path,
                source_url,
                format,
                hash,
            } => {
                self.write_stem(
                    client_lock,
                    manifest,
                    clip_id,
                    key,
                    stem_id,
                    path,
                    source_url,
                    *format,
                    hash,
                    committed,
                )
                .await
            }
            Action::DeleteStem { clip_id, key, path } => {
                self.delete_stem(manifest, clip_id, key, path)
            }
            Action::MoveStem {
                clip_id,
                key,
                stem_id,
                from,
                to,
                source_url,
                format,
                hash,
            } => {
                self.move_stem(
                    client_lock,
                    manifest,
                    clip_id,
                    key,
                    stem_id,
                    from,
                    to,
                    source_url,
                    *format,
                    hash,
                    committed,
                )
                .await
            }
        }
    }

    /// Render one audio action's tagged bytes, side-effect-free.
    ///
    /// This is the concurrent part: it fetches, transcodes, and tags the file
    /// (through shared ports, plus the client behind `client_lock`), then returns
    /// the bytes and where they must go. It deliberately writes nothing, removes
    /// nothing, and never touches `manifest`, so many run at once and an aborted
    /// run can drop them with no destination or manifest effect. The serial
    /// [`commit_audio`](Self::commit_audio) applies those effects in plan order.
    async fn prepare_audio(
        &self,
        client_lock: &ClientLock<'_, C>,
        action: &Action,
    ) -> Result<RenderedAudio, Fail> {
        match action {
            Action::Download {
                clip,
                lineage,
                path,
                format,
            } => {
                let bytes = self
                    .produce_audio(client_lock, clip, lineage, *format)
                    .await?;
                Ok(RenderedAudio {
                    clip_id: clip.id.clone(),
                    path: path.clone(),
                    format: *format,
                    from_path: None,
                    effect: Effect::Downloaded,
                    bytes,
                })
            }
            Action::Reformat {
                clip,
                path,
                from_path,
                from: _,
                to,
            } => {
                // A Reformat action carries no lineage, so recover it from the
                // desired set (the same context that drove naming and the hash),
                // falling back to a self-rooted context when the clip is not in
                // the current selection.
                let lineage = self
                    .by_id
                    .get(clip.id.as_str())
                    .map(|d| d.lineage.clone())
                    .unwrap_or_else(|| LineageContext::own_root(clip));
                let bytes = self.produce_audio(client_lock, clip, &lineage, *to).await?;
                Ok(RenderedAudio {
                    clip_id: clip.id.clone(),
                    path: path.clone(),
                    format: *to,
                    from_path: Some(from_path.clone()),
                    effect: Effect::Reformatted,
                    bytes,
                })
            }
            _ => unreachable!("prepare_audio only handles audio actions"),
        }
    }

    /// Commit one rendered audio result serially, in plan order.
    ///
    /// Writes the tagged bytes to the destination, then, for a [`Reformat`], drops
    /// the superseded file, then records the manifest entry. Ordering the write
    /// before the removal keeps a crash from losing both copies; keeping all of
    /// this off the concurrent phase preserves the sequential executor's plan-order
    /// guarantee for every destination and manifest effect.
    fn commit_audio(
        &self,
        manifest: &mut Manifest,
        rendered: RenderedAudio,
    ) -> Result<Effect, Fail> {
        let RenderedAudio {
            clip_id,
            path,
            format,
            from_path,
            effect,
            bytes,
        } = rendered;
        let size = self.write_verify(&clip_id, &path, &bytes)?;
        if let Some(from) = from_path {
            // The new file is safely in place; only now drop the old rendering.
            self.fs.remove(&from).map_err(|err| {
                permanent_fail(&clip_id, format!("could not remove old file: {err}"))
            })?;
        }
        manifest.insert(clip_id.clone(), self.entry(&clip_id, &path, format, size));
        Ok(effect)
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
            let (meta, synced) = self.track_meta(clip, lineage);
            let cover = self.fetch_cover(clip).await;
            let existing = self.fs.read(path).map_err(|err| {
                permanent_fail(&clip.id, format!("could not read for retag: {err}"))
            })?;
            let tagged = tag_wav(&existing, &meta, cover.as_deref(), synced)
                .map_err(|err| permanent_fail(&clip.id, err.to_string()))?;
            let size = self.write_verify(&clip.id, path, &tagged)?;
            self.refresh_hashes(manifest, &clip.id, Some(size));
            return Ok(Effect::Retagged);
        }

        let (meta, synced) = self.track_meta(clip, lineage);
        let cover = self.fetch_cover(clip).await;
        let existing = self
            .fs
            .read(path)
            .map_err(|err| permanent_fail(&clip.id, format!("could not read for retag: {err}")))?;
        let tagged = match format {
            AudioFormat::Mp3 => tag_mp3(&existing, &meta, cover.as_deref(), synced),
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
    /// Folder art ([`FolderJpg`](ArtifactKind::FolderJpg),
    /// [`FolderWebp`](ArtifactKind::FolderWebp), and
    /// [`FolderMp4`](ArtifactKind::FolderMp4)) is album-scoped: its `owner_id`
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
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        // A per-song sidecar needs its owning clip's manifest entry; album and
        // playlist kinds are keyed elsewhere and skip this guard.
        if is_per_clip_kind(kind) && manifest.get(owner_id).is_none() {
            // The owning audio never landed this run, so this sidecar is skipped
            // and will never drain a cover the producer cached for it. Drop that
            // entry now: an insert without a matching sidecar write must not
            // outlive its clip, keeping `cover_cache` bounded to the clips in
            // flight (#89). A non-cover kind has no entry here, so this is a
            // harmless no-op for them.
            self.cover_cache
                .lock()
                .expect("cover cache mutex poisoned")
                .remove(source_url);
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
            ArtifactKind::FolderJpg | ArtifactKind::FolderWebp | ArtifactKind::FolderMp4 => albums
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
        //
        // The removal is gated so it can never delete a live file (#76). This
        // slot is releasing `old`, so drop its reference in `tracked_paths`; the
        // file is removed only once nothing else holds it — no other tracked slot
        // still references it (count now zero) and no *committed* write this run
        // has already placed a file there (`committed`, the commit-tracked twin of
        // `suppress_path_aliasing`). On a path swap (A: x -> y while B: y -> x)
        // the earlier write commits its path, so the later mover keeps it; when
        // two slots share a path after a prior failed swap, the reference count
        // keeps it. But a merely *planned* colliding write that later fails no
        // longer protects a stale file, so it is cleaned up rather than orphaned
        // (#142).
        if let Some(old) = old_path.as_deref()
            && !old.is_empty()
            && old != path
        {
            let still_referenced = tracked_paths
                .get_mut(old)
                .map(|count| {
                    *count = count.saturating_sub(1);
                    *count > 0
                })
                .unwrap_or(false);
            if !still_referenced && !committed.contains(old) {
                self.fs.remove(old).map_err(|err| {
                    permanent_fail(
                        owner_id,
                        format!("could not remove old sidecar {old}: {err}"),
                    )
                })?;
            }
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

    /// Relocate a fetched per-clip sidecar with a local rename, falling back to a
    /// fetch-and-write when the move is unsafe or the old file has vanished.
    ///
    /// Reconcile downgrades a pure path drift (same bytes, new path, old file
    /// present, fetched kind) to a `MoveArtifact`, so a retitle renames the file
    /// rather than re-downloading a cover or re-transcoding an animated WebP
    /// (#141). The in-place rename is taken only when `from` is this slot's alone
    /// to give up (no other tracked slot references it and no committed write has
    /// placed a file there); otherwise, or if the rename fails, the ordinary
    /// [`write_artifact`](Self::write_artifact) fetches fresh bytes and runs the
    /// gated old-path cleanup, so a swap or co-reference is handled exactly as
    /// before.
    #[allow(clippy::too_many_arguments)]
    async fn move_artifact(
        &self,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        playlists: &mut BTreeMap<String, PlaylistState>,
        kind: ArtifactKind,
        from: &str,
        to: &str,
        source_url: &str,
        hash: &str,
        owner_id: &str,
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        // A per-clip sidecar needs its owning clip's audio present, exactly as
        // write_artifact requires.
        if is_per_clip_kind(kind) && manifest.get(owner_id).is_none() {
            return Ok(Effect::Skipped);
        }
        // Relocate in place only when `from` is ours alone to give up: no other
        // tracked slot still references it (a prior failed swap can share a path)
        // and no committed write this run has already placed a file there.
        // Otherwise the fetch-and-write fallback copies fresh bytes and runs the
        // gated old-path cleanup.
        let exclusive =
            tracked_paths.get(from).is_none_or(|count| *count <= 1) && !committed.contains(from);
        if from != to && exclusive {
            match self.fs.rename(from, to) {
                Ok(()) => {
                    if let Some(count) = tracked_paths.get_mut(from) {
                        *count = count.saturating_sub(1);
                    }
                    if let Some(entry) = manifest.entries.get_mut(owner_id) {
                        set_manifest_artifact(
                            entry,
                            kind,
                            Some(ArtifactState {
                                path: to.to_owned(),
                                hash: hash.to_owned(),
                            }),
                        );
                    }
                    return Ok(Effect::Renamed);
                }
                Err(err) if err.is_out_of_space() => {
                    return Err(disk_fail(
                        owner_id,
                        "disk full: no space left to move sidecar",
                    ));
                }
                // The old file has vanished, or the rename is unsupported: fall
                // through to a fetch-and-write at `to`.
                Err(_) => {}
            }
        }
        self.write_artifact(
            manifest,
            albums,
            playlists,
            kind,
            to,
            source_url,
            hash,
            owner_id,
            None,
            tracked_paths,
            committed,
        )
        .await
    }
    ///
    /// An animated cover — a per-clip [`CoverWebp`](ArtifactKind::CoverWebp) or an
    /// album [`FolderWebp`](ArtifactKind::FolderWebp) — fetches the clip's
    /// `video_cover` MP4 preview and transcodes it to an animated WebP through the
    /// ffmpeg port; every other kind is the fetched source verbatim (the static
    /// [`CoverJpg`](ArtifactKind::CoverJpg) / album [`FolderJpg`](ArtifactKind::FolderJpg)
    /// image, or the raw album [`FolderMp4`](ArtifactKind::FolderMp4) whose
    /// `video_cover_url` is kept untranscoded). A fetch or transcode failure
    /// is attributed to the owning clip and is a per-clip [`Fail`], except a
    /// disk-full transcode, which aborts the run like the audio FLAC path.
    async fn artifact_bytes(
        &self,
        kind: ArtifactKind,
        source_url: &str,
        owner_id: &str,
    ) -> Result<Vec<u8>, Fail> {
        // Reuse the cover the audio producer already fetched for the embedded tag
        // when it cached this exact URL (#89); otherwise fetch it now. The guard
        // is taken and dropped in its own statement so it never spans the await.
        let cached = self
            .cover_cache
            .lock()
            .expect("cover cache mutex poisoned")
            .remove(source_url);
        let source = match cached {
            Some(bytes) => bytes,
            None => {
                let fetched = self
                    .fetch_bytes(source_url)
                    .await
                    .map_err(|err| err.attribute(owner_id))?;
                // Cache the raw source when a sibling folder artifact will fetch
                // the same URL (the `both` retention: cover.webp + cover.mp4), so
                // it is fetched exactly once. Bounded to shared URLs and drained
                // on the sibling's use.
                if self.shared_cover_urls.contains(source_url) {
                    self.cover_cache
                        .lock()
                        .expect("cover cache mutex poisoned")
                        .insert(source_url.to_owned(), fetched.clone());
                }
                fetched
            }
        };
        match kind {
            ArtifactKind::CoverWebp | ArtifactKind::FolderWebp => self
                .ffmpeg
                .mp4_to_webp(&source, self.opts.cover_webp)
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
            | ArtifactKind::FolderMp4
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

    /// Fetch one stem's bytes, write them atomically, then record the stem on
    /// the owning clip's keyed stem map.
    ///
    /// Mirrors [`write_artifact`](Self::write_artifact) for the keyed-stem case,
    /// sharing the fetch resilience (`fetch_bytes` retries and verifies
    /// `Content-Length`) and the atomic size-verified write. A stem is only ever
    /// written for a clip whose audio is present, so an absent owning manifest
    /// entry means the audio failed or never existed this run; we skip rather
    /// than strand an untracked stem with no owning audio.
    ///
    /// Stems are stored RAW in their native container and are NEVER transcoded to
    /// FLAC, even when the song's own format is FLAC — they are the deliberate
    /// exception. A `Wav` stem is rendered through the free `convert_wav` flow
    /// (see [`fetch_stem_bytes`](Self::fetch_stem_bytes)); an `Mp3` stem is fetched
    /// straight from its public CDN url. Either way the bytes land verbatim at
    /// `path`, whose extension already matches the stem format.
    ///
    /// When a title/album change moves the song, reconcile re-emits this write at
    /// the NEW path; this handler then removes the stem left at the previously
    /// tracked path, moving it rather than orphaning it. The removal happens only
    /// after the new file is safely written and only when nothing else this run
    /// writes that path, and a remove failure returns before the slot advances so
    /// the next run re-plans the identical write and retries — self-healing.
    #[allow(clippy::too_many_arguments)]
    async fn write_stem(
        &self,
        client_lock: &ClientLock<'_, C>,
        manifest: &mut Manifest,
        clip_id: &str,
        key: &str,
        stem_id: &str,
        path: &str,
        source_url: &str,
        format: StemFormat,
        hash: &str,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        // A stem needs its owning clip's manifest entry (its audio must exist).
        if manifest.get(clip_id).is_none() {
            return Ok(Effect::Skipped);
        }
        let old_path = manifest
            .get(clip_id)
            .and_then(|e| e.stems.get(key))
            .map(|s| s.path.clone());
        let bytes = self
            .fetch_stem_bytes(client_lock, clip_id, stem_id, source_url, format)
            .await?;
        self.write_verify(clip_id, path, &bytes)?;
        // The new stem is in place; only now drop a stale copy left at the old
        // path (the song moved, or the stem format changed). `remove` is
        // idempotent. A path a *committed* write this run has already placed is
        // never removed (the commit-tracked twin of `suppress_path_aliasing`); a
        // merely planned write that later fails no longer protects a stale file
        // (#142). On a genuine remove failure we return BEFORE updating the slot,
        // so the next run re-plans the same write and retries the cleanup.
        if let Some(old) = old_path.as_deref()
            && !old.is_empty()
            && old != path
            && !committed.contains(old)
        {
            self.fs.remove(old).map_err(|err| {
                permanent_fail(clip_id, format!("could not remove old stem {old}: {err}"))
            })?;
        }
        if let Some(entry) = manifest.entries.get_mut(clip_id) {
            set_manifest_stem(
                entry,
                key,
                Some(ArtifactState {
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                }),
            );
        }
        Ok(Effect::ArtifactWritten)
    }

    /// Relocate a stem with a local rename, falling back to a fetch-and-write
    /// when the move is unsafe or the old file has vanished (#141).
    ///
    /// Reconcile downgrades a pure stem path drift to a `MoveStem`, so a retitle
    /// renames the raw stem rather than re-rendering a WAV through `convert_wav`
    /// or re-fetching an MP3. Stems carry no co-reference machinery (the
    /// [`write_stem`](Self::write_stem) cleanup only guards on the committed set),
    /// so the in-place rename is taken unless a committed write already holds
    /// `from`; otherwise the fetch-and-write fallback re-renders at `to`.
    #[allow(clippy::too_many_arguments)]
    async fn move_stem(
        &self,
        client_lock: &ClientLock<'_, C>,
        manifest: &mut Manifest,
        clip_id: &str,
        key: &str,
        stem_id: &str,
        from: &str,
        to: &str,
        source_url: &str,
        format: StemFormat,
        hash: &str,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        if manifest.get(clip_id).is_none() {
            return Ok(Effect::Skipped);
        }
        if from != to && !committed.contains(from) {
            match self.fs.rename(from, to) {
                Ok(()) => {
                    if let Some(entry) = manifest.entries.get_mut(clip_id) {
                        set_manifest_stem(
                            entry,
                            key,
                            Some(ArtifactState {
                                path: to.to_owned(),
                                hash: hash.to_owned(),
                            }),
                        );
                    }
                    return Ok(Effect::Renamed);
                }
                Err(err) if err.is_out_of_space() => {
                    return Err(disk_fail(clip_id, "disk full: no space left to move stem"));
                }
                // The old file has vanished, or the rename is unsupported: fall
                // through to a fetch-and-write at `to`.
                Err(_) => {}
            }
        }
        self.write_stem(
            client_lock,
            manifest,
            clip_id,
            key,
            stem_id,
            to,
            source_url,
            format,
            hash,
            committed,
        )
        .await
    }

    /// Resolve a stem's RAW bytes in its native container, never transcoding.
    ///
    /// A `Wav` stem renders the stem clip's lossless WAV through the very same
    /// free `convert_wav` + poll flow the main FLAC/WAV audio uses
    /// ([`resolve_wav_url`](Self::resolve_wav_url)), keyed on the stem's own
    /// `stem_id`, then downloads that WAV. An `Mp3` stem (or a degenerate `Wav`
    /// stem with no id to render) downloads its public CDN url directly. Stems
    /// are the deliberate exception to the source format: the bytes are returned
    /// exactly as delivered and are never re-encoded to FLAC.
    async fn fetch_stem_bytes(
        &self,
        client_lock: &ClientLock<'_, C>,
        clip_id: &str,
        stem_id: &str,
        source_url: &str,
        format: StemFormat,
    ) -> Result<Vec<u8>, Fail> {
        let url = match format {
            StemFormat::Wav if !stem_id.is_empty() => {
                match self.resolve_wav_url(client_lock, stem_id).await? {
                    Some(url) => url,
                    None => return Err(transient_fail(clip_id, "stem WAV render was not ready")),
                }
            }
            // Mp3, or a Wav stem with no id to render, downloads the CDN mp3.
            _ => source_url.to_owned(),
        };
        self.fetch_bytes(&url)
            .await
            .map_err(|err| err.attribute(clip_id))
    }

    /// Remove one stem file and clear its slot in the owning clip's stem map.
    ///
    /// `remove` is idempotent, so an already-absent stem is not a failure. When
    /// the owning entry is already gone (its audio was deleted earlier this run,
    /// co-deleting the stem), there is no slot to clear and that is fine; the
    /// emptied `.stems` folder is pruned by the end-of-run directory sweep.
    fn delete_stem(
        &self,
        manifest: &mut Manifest,
        clip_id: &str,
        key: &str,
        path: &str,
    ) -> Result<Effect, Fail> {
        self.fs
            .remove(path)
            .map_err(|err| permanent_fail(clip_id, format!("stem delete failed: {err}")))?;
        if let Some(entry) = manifest.entries.get_mut(clip_id) {
            set_manifest_stem(entry, key, None);
        }
        Ok(Effect::ArtifactDeleted)
    }

    /// Download (and transcode/tag) the audio for `clip` in `format`.
    async fn produce_audio(
        &self,
        client_lock: &ClientLock<'_, C>,
        clip: &Clip,
        lineage: &LineageContext,
        format: AudioFormat,
    ) -> Result<Vec<u8>, Fail> {
        let (meta, synced) = self.track_meta(clip, lineage);
        match format {
            AudioFormat::Mp3 => {
                let url = clip.mp3_url();
                let audio = self
                    .fetch_bytes(&url)
                    .await
                    .map_err(|err| err.attribute(&clip.id))?;
                let cover = self.fetch_cover(clip).await;
                tag_mp3(&audio, &meta, cover.as_deref(), synced)
                    .map_err(|err| permanent_fail(&clip.id, err.to_string()))
            }
            AudioFormat::Flac => {
                let wav = self.fetch_wav(client_lock, clip).await?;
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
            AudioFormat::Wav => {
                let wav = self.fetch_wav(client_lock, clip).await?;
                let cover = self.fetch_cover(clip).await;
                tag_wav(&wav, &meta, cover.as_deref(), synced)
                    .map_err(|err| permanent_fail(&clip.id, err.to_string()))
            }
        }
    }

    /// This run's non-empty aligned lyrics for a clip, if any were fetched.
    fn synced_for(&self, clip_id: &str) -> Option<&AlignedLyrics> {
        self.synced
            .get(clip_id)
            .filter(|aligned| !aligned.is_empty())
    }

    /// The track metadata for a clip, paired with its synced lyrics (if any).
    ///
    /// The feed omits per-clip lyrics, so when this run fetched aligned lyrics
    /// for the clip the plain text is folded into `lyrics` here, which the MP3
    /// `USLT` and FLAC `LYRICS` tags then carry. The returned [`AlignedLyrics`]
    /// is passed on to [`tag_mp3`] for the word-level `SYLT` frame.
    fn track_meta<'m>(
        &'m self,
        clip: &Clip,
        lineage: &LineageContext,
    ) -> (TrackMetadata, Option<&'m AlignedLyrics>) {
        let synced = self.synced_for(&clip.id);
        let mut meta = TrackMetadata::from_clip(clip, lineage);
        if let Some(aligned) = synced {
            meta.lyrics = aligned.plain_text();
        }
        (meta, synced)
    }

    /// Resolve the rendered WAV URL and download it.
    async fn fetch_wav(
        &self,
        client_lock: &ClientLock<'_, C>,
        clip: &Clip,
    ) -> Result<Vec<u8>, Fail> {
        let url = match self.resolve_wav_url(client_lock, &clip.id).await? {
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
    ///
    /// Each client call briefly locks `client_lock`; the poll waits happen
    /// unlocked, so concurrent clips interleave their WAV renders rather than
    /// serialising behind one clip's whole poll budget.
    async fn resolve_wav_url(
        &self,
        client_lock: &ClientLock<'_, C>,
        id: &str,
    ) -> Result<Option<String>, Fail> {
        if let Some(url) = self.wav_url_retrying(client_lock, id).await? {
            return Ok(Some(url));
        }
        self.request_wav_retrying(client_lock, id).await?;
        for _ in 0..self.opts.wav_poll_attempts {
            self.clock.sleep(self.opts.wav_poll_interval).await;
            if let Some(url) = self.wav_url_retrying(client_lock, id).await? {
                return Ok(Some(url));
            }
        }
        Ok(None)
    }

    /// Read the rendered WAV URL, retrying transient API failures with backoff
    /// (SYNC-16/17), so the default FLAC path is as resilient as the CDN path.
    async fn wav_url_retrying(
        &self,
        client_lock: &ClientLock<'_, C>,
        id: &str,
    ) -> Result<Option<String>, Fail> {
        let mut attempt: u32 = 0;
        loop {
            let result = {
                let mut client = client_lock.lock().await;
                client.wav_url(self.http, id).await
            };
            match result {
                Ok(url) => return Ok(url),
                Err(err) => match self.retry_core(id, err, &mut attempt).await {
                    Some(fail) => return Err(fail),
                    None => continue,
                },
            }
        }
    }

    /// Ask Suno to render a WAV, retrying transient API failures with backoff.
    async fn request_wav_retrying(
        &self,
        client_lock: &ClientLock<'_, C>,
        id: &str,
    ) -> Result<(), Fail> {
        let mut attempt: u32 = 0;
        loop {
            let result = {
                let mut client = client_lock.lock().await;
                client.request_wav(self.http, id).await
            };
            match result {
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
                // A `CoverJpg` sidecar will fetch this exact URL this run; keep the
                // bytes so its write reuses them instead of fetching again (#89).
                // The lock guards only the insert, never the await above.
                if self.cover_wanted.contains(url) {
                    self.cover_cache
                        .lock()
                        .expect("cover cache mutex poisoned")
                        .insert(url.to_owned(), response.body.clone());
                }
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
        Error::Api(_)
        | Error::NotFound(_)
        | Error::Tag(_)
        | Error::Config(_)
        | Error::Refused(_) => permanent_fail(id, reason),
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
        let mut client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
        let synced = HashMap::new();
        pollster::block_on(execute(
            plan,
            manifest,
            albums,
            playlists,
            desired,
            &synced,
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
            concurrency: 4,
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
        let mut client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
        let outcome = pollster::block_on(execute(
            &plan,
            &mut manifest,
            &mut albums,
            &mut playlists,
            &[d],
            &synced,
            Ports {
                client: &mut client,
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
        let mut client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
        let outcome = pollster::block_on(execute(
            &plan,
            &mut manifest,
            &mut albums,
            &mut playlists,
            &[d],
            &HashMap::new(),
            Ports {
                client: &mut client,
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
    fn write_stem_mp3_never_issues_a_generation_post() {
        // The MP3 stem path is GET-only: writing a stem fetches its CDN url and
        // never POSTs, let alone to any generation or WAV-render endpoint.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac));
        let plan = Plan {
            actions: vec![Action::WriteStem {
                clip_id: "a".to_owned(),
                key: "voc".to_owned(),
                stem_id: "voc".to_owned(),
                path: "a.stems/voc.mp3".to_owned(),
                source_url: "https://cdn1.suno.ai/voc.mp3".to_owned(),
                format: StemFormat::Mp3,
                hash: "vh".to_owned(),
            }],
        };
        let http = ScriptedHttp::new().route("voc.mp3", Reply::ok(b"stem".to_vec()));

        run(
            &plan,
            &mut manifest,
            &[],
            &http,
            &MemFs::new(),
            &StubFfmpeg::flac(),
            &RecordingClock::new(),
            &ExecOptions::default(),
        );

        assert_eq!(
            http.count("stem_task"),
            0,
            "no generation endpoint is ever hit"
        );
        assert_eq!(http.count("convert_wav"), 0);
        assert_eq!(http.count("/api/gen/"), 0);
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
        let mut auth = ClerkAuth::new("eyJtoken");
        pollster::block_on(auth.authenticate(&http)).unwrap();
        let mut client = SunoClient::new(auth, RecordingClock::new());
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
        let plan =
            crate::reconcile::reconcile(&manifest, std::slice::from_ref(&d), &local, &sources);
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

        let mut auth = ClerkAuth::new("eyJtoken");
        pollster::block_on(auth.authenticate(&http)).unwrap();
        let mut client = SunoClient::new(auth, RecordingClock::new());
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
        let plan =
            crate::reconcile::reconcile(&manifest, std::slice::from_ref(&d), &local, &sources);

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

    #[test]
    fn write_artifact_uses_configured_webp_settings() {
        use std::sync::{Arc, Mutex};

        struct RecordingWebpFfmpeg {
            seen: Arc<Mutex<Vec<WebpEncodeSettings>>>,
        }

        impl Ffmpeg for RecordingWebpFfmpeg {
            async fn wav_to_flac(
                &self,
                _wav: &[u8],
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
        let seen = Arc::new(Mutex::new(Vec::new()));
        let ffmpeg = RecordingWebpFfmpeg {
            seen: Arc::clone(&seen),
        };
        let opts = ExecOptions {
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
            &mut manifest,
            &[],
            &ScriptedHttp::new().route("a/video.mp4", Reply::ok(b"mp4-bytes".to_vec())),
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
            let mut client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
            pollster::block_on(execute(
                plan,
                manifest,
                &mut albums,
                &mut playlists,
                desired,
                &HashMap::new(),
                Ports {
                    client: &mut client,
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
            let mut client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());

            let outcome = pollster::block_on(execute(
                &plan,
                &mut manifest,
                &mut albums,
                &mut playlists,
                &desireds,
                &HashMap::new(),
                Ports {
                    client: &mut client,
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
            fn wav_to_flac(
                &self,
                wav: &[u8],
            ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send {
                let fut = self.inner.wav_to_flac(wav);
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
            let mut client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
            let plan = Plan { actions };

            let outcome = pollster::block_on(execute(
                &plan,
                &mut manifest,
                &mut albums,
                &mut playlists,
                &desireds,
                &HashMap::new(),
                Ports {
                    client: &mut client,
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
    }
}
