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

use crate::album_art::{AlbumArt, PlaylistState, set_album_artifact, set_playlist};
use crate::backoff::{backoff_delay, retry_after};
use crate::client::SunoClient;
use crate::clock::Clock;
use crate::error::Error;
use crate::ffmpeg::Ffmpeg;
use crate::fs::Filesystem;
use crate::http::{Http, HttpRequest};
use crate::lineage::LineageContext;
use crate::lyrics::AlignedLyrics;
use crate::manifest::{ArtifactState, Manifest, ManifestEntry};
use crate::model::Clip;
use crate::reconcile::{Action, Desired, Plan, set_manifest_artifact, set_manifest_stem};
use crate::tag::{Cover, TrackMetadata, flac_picture_data_budget, tag_flac, tag_mp3, tag_wav};
use crate::tag_alac::tag_alac;
use crate::vocab::{ArtifactKind, AudioFormat, SourceMode, StemFormat, WebpEncodeSettings};

/// The shared Suno client behind an async mutex, so concurrent audio work can
/// serialise its order-sensitive API calls (JWT refresh, adaptive limiter)
/// without a runtime-specific lock. Held only for the brief WAV-render calls;
/// the heavy CDN/transcode/tag work runs unlocked.
type ClientLock<'a, C> = AsyncMutex<&'a SunoClient<C>>;

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
    /// Embed a bounded animated WebP as the audio file's front cover (in place of
    /// the static JPEG) for clips that carry a video preview. Off leaves the
    /// static JPEG embed unchanged.
    pub embed_animated_cover: bool,
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
            embed_animated_cover: false,
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
/// `client` performs the authenticated WAV render flow. The rest are shared
/// references.
pub struct Ports<'a, H, F, G, C> {
    /// Performs the authenticated WAV render and poll flow.
    pub client: &'a SunoClient<C>,
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
/// Audio-producing ([`Download`](Action::Download) /
/// [`Reformat`](Action::Reformat)), fetched-artifact
/// ([`WriteArtifact`](Action::WriteArtifact) with no inline content), and stem
/// ([`WriteStem`](Action::WriteStem)) actions all run their slow,
/// side-effect-free work concurrently, bounded by
/// [`ExecOptions::concurrency`]: WAV render + CDN download + transcode + tag for
/// audio; CDN fetch + optional WebP transcode for artifacts; WAV render + CDN
/// download for stems. Order-sensitive Suno API calls (WAV render initiation and
/// poll) are serialised behind an async mutex over the shared [`SunoClient`],
/// keeping the adaptive limiter and JWT refresh correct. The remaining actions
/// (retag, rename, delete, artifact deletes, and inline artifact writes) run
/// serially in plan order.
///
/// The outcome is deterministic regardless of completion order: all prepared
/// results are committed to the manifest in plan-index order, so the same plan
/// always yields the same manifest and counts whatever the concurrency level. A
/// per-clip failure is recorded and the run continues; only an auth failure or a
/// full disk aborts, and it does so promptly by stopping further concurrent work.
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

    // Audio (Download/Reformat), fetched-artifact (WriteArtifact with no inline
    // content), and stem (WriteStem) actions all split their work to maintain the
    // CRITICAL DELETION-SAFETY INVARIANT: NO destination write, file removal, or
    // manifest/album/playlist mutation happens off plan order:
    //
    // - concurrent preparers ([`prepare`](Ctx::prepare)) do only the slow,
    //   side-effect-free work — fetch CDN/WAV bytes, transcode, tag — returning
    //   bytes and the routing metadata the committer needs; and
    // - a single serial committer below writes those bytes to the destination,
    //   removes any superseded file, and records the manifest/album/playlist
    //   entry, in strict plan-index order, interleaved with the remaining serial
    //   actions.
    //
    // The shared client is the only `&mut` port and its API calls must stay
    // ordered, so it rides behind an async mutex; each producer locks it only for
    // the brief WAV-render calls and runs the heavy work unlocked. Prepares are
    // yielded in plan order and bounded to `concurrency` in flight (and buffered),
    // so at most about `concurrency` payloads are ever held in memory — never the
    // whole library.
    let client_lock = AsyncMutex::new(client);
    let concurrency = opts.concurrency.max(1) as usize;
    let ctx_ref = &ctx;
    let client_lock_ref = &client_lock;
    // Clip IDs already in the manifest before this plan runs. Per-clip
    // artifacts and stems for these clips are prepared concurrently; for new
    // clips (not yet in the manifest) the serial apply path handles them after
    // the audio commit, so the owner-absent guard fires correctly.
    let pre_clip_ids: HashSet<String> = manifest.entries.keys().cloned().collect();
    // Clip IDs with a concurrent audio (Download/Reformat) action this run.
    // Used to keep CoverJpg serial when its audio producer will cache the same
    // cover URL (#89); preparing both concurrently races the remove vs insert.
    let audio_clip_ids: HashSet<&str> = plan
        .actions
        .iter()
        .filter_map(|action| match action {
            Action::Download { clip, .. } | Action::Reformat { clip, .. } => Some(clip.id.as_str()),
            _ => None,
        })
        .collect();
    let mut prepares = stream::iter(
        plan.actions
            .iter()
            .filter(|action| is_prepareable(action, &pre_clip_ids, &audio_clip_ids))
            .map(|action| async move { ctx_ref.prepare(client_lock_ref, action).await }),
    )
    .buffered(concurrency);

    for action in &plan.actions {
        // Prepareable actions pull their pre-fetched bytes (yielded in plan order)
        // and commit them here; every other action applies its own effect. Both the
        // serial commit and the serial apply run in the same serial loop, so all
        // destination and manifest effects keep the plan's order exactly.
        let result = if is_prepareable(action, &pre_clip_ids, &audio_clip_ids) {
            match prepares.next().await {
                Some(Ok(Prepared::Audio(rendered))) => ctx.commit_audio(manifest, rendered),
                Some(Ok(Prepared::Artifact(prepared))) => ctx.commit_artifact(
                    manifest,
                    albums,
                    playlists,
                    prepared,
                    &mut tracked_paths,
                    &committed,
                ),
                Some(Ok(Prepared::Stem(prepared))) => {
                    ctx.commit_stem(manifest, prepared, &mut tracked_paths, &committed)
                }
                Some(Err(fail)) => Err(fail),
                None => unreachable!("buffered yields one result per prepareable action"),
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
                    // A systemic abort stops the run. Dropping the prepare stream
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
    drop(prepares);

    // Renames and deletes can leave an album directory empty; prune those ghost
    // directories bottom-up. This runs on both the completed and the aborted
    // paths, and is best-effort: a prune failure is only a missed tidy that the
    // next run repeats, never a reason to fail the run.
    let _ = fs.prune_empty_dirs("");
    outcome
}

/// Whether an action has a slow, side-effect-free network or transcode phase
/// that benefits from concurrent preparation. Audio actions (Download/Reformat)
/// are always prepareable. A fetched artifact (WriteArtifact with no inline
/// content) or stem write (WriteStem) is prepareable only when its owning clip
/// was already in the manifest before this plan started: a new clip's sidecar
/// cannot be prepared concurrently because its audio has not committed yet (the
/// manifest entry doesn't exist at prepare time), so it falls through to the
/// serial apply path which checks the manifest after the audio commits.
///
/// Two additional cases stay serial to preserve fetch-once dedup:
///
/// - [`FolderWebp`](ArtifactKind::FolderWebp) / [`FolderMp4`](ArtifactKind::FolderMp4):
///   the `both` retention shares one `video_cover_url`; serial ordering lets
///   the first fetch insert into `cover_cache` and the second drain it (#90).
/// - [`CoverJpg`](ArtifactKind::CoverJpg) whose owner clip also has an audio
///   action this run: the audio producer caches the cover bytes in `cover_cache`
///   (#89); a concurrent CoverJpg drains the cache before the insert, causing a
///   double fetch and a leaked entry.
fn is_prepareable(
    action: &Action,
    pre_clip_ids: &HashSet<String>,
    audio_clip_ids: &HashSet<&str>,
) -> bool {
    match action {
        Action::Download { .. } | Action::Reformat { .. } => true,
        Action::WriteArtifact {
            kind,
            owner_id,
            content: None,
            ..
        } => {
            if matches!(kind, ArtifactKind::FolderWebp | ArtifactKind::FolderMp4) {
                return false;
            }
            if *kind == ArtifactKind::CoverJpg && audio_clip_ids.contains(owner_id.as_str()) {
                return false;
            }
            !is_per_clip_kind(*kind) || pre_clip_ids.contains(owner_id.as_str())
        }
        Action::WriteStem { clip_id, .. } => pre_clip_ids.contains(clip_id.as_str()),
        _ => false,
    }
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

/// A fetched-but-uncommitted artifact result: bytes for one
/// [`WriteArtifact`](Action::WriteArtifact) with no inline content. Produced
/// concurrently and side-effect-free; [`commit_artifact`](Ctx::commit_artifact)
/// applies all filesystem and manifest/album/playlist effects in plan order.
struct PreparedArtifact {
    kind: ArtifactKind,
    path: String,
    hash: String,
    owner_id: String,
    bytes: Vec<u8>,
}

/// A fetched-but-uncommitted stem result: bytes for one
/// [`WriteStem`](Action::WriteStem) action (including any WAV render + poll).
/// Produced concurrently and side-effect-free; [`commit_stem`](Ctx::commit_stem)
/// applies all filesystem and manifest effects in plan order.
struct PreparedStem {
    clip_id: String,
    key: String,
    path: String,
    hash: String,
    bytes: Vec<u8>,
}

/// The result of one concurrent preparation: audio, an artifact, or a stem.
enum Prepared {
    Audio(RenderedAudio),
    Artifact(PreparedArtifact),
    Stem(PreparedStem),
}

/// A cover image resolved for embedding: owned bytes plus their MIME type.
struct EmbedCover {
    bytes: Vec<u8>,
    mime: &'static str,
}

impl EmbedCover {
    /// Borrow as the [`Cover`] the taggers take.
    fn as_cover(&self) -> Cover<'_> {
        Cover {
            bytes: &self.bytes,
            mime: self.mime,
        }
    }
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
    /// Apply one serial action, returning what it did or why it failed.
    ///
    /// Audio actions ([`Download`](Action::Download) and
    /// [`Reformat`](Action::Reformat)) are always prepared concurrently and never
    /// reach here. Fetched [`WriteArtifact`](Action::WriteArtifact) and
    /// [`WriteStem`](Action::WriteStem) actions reach here only when their owning
    /// clip was NOT in the manifest at plan start (new clips); those for existing
    /// clips are prepared concurrently and commit through the stream path.
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
                unreachable!("audio actions are prepared concurrently")
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
                // Inline text sidecars carry their body in the plan.
                // Fetched artifacts for clips already in the manifest are prepared
                // concurrently and never reach here. Fetched artifacts for new clips
                // (owner not in the manifest at plan start) are handled here in the
                // serial path, with the owner-absent guard fired before any fetch.
                let bytes = match content.as_deref() {
                    Some(text) => text.as_bytes().to_vec(),
                    None => {
                        if is_per_clip_kind(*kind) && manifest.get(owner_id).is_none() {
                            // Owner never landed (audio failed or never existed).
                            // Drain any stale cache entry so it doesn't outlive
                            // this clip, then skip without fetching.
                            self.cover_cache_lock().remove(source_url);
                            return Ok(Effect::Skipped);
                        }
                        self.artifact_bytes(*kind, source_url, owner_id).await?
                    }
                };
                self.commit_artifact(
                    manifest,
                    albums,
                    playlists,
                    PreparedArtifact {
                        kind: *kind,
                        path: path.clone(),
                        hash: hash.clone(),
                        owner_id: owner_id.clone(),
                        bytes,
                    },
                    tracked_paths,
                    committed,
                )
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
                // Stems for clips already in the manifest at plan start are
                // prepared concurrently and never reach here. Stems for new
                // clips (owner not yet in the manifest) are fetched here in the
                // serial path, after the audio commit, with the same owner-absent
                // guard as the old serial write_stem.
                if manifest.get(clip_id).is_none() {
                    return Ok(Effect::Skipped);
                }
                let bytes = self
                    .fetch_stem_bytes(client_lock, clip_id, stem_id, source_url, *format)
                    .await?;
                self.commit_stem(
                    manifest,
                    PreparedStem {
                        clip_id: clip_id.clone(),
                        key: key.clone(),
                        path: path.clone(),
                        hash: hash.clone(),
                        bytes,
                    },
                    tracked_paths,
                    committed,
                )
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
                    tracked_paths,
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

    /// Lock the cover cache, panicking on poison (uniform access point, no repeated magic string).
    fn cover_cache_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.cover_cache.lock().expect("cover cache mutex poisoned")
    }

    /// Prepare one concurrent action side-effect-free, returning the bytes and
    /// routing metadata the serial committer needs. Only actions that pass
    /// [`is_prepareable`] reach here.
    async fn prepare(
        &self,
        client_lock: &ClientLock<'_, C>,
        action: &Action,
    ) -> Result<Prepared, Fail> {
        match action {
            Action::Download { .. } | Action::Reformat { .. } => self
                .prepare_audio(client_lock, action)
                .await
                .map(Prepared::Audio),
            Action::WriteArtifact {
                kind,
                path,
                source_url,
                hash,
                owner_id,
                content: None,
            } => {
                let bytes = self.artifact_bytes(*kind, source_url, owner_id).await?;
                Ok(Prepared::Artifact(PreparedArtifact {
                    kind: *kind,
                    path: path.clone(),
                    hash: hash.clone(),
                    owner_id: owner_id.clone(),
                    bytes,
                }))
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
                let bytes = self
                    .fetch_stem_bytes(client_lock, clip_id, stem_id, source_url, *format)
                    .await?;
                Ok(Prepared::Stem(PreparedStem {
                    clip_id: clip_id.clone(),
                    key: key.clone(),
                    path: path.clone(),
                    hash: hash.clone(),
                    bytes,
                }))
            }
            _ => unreachable!("prepare only handles prepareable actions"),
        }
    }

    /// Commit one prepared artifact result serially, in plan order.
    ///
    /// Writes the pre-fetched bytes, removes any stale copy left at the previously
    /// tracked path (when the audio moved), then records the slot on the manifest,
    /// album, or playlist store. All filesystem and state effects are identical to
    /// what the former serial [`write_artifact`] did; moving the slow fetch (and
    /// optional transcode) into [`prepare`] is the only change.
    ///
    /// A per-clip sidecar is skipped when its owning clip's audio is absent from
    /// the manifest: the audio failed or never existed this run, so the sidecar
    /// must not land without an owner (the preparation was speculative).
    fn commit_artifact(
        &self,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        playlists: &mut BTreeMap<String, PlaylistState>,
        prepared: PreparedArtifact,
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        let PreparedArtifact {
            kind,
            path,
            hash,
            owner_id,
            bytes,
        } = prepared;
        if is_per_clip_kind(kind) && manifest.get(&owner_id).is_none() {
            return Ok(Effect::Skipped);
        }
        let old_path = match kind {
            ArtifactKind::CoverJpg => manifest
                .get(&owner_id)
                .and_then(|e| e.cover_jpg.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::CoverWebp => manifest
                .get(&owner_id)
                .and_then(|e| e.cover_webp.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::DetailsTxt => manifest
                .get(&owner_id)
                .and_then(|e| e.details_txt.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::LyricsTxt => manifest
                .get(&owner_id)
                .and_then(|e| e.lyrics_txt.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::Lrc => manifest
                .get(&owner_id)
                .and_then(|e| e.lrc.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::VideoMp4 => manifest
                .get(&owner_id)
                .and_then(|e| e.video_mp4.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::FolderJpg | ArtifactKind::FolderWebp | ArtifactKind::FolderMp4 => albums
                .get(&owner_id)
                .and_then(|a| a.artifact(kind))
                .map(|s| s.path.clone()),
            ArtifactKind::Playlist => None,
        };
        self.write_verify(&owner_id, &path, &bytes)?;
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
                        &owner_id,
                        format!("could not remove old sidecar {old}: {err}"),
                    )
                })?;
            }
        }
        if is_album_kind(kind) {
            set_album_artifact(
                albums,
                &owner_id,
                kind,
                Some(ArtifactState {
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                }),
            );
        } else if is_playlist_kind(kind) {
            set_playlist(
                playlists,
                &owner_id,
                Some(PlaylistState {
                    name: playlist_name_from_path(&path),
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                }),
            );
        } else if let Some(entry) = manifest.entries.get_mut(&owner_id) {
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

    /// Commit one prepared stem result serially, in plan order.
    ///
    /// Writes the pre-fetched bytes (including any WAV render), removes any stale
    /// copy left at the previously tracked path, and records the stem slot.
    /// All filesystem and manifest effects are identical to what the former serial
    /// [`write_stem`] did; moving the slow fetch into [`prepare`] is the only change.
    ///
    /// Skipped when the owning clip's audio is absent from the manifest.
    fn commit_stem(
        &self,
        manifest: &mut Manifest,
        prepared: PreparedStem,
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        let PreparedStem {
            clip_id,
            key,
            path,
            hash,
            bytes,
        } = prepared;
        if manifest.get(&clip_id).is_none() {
            return Ok(Effect::Skipped);
        }
        let old_path = manifest
            .get(&clip_id)
            .and_then(|e| e.stems.get(&key))
            .map(|s| s.path.clone());
        self.write_verify(&clip_id, &path, &bytes)?;
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
                    permanent_fail(&clip_id, format!("could not remove old stem {old}: {err}"))
                })?;
            }
        }
        if let Some(entry) = manifest.entries.get_mut(&clip_id) {
            set_manifest_stem(
                entry,
                &key,
                Some(ArtifactState {
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                }),
            );
        }
        Ok(Effect::ArtifactWritten)
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
            let cover = self.resolve_cover(clip, format).await?;
            let existing = self.fs.read(path).map_err(|err| {
                permanent_fail(&clip.id, format!("could not read for retag: {err}"))
            })?;
            let tagged = tag_wav(
                &existing,
                &meta,
                cover.as_ref().map(EmbedCover::as_cover),
                synced,
            )
            .map_err(|err| permanent_fail(&clip.id, err.to_string()))?;
            let size = self.write_verify(&clip.id, path, &tagged)?;
            self.refresh_hashes(manifest, &clip.id, Some(size));
            return Ok(Effect::Retagged);
        }

        let (meta, synced) = self.track_meta(clip, lineage);
        let cover = self.resolve_cover(clip, format).await?;
        let cover = cover.as_ref().map(EmbedCover::as_cover);
        let existing = self
            .fs
            .read(path)
            .map_err(|err| permanent_fail(&clip.id, format!("could not read for retag: {err}")))?;
        let tagged = match format {
            AudioFormat::Mp3 => tag_mp3(&existing, &meta, cover, synced),
            AudioFormat::Flac => tag_flac(&existing, &meta, cover),
            AudioFormat::Alac => tag_alac(&existing, &meta, cover),
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

    /// Relocate a fetched per-clip sidecar with a local rename, falling back to a
    /// fetch-and-write when the move is unsafe or the old file has vanished.
    ///
    /// Reconcile downgrades a pure path drift (same bytes, new path, old file
    /// present, fetched kind) to a `MoveArtifact`, so a retitle renames the file
    /// rather than re-downloading a cover or re-transcoding an animated WebP
    /// (#141). The in-place rename is taken only when `from` is this slot's alone
    /// to give up (no other tracked slot references it and no committed write has
    /// placed a file there); otherwise, or if the rename fails, fresh bytes are
    /// fetched and [`commit_artifact`](Self::commit_artifact) runs the gated
    /// old-path cleanup, so a swap or co-reference is handled exactly as before.
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
        // A per-clip sidecar needs its owning clip's audio present.
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
        let bytes = self.artifact_bytes(kind, source_url, owner_id).await?;
        self.commit_artifact(
            manifest,
            albums,
            playlists,
            PreparedArtifact {
                kind,
                path: to.to_owned(),
                hash: hash.to_owned(),
                owner_id: owner_id.to_owned(),
                bytes,
            },
            tracked_paths,
            committed,
        )
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
        let cached = self.cover_cache_lock().remove(source_url);
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
                    self.cover_cache_lock()
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
            set_album_artifact(albums, owner_id, kind, None);
        } else if is_playlist_kind(kind) {
            set_playlist(playlists, owner_id, None);
        } else if let Some(entry) = manifest.entries.get_mut(owner_id) {
            set_manifest_artifact(entry, kind, None);
        }
        Ok(Effect::ArtifactDeleted)
    }

    /// Relocate a stem with a local rename, falling back to a fetch-and-write
    /// when the move is unsafe or the old file has vanished (#141).
    ///
    /// Reconcile downgrades a pure stem path drift to a `MoveStem`, so a retitle
    /// renames the raw stem rather than re-rendering a WAV through `convert_wav`
    /// or re-fetching an MP3. The in-place rename is taken only when `from` is
    /// this slot's alone to give up (no other tracked slot references it — two
    /// same-base clips can share a stem path after a partially-failed swap — and
    /// no committed write this run already holds it); otherwise the
    /// fetch-and-write fallback re-fetches the correct bytes at `to`, so a
    /// co-referenced shared stem is never renamed away with mismatched content.
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
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        if manifest.get(clip_id).is_none() {
            return Ok(Effect::Skipped);
        }
        let exclusive =
            tracked_paths.get(from).is_none_or(|count| *count <= 1) && !committed.contains(from);
        if from != to && exclusive {
            match self.fs.rename(from, to) {
                Ok(()) => {
                    if let Some(count) = tracked_paths.get_mut(from) {
                        *count = count.saturating_sub(1);
                    }
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
        let bytes = self
            .fetch_stem_bytes(client_lock, clip_id, stem_id, source_url, format)
            .await?;
        self.commit_stem(
            manifest,
            PreparedStem {
                clip_id: clip_id.to_owned(),
                key: key.to_owned(),
                path: to.to_owned(),
                hash: hash.to_owned(),
                bytes,
            },
            tracked_paths,
            committed,
        )
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
                let cover = self.resolve_cover(clip, format).await?;
                tag_mp3(
                    &audio,
                    &meta,
                    cover.as_ref().map(EmbedCover::as_cover),
                    synced,
                )
                .map_err(|err| permanent_fail(&clip.id, err.to_string()))
            }
            AudioFormat::Flac | AudioFormat::Alac => {
                let wav = self.fetch_wav(client_lock, clip).await?;
                let audio = self
                    .ffmpeg
                    .wav_to_lossless(&wav, format)
                    .await
                    .map_err(|err| {
                        if err.is_out_of_space() {
                            disk_fail(&clip.id, "disk full: no space left to transcode")
                        } else {
                            permanent_fail(&clip.id, format!("transcode failed: {err}"))
                        }
                    })?;
                let cover = self.resolve_cover(clip, format).await?;
                let cover = cover.as_ref().map(EmbedCover::as_cover);
                let tagged = match format {
                    AudioFormat::Alac => tag_alac(&audio, &meta, cover),
                    _ => tag_flac(&audio, &meta, cover),
                };
                tagged.map_err(|err| permanent_fail(&clip.id, err.to_string()))
            }
            AudioFormat::Wav => {
                let wav = self.fetch_wav(client_lock, clip).await?;
                let cover = self.resolve_cover(clip, format).await?;
                tag_wav(
                    &wav,
                    &meta,
                    cover.as_ref().map(EmbedCover::as_cover),
                    synced,
                )
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
                let client = client_lock.lock().await;
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
                let client = client_lock.lock().await;
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
                    self.cover_cache_lock()
                        .insert(url.to_owned(), response.body.clone());
                }
                return Some(response.body);
            }
        }
        None
    }

    /// Resolve the cover to embed in `clip`'s audio for `format`.
    ///
    /// When animated covers are enabled, the container can embed WebP
    /// ([`AudioFormat::embeds_animated_cover`]), and the clip has a
    /// `video_cover_url`, this fetches that MP4 preview, transcodes it to a
    /// bounded animated WebP, and — if the result fits the FLAC picture budget —
    /// embeds it as `image/webp`. It falls back to the static JPEG (exactly what
    /// a coverless clip embeds today) when the feature is off, the clip has no
    /// preview, the container is ALAC, the encode overflows the budget, or the
    /// fetch/transcode fails for any non-systemic reason. A disk-full transcode
    /// aborts the run, like the audio transcode path.
    async fn resolve_cover(
        &self,
        clip: &Clip,
        format: AudioFormat,
    ) -> Result<Option<EmbedCover>, Fail> {
        if self.opts.embed_animated_cover
            && format.embeds_animated_cover()
            && !clip.video_cover_url.is_empty()
        {
            match self.animated_cover_webp(clip).await {
                Ok(webp) if webp.len() <= flac_picture_data_budget("image/webp") => {
                    return Ok(Some(EmbedCover {
                        bytes: webp,
                        mime: "image/webp",
                    }));
                }
                // Oversized encode: keep the file valid by embedding the static
                // JPEG instead (the intent hash is unchanged, so this does not
                // churn; a settings change that makes it fit re-embeds).
                Ok(_) => {}
                // A full scratch disk is systemic: abort like the audio path.
                Err(fail) if matches!(fail.class, Class::Disk) => return Err(fail),
                // Any other fetch/transcode failure is best-effort, exactly like a
                // failed static-cover fetch: fall back to the JPEG.
                Err(_) => {}
            }
        }
        Ok(self.fetch_cover(clip).await.map(|bytes| EmbedCover {
            bytes,
            mime: "image/jpeg",
        }))
    }

    /// Fetch the clip's MP4 preview and transcode it to an animated WebP.
    ///
    /// A disk-full transcode is classified [`Class::Disk`] so [`resolve_cover`]
    /// can abort the run; every other failure is per-clip and triggers the JPEG
    /// fallback.
    async fn animated_cover_webp(&self, clip: &Clip) -> Result<Vec<u8>, Fail> {
        let mp4 = self
            .fetch_bytes(&clip.video_cover_url)
            .await
            .map_err(|err| err.attribute(&clip.id))?;
        self.ffmpeg
            .mp4_to_webp(&mp4, self.opts.cover_webp)
            .await
            .map_err(|err| {
                if err.is_out_of_space() {
                    disk_fail(&clip.id, "disk full: no space left to transcode cover")
                } else {
                    permanent_fail(&clip.id, format!("cover transcode failed: {err}"))
                }
            })
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
        | Error::BadRequest(_)
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
mod tests;
