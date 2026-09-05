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
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures_util::lock::Mutex as AsyncMutex;
use futures_util::stream::{self, StreamExt};

use crate::album_art::{
    AlbumArt, PlaylistState, clear_playlist_artifact, set_album_artifact, set_playlist_artifact,
};
use crate::backoff::{backoff_delay, retry_after};
use crate::client::SunoClient;
use crate::clock::Clock;
use crate::error::Error;
use crate::ffmpeg::Ffmpeg;
use crate::fs::Filesystem;
use crate::hash::bytes_hash;
use crate::hash::embedded_art_hash;
use crate::http::{Http, HttpRequest};
use crate::lineage::LineageContext;
use crate::lyrics::AlignedLyrics;
use crate::manifest::{ArtifactState, Manifest, ManifestEntry};
use crate::model::Clip;
use crate::observed::ObservedAudio;
use crate::reconcile::{Action, Desired, Plan, set_manifest_artifact, set_manifest_stem};
use crate::tag::{
    Cover, TrackMetadata, flac_picture_data_budget, retag_flac, retag_mp3_with_timing,
    retag_wav_with_timing, tag_flac, tag_mp3_with_timing, tag_wav_with_timing,
};
use crate::tag_alac::{retag_alac, tag_alac};
use crate::vocab::{
    ArtifactKind, AudioFormat, LyricsTiming, SourceMode, StemFormat, WebpEncodeSettings,
};

mod artifact;
mod audio;
mod classify;
mod cover;
mod lifecycle;
mod stem;
mod tag;

use artifact::ArtifactRef;
use classify::*;
use lifecycle::{MoveSpec, PathGuard};
use stem::StemRef;

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
    /// Embed a bounded animated WebP as the audio file's front cover and retain
    /// the static JPEG as a secondary managed picture. Off embeds only the JPEG.
    pub embed_animated_cover: bool,
    /// Settings used for animated WebP cover transcodes.
    pub cover_webp: WebpEncodeSettings,
    /// Embed ID3 `SYLT` timing from fetched alignment. Plain embedded lyrics are
    /// independent and always flow through `TrackMetadata`.
    pub embed_synced_lyrics: bool,
    /// Granularity for synced `.lrc` and supported ID3 timing.
    pub lyrics_timing: LyricsTiming,
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
            embed_synced_lyrics: false,
            lyrics_timing: LyricsTiming::Line,
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
    /// Existing files whose managed state could not be verified safely. These
    /// are reported separately from attempted action failures because no write
    /// was attempted.
    pub unverifiable: Vec<Failure>,
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

    /// Merge a later execution phase into this outcome.
    pub fn merge(&mut self, mut other: Self) {
        self.downloaded += other.downloaded;
        self.reformatted += other.reformatted;
        self.retagged += other.retagged;
        self.renamed += other.renamed;
        self.deleted += other.deleted;
        self.skipped += other.skipped;
        self.artifacts_written += other.artifacts_written;
        self.artifacts_deleted += other.artifacts_deleted;
        self.unverifiable.append(&mut other.unverifiable);
        self.failures.append(&mut other.failures);
        if self.status == RunStatus::Completed {
            self.status = other.status;
        }
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
            Effect::Unverifiable(failure) => self.unverifiable.push(failure),
        }
    }
}

/// The durable library state one run mutates: the per-clip manifest, plus the
/// album and playlist art state that folder-level writes record against instead
/// of the manifest.
///
/// Grouped for the same reason as [`Ports`] and [`ExecOptions`]: the three
/// always travel together through the commit path, so one value threads them
/// through rather than three adjacent `&mut` arguments.
pub struct Stores<'a> {
    pub manifest: &'a mut Manifest,
    pub albums: &'a mut BTreeMap<String, AlbumArt>,
    pub playlists: &'a mut BTreeMap<String, PlaylistState>,
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
/// `synced` carries this run's fetched aligned lyrics keyed by clip id; it is the
/// caller's IO result, not part of the pure plan. Its plain text fills otherwise
/// missing `USLT`/`LYRICS`/`©lyr` metadata. Word-level `SYLT` is added only when
/// [`ExecOptions::embed_synced_lyrics`] is enabled. The `.lrc` sidecar itself is
/// a generated artifact whose body the caller already resolved into the plan.
pub async fn execute<H, F, G, C>(
    plan: &Plan,
    stores: Stores<'_>,
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
    let Stores {
        manifest,
        albums,
        playlists,
    } = stores;
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
    let lossless_unavailable = AtomicBool::new(false);
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
        lossless_unavailable: &lossless_unavailable,
    };

    let mut outcome = ExecOutcome::default();
    // Destinations whose write has actually committed this run, gating old-path
    // cleanup so a vacated sidecar/stem is kept only when a *successful* write
    // also targets it (#142). Serial commit order makes this a clean prefix.
    let mut committed: BTreeSet<String> = BTreeSet::new();
    let mut failed_playlist_writes: HashSet<(ArtifactKind, String)> = HashSet::new();

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
        if let Action::DeleteArtifact { kind, owner_id, .. } = action
            && is_playlist_kind(*kind)
            && failed_playlist_writes.contains(&(*kind, owner_id.clone()))
        {
            outcome.record(Effect::Skipped);
            continue;
        }
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
                    &mut PathGuard {
                        tracked_paths: &mut tracked_paths,
                        committed: &committed,
                    },
                ),
                Some(Ok(Prepared::Stem(prepared))) => ctx.commit_stem(
                    manifest,
                    prepared,
                    &mut PathGuard {
                        tracked_paths: &mut tracked_paths,
                        committed: &committed,
                    },
                ),
                Some(Err(fail)) => Err(fail),
                // Buffered fan-out yields exactly one result per prepareable action.
                #[allow(clippy::unreachable)]
                None => unreachable!("buffered yields one result per prepareable action"),
            }
        } else {
            ctx.apply(
                client_lock_ref,
                action,
                manifest,
                albums,
                playlists,
                &mut PathGuard {
                    tracked_paths: &mut tracked_paths,
                    committed: &committed,
                },
            )
            .await
        };
        match result {
            Ok(effect) => {
                let committed_path = written_path(action).map(str::to_owned);
                outcome.record(effect);
                // Record this action's destination now that its write succeeded.
                // A later action vacating a path removes it only when no
                // *committed* write also targets it; commit is strictly serial in
                // plan order, so a planned-but-failed or not-yet-run write never
                // protects a stale file from cleanup (#142).
                if let Some(dest) = committed_path {
                    committed.insert(dest);
                }
            }
            Err(fail) => {
                if let Action::WriteArtifact { kind, owner_id, .. } = action
                    && is_playlist_kind(*kind)
                {
                    failed_playlist_writes.insert((*kind, owner_id.clone()));
                }
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
            !kind.is_per_clip() || pre_clip_ids.contains(owner_id.as_str())
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
    effect: AudioEffect,
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
    static_fallback: Option<Vec<u8>>,
}

impl EmbedCover {
    /// Borrow as the [`Cover`] the taggers take.
    fn as_cover(&self) -> Cover<'_> {
        Cover {
            bytes: &self.bytes,
            mime: self.mime,
            static_fallback: self.static_fallback.as_deref(),
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
    Unverifiable(Failure),
}

#[derive(Debug, Clone, Copy)]
enum AudioEffect {
    Downloaded,
    Reformatted,
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
    matches!(
        kind,
        ArtifactKind::Playlist | ArtifactKind::PlaylistCoverJpg
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

/// The shared, read-only context threaded through every action handler.
struct Ctx<'a, H, F, G, C> {
    http: &'a H,
    fs: &'a F,
    ffmpeg: &'a G,
    clock: &'a C,
    opts: &'a ExecOptions,
    by_id: &'a HashMap<&'a str, &'a Desired>,
    by_path: &'a HashMap<&'a str, &'a Desired>,
    /// This run's fetched aligned lyrics, keyed by clip id. Audio tagging reads
    /// a clip's entry for plain fallback text and, when enabled, ID3 `SYLT`.
    /// Populated by the caller, so the engine stays free of direct IO.
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
    /// Set after the first entitlement refusal so later audio producers fail
    /// quickly without repeating a WAV request the account cannot fulfil.
    lossless_unavailable: &'a AtomicBool,
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
    async fn apply(
        &self,
        client_lock: &ClientLock<'_, C>,
        action: &Action,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        playlists: &mut BTreeMap<String, PlaylistState>,
        guard: &mut PathGuard<'_>,
    ) -> Result<Effect, Fail> {
        match action {
            // Audio actions are prepared concurrently, never routed through apply().
            #[allow(clippy::unreachable)]
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
            Action::Unverifiable {
                owner_id, reason, ..
            } => {
                self.refresh_preserve(manifest, owner_id);
                Ok(Effect::Unverifiable(Failure {
                    clip_id: owner_id.clone(),
                    reason: reason.clone(),
                }))
            }
            Action::VerifyArtifact {
                kind,
                path,
                source_hash,
                content_hash,
                owner_id,
            } => {
                let state = ArtifactState::verified(path, source_hash, content_hash);
                if is_album_kind(*kind) {
                    set_album_artifact(albums, owner_id, *kind, Some(state));
                } else if is_playlist_kind(*kind) {
                    set_playlist_artifact(
                        playlists,
                        owner_id,
                        *kind,
                        state,
                        playlist_name_from_path(path),
                    );
                } else if let Some(entry) = manifest.entries.get_mut(owner_id) {
                    set_manifest_artifact(entry, *kind, Some(state));
                }
                Ok(Effect::Skipped)
            }
            Action::VerifyAudio {
                clip_id,
                meta_hash,
                art_source_hash,
                art_content_hash,
                embedded_lyrics_hash,
                embedded_timed_lyrics_hash,
            } => {
                if let Some(entry) = manifest.entries.get_mut(clip_id) {
                    entry.meta_hash = meta_hash.clone();
                    match art_content_hash {
                        Some(content_hash) => {
                            entry.set_verified_art(art_source_hash, content_hash);
                        }
                        None => {
                            entry.art_hash = art_source_hash.clone();
                        }
                    }
                    entry.embedded_lyrics_hash = embedded_lyrics_hash.clone();
                    entry.embedded_timed_lyrics_hash = embedded_timed_lyrics_hash.clone();
                    if let Some(desired) = self.by_id.get(clip_id.as_str()).copied() {
                        entry.preserve = preserve_for(desired);
                    }
                }
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
                        if kind.is_per_clip() && manifest.get(owner_id).is_none() {
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
                    guard,
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
                    ArtifactRef {
                        kind: *kind,
                        owner_id,
                    },
                    MoveSpec {
                        from,
                        to,
                        source_url,
                        hash,
                    },
                    guard,
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
                    guard,
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
                    StemRef {
                        clip_id,
                        key,
                        stem_id,
                        format: *format,
                    },
                    MoveSpec {
                        from,
                        to,
                        source_url,
                        hash,
                    },
                    guard,
                )
                .await
            }
        }
    }

    /// Move the file and update the entry's path (and protection).
    fn rename(&self, manifest: &mut Manifest, from: &str, to: &str) -> Result<Effect, Fail> {
        let label = self
            .by_path
            .get(to)
            .map(|d| d.clip.id.clone())
            .unwrap_or_else(|| to.to_owned());
        self.fs.rename(from, to).map_err(|err| {
            disk_or_permanent(
                label,
                err.is_out_of_space(),
                "disk full: no space left to rename",
                format!("rename failed: {err}"),
            )
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
        self.fs.remove(path).map_err(|err| {
            disk_or_permanent(
                clip_id,
                err.is_out_of_space(),
                format!("disk full: no space left to remove {path}"),
                format!("delete failed: {err}"),
            )
        })?;
        manifest.remove(clip_id);
        Ok(Effect::Deleted)
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

    /// Run one authenticated client call, retrying transient core errors with the
    /// shared backoff ([`retry_core`](Self::retry_core)) until the budget is spent.
    ///
    /// The single home for the WAV render loop shape: `op` performs one attempt,
    /// acquiring and releasing the client lock as its future completes, so the
    /// backoff sleep in `retry_core` always runs unlocked and concurrent clips
    /// interleave rather than serialising behind one clip's retries.
    async fn retry_client<T>(
        &self,
        id: &str,
        mut op: impl AsyncFnMut() -> Result<T, Error>,
    ) -> Result<T, Fail> {
        let mut attempt: u32 = 0;
        loop {
            match op().await {
                Ok(value) => return Ok(value),
                Err(err) => match self.retry_core(id, err, &mut attempt).await {
                    Some(fail) => return Err(fail),
                    None => continue,
                },
            }
        }
    }

    /// GET `url`, retrying transient failures with backoff, verifying size.
    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        let result = self.fetch_bytes_once(url).await;
        self.retry_fetch_bytes(url, result).await
    }

    /// Continue a fetch after its first attempt has already been classified.
    async fn retry_fetch_bytes(
        &self,
        url: &str,
        mut result: Result<Vec<u8>, FetchError>,
    ) -> Result<Vec<u8>, FetchError> {
        let mut attempt: u32 = 0;
        loop {
            match result {
                Ok(body) => return Ok(body),
                Err(err) => {
                    if matches!(err.class, Class::Transient) && attempt < self.opts.max_retries {
                        let delay = backoff_delay(attempt, err.retry_after);
                        self.clock.sleep(delay).await;
                        attempt += 1;
                        result = self.fetch_bytes_once(url).await;
                    } else {
                        return Err(err);
                    }
                }
            }
        }
    }

    /// GET `url` once so callers that can refresh an expired URL own the retry.
    async fn fetch_bytes_once(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        classify_response(self.http.send(HttpRequest::get(url)).await)
    }

    /// Write `bytes` atomically, then confirm the on-disk size (SYNC-13/14).
    fn write_verify(&self, clip_id: &str, path: &str, bytes: &[u8]) -> Result<u64, Fail> {
        self.fs.write_atomic(path, bytes).map_err(|err| {
            disk_or_permanent(
                clip_id,
                err.is_out_of_space(),
                format!("disk full: no space left to write {path}"),
                format!("write failed: {err}"),
            )
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
            Some(d) => {
                let mut entry = manifest_entry(d, size);
                entry.path = path.to_owned();
                entry.format = format;
                if format != d.format {
                    entry.art_hash = embedded_art_hash(
                        &d.clip,
                        self.opts.embed_animated_cover && format.embeds_animated_cover(),
                        &self.opts.cover_webp,
                    );
                }
                entry
            }
            None => ManifestEntry {
                path: path.to_owned(),
                format,
                size,
                ..ManifestEntry::default()
            },
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
        embedded_lyrics_hash: d.embedded_lyrics_hash.clone(),
        embedded_timed_lyrics_hash: d.embedded_timed_lyrics_hash.clone(),
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

#[cfg(test)]
mod tests;
