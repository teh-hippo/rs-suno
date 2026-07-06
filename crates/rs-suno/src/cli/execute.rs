//! Plan build and execution: turn the desired set into a reconciled plan and
//! run it, then persist the manifest, graph, logs, and last-run marker.
//!
//! The commit phase races the executor against an interrupt signal so a
//! cancellation preserves partial progress, and a full disk aborts the run
//! without leaving the library changed. Every safety decision (what may be
//! deleted) is delegated to `suno-core`; this module only sequences the IO.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use suno_core::{
    AlbumArt, AlbumDesired, AlignedLyrics, Clip, ExecOptions, Filesystem, LocalFile,
    PlaylistDesired, PlaylistState, Ports, RunStatus, SourceStatus, SunoClient, deletion_allowed,
    plan_album_artifacts, plan_playlist_artifacts, reconcile,
};

use crate::cli::desired::{ExitCode, run_exit_code};
use crate::cli::last_run;
use crate::cli::logs;
use crate::cli::output;
use crate::cli::signal;
use crate::cli::synced_lyrics;
use crate::cli::task_output::eprint_t;
use crate::clock::TokioClock;
use crate::download::cleanup_stale_parts;
use crate::ffmpeg::FfmpegAdapter;
use crate::fs::FsAdapter;
use crate::http::ReqwestHttp;

const WAV_POLL_ATTEMPTS: u32 = 24;
const WAV_POLL_INTERVAL: Duration = Duration::from_secs(5);

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_plan(
    summary_label: &'static str,
    plan: suno_core::Plan,
    desired: &[suno_core::Desired],
    mut manifest: suno_core::Manifest,
    synced: HashMap<String, AlignedLyrics>,
    pending_checks: Vec<suno_core::PendingCheck>,
    store: &mut suno_core::LineageStore,
    client: &SunoClient<TokioClock>,
    http: &ReqwestHttp,
    dest: &Path,
    settings: &suno_core::EffectiveSettings,
    account: &str,
    verbosity: i8,
    library_authoritative: bool,
) -> Result<ExitCode> {
    cleanup_stale_parts(dest);
    let fs = FsAdapter::new(dest);
    let ffmpeg = FfmpegAdapter::new(dest);
    let clock = TokioClock;
    let opts = ExecOptions {
        max_retries: settings.retries,
        wav_poll_attempts: WAV_POLL_ATTEMPTS,
        wav_poll_interval: WAV_POLL_INTERVAL,
        concurrency: settings.concurrency,
        cover_webp: settings.animated_cover_webp,
    };
    let started = std::time::Instant::now();

    let outcome = {
        let ports = Ports {
            client,
            http,
            fs: &fs,
            ffmpeg: &ffmpeg,
            clock: &clock,
        };
        tokio::select! {
            out = suno_core::execute(&plan, &mut manifest, &mut store.albums, &mut store.playlists, desired, &synced, ports, &opts) => Some(out),
            _ = signal::wait_for_signal() => None,
        }
    };

    let Some(outcome) = outcome else {
        logs::save_manifest(dest, &manifest)?;
        // Folder art may have been written before the interrupt; persist the
        // album-art store so those sidecars are tracked on the next run.
        logs::save_graph(dest, store)?;
        // A signal cancels the executor mid-flight, before its own end-of-run
        // prune; tidy any directories emptied by moves/deletes so far. The
        // completed path is already pruned inside `execute`.
        let _ = fs.prune_empty_dirs("");
        eprint_t!(
            "warning: interrupted -- partial run saved\n  Progress so far is recorded in the manifest; re-run to continue."
        );
        return Ok(ExitCode::Interrupted);
    };

    if outcome.status == RunStatus::DiskFull {
        // A full disk aborts the run; persistence would only re-hit ENOSPC, so
        // save best-effort (mirroring the interrupt path) and stop before the
        // `?`-propagating summary writes below. The summary and hint are
        // eprintln-only, so they never re-hit the full disk.
        let _ = logs::save_manifest(dest, &manifest);
        let _ = logs::save_graph(dest, store);
        let _ = fs.prune_empty_dirs("");
        // The counter block honours quiet mode, but the actionable error and its
        // specific reason always print (even under `-qq`), matching main.rs.
        if verbosity >= -1 {
            eprint_t!(
                "{}",
                output::run_summary(
                    summary_label,
                    account,
                    &outcome,
                    started.elapsed().as_secs_f64()
                )
            );
        }
        eprint_t!(
            "error: {} The library is unchanged for the failing action.",
            crate::diskspace::DISK_FULL_HINT
        );
        if let Some(last) = outcome.failures.last() {
            eprint_t!("  {}", last.reason);
        }
        return Ok(ExitCode::DiskFull);
    }

    // Record the synced-lyrics resolution markers now the writes have landed:
    // an instrumental is marked so it is not re-fetched every run, and a written
    // clip is marked only once its `.lrc` slot reflects the body (so an
    // interrupted or failed write is re-resolved next run rather than skipped).
    synced_lyrics::record_synced_lyrics_checks(&mut manifest, &pending_checks);

    logs::save_manifest(dest, &manifest)?;
    // Persist the graph again after execute: the lineage part was already saved
    // for durability before execute, but album-art state is mutated *during*
    // execute (folder.jpg / cover.webp writes and deletes), so it lands now.
    logs::save_graph(dest, store)?;
    let clips_by_id: HashMap<&str, &Clip> = desired
        .iter()
        .map(|d| (d.clip.id.as_str(), &d.clip))
        .collect();
    // Best-effort library index: a regenerable scripting artefact, so a failure
    // to write it must never fail an otherwise-green mirror (unlike the
    // manifest). Gated on an authoritative Library (D4), not playlist membership:
    // a narrowed `--limit`/`--since` or area-only run sees only a window of clips
    // live, so it would null the artist/tags/duration of every out-of-window clip
    // and regress a richer index from a prior full run; only an authoritative
    // Library run writes, avoiding that live-field oscillation.
    if library_authoritative
        && let Err(err) = logs::save_index(dest, &manifest, store, &clips_by_id)
        && verbosity >= -1
    {
        eprint_t!("warning: could not write {}: {err}", logs::INDEX_NAME);
    }
    logs::append_failures(dest, &outcome.failures, &clips_by_id)?;
    let failed: HashSet<&str> = outcome
        .failures
        .iter()
        .map(|f| f.clip_id.as_str())
        .collect();
    let rename_owner: HashMap<&str, &str> = desired
        .iter()
        .map(|d| (d.path.as_str(), d.clip.id.as_str()))
        .collect();
    logs::append_audit(dest, &plan, &failed, &rename_owner)?;
    last_run::write_last_run(dest);

    if verbosity >= 1 {
        for line in output::action_lines(&plan, &failed, verbosity) {
            eprint_t!("{line}");
        }
    }

    if !outcome.failures.is_empty() && verbosity >= -1 {
        eprint_t!(
            "warning: {} clip(s) failed after retries\n  See {} for details.",
            outcome.failures.len(),
            dest.join(".suno-failures.log").display()
        );
    }
    if verbosity >= -1 {
        eprint_t!(
            "{}",
            output::run_summary(
                summary_label,
                account,
                &outcome,
                started.elapsed().as_secs_f64()
            )
        );
    }

    Ok(run_exit_code(&outcome))
}

/// The inputs to [`reconcile_run`]: the loaded manifest and destination plus the
/// assembled desired state and the deletion gates. Bundled so both run-mode
/// tails build one value instead of threading ten positional arguments.
pub(crate) struct ReconcileInputs<'a> {
    pub manifest: &'a suno_core::Manifest,
    pub dest: &'a Path,
    pub desired: &'a [suno_core::Desired],
    pub albums_desired: &'a [AlbumDesired],
    pub albums: &'a BTreeMap<String, AlbumArt>,
    pub playlist_desired: &'a [PlaylistDesired],
    pub playlists: &'a BTreeMap<String, PlaylistState>,
    pub sources: &'a [SourceStatus],
    pub library_authoritative: bool,
    pub playlists_enumerated: bool,
}

/// Reconcile `desired` against `manifest` (already loaded), then append the
/// folder-art and playlist plans.
///
/// Shared by the dry-run and executing paths. The manifest is loaded and the
/// desired `.lrc` artifacts resolved by the caller *before* this, so reconcile
/// sees each `.lrc`'s real content hash. Statting absent files is harmless, so
/// this never creates the destination directory. The folder-art actions share
/// the run's single deletion verdict ([`deletion_allowed`]) so album art is
/// never removed on an incomplete listing, and they land on the same [`Plan`] so
/// the mass-delete cap and the confirmation prompt already cover them.
///
/// Playlists carry a second, independent gate: `playlists_enumerated` is true
/// only when the playlist listing succeeded on a fully-enumerated run.
/// [`plan_playlist_artifacts`] emits a playlist delete only when BOTH the shared
/// `can_delete` verdict and `playlists_enumerated` hold, so a failed, empty, or
/// partial playlist listing never removes an existing `.m3u8` (HARDENING B2).
/// These deletes also count toward the mass-delete cap via [`Plan::artifact_deletes`].
///
/// `sources` is one [`SourceStatus`] per selected area, so [`deletion_allowed`]
/// requires every area fully enumerated and at least one Mirror. Folder art
/// carries the extra `library_authoritative` gate: without an authoritative
/// Library the folder view is partial, so art is neither rewritten (the caller
/// passes an empty `albums_desired`) nor deleted.
pub(crate) async fn reconcile_run(inputs: &ReconcileInputs<'_>) -> suno_core::Plan {
    let local = stat_manifest(
        inputs.dest,
        inputs.manifest,
        inputs.albums,
        inputs.playlists,
    )
    .await;
    let can_delete = deletion_allowed(inputs.sources);
    let art_can_delete = can_delete && inputs.library_authoritative;
    let mut plan = reconcile(inputs.manifest, inputs.desired, &local, inputs.sources);
    plan.actions.extend(plan_album_artifacts(
        inputs.albums_desired,
        inputs.albums,
        art_can_delete,
        &local,
    ));
    plan.actions.extend(plan_playlist_artifacts(
        inputs.playlist_desired,
        inputs.playlists,
        can_delete,
        inputs.playlists_enumerated,
        &local,
    ));
    plan
}

/// Stat every manifest path and all tracked artifact paths so reconcile can
/// spot missing or empty files.
///
/// Returns a combined map keyed by both clip-id (for audio) and file path (for
/// per-clip sidecars, folder art, and playlist files). Statting absent paths is
/// harmless; the caller's destination directory need not exist yet.
async fn stat_manifest(
    dest: &Path,
    manifest: &suno_core::Manifest,
    albums: &BTreeMap<String, AlbumArt>,
    playlists: &BTreeMap<String, PlaylistState>,
) -> HashMap<String, LocalFile> {
    // Collect (key, absolute_path) pairs to stat. Audio is keyed by clip_id;
    // everything else is keyed by its stored relative path, deduplicated.
    let mut to_stat: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (clip_id, entry) in manifest.iter() {
        // Audio file, keyed by clip_id (may share a path with another clip; stat separately).
        to_stat.push((clip_id.clone(), dest.join(&entry.path)));

        for path in [
            entry.cover_jpg.as_ref().map(|s| s.path.as_str()),
            entry.cover_webp.as_ref().map(|s| s.path.as_str()),
            entry.details_txt.as_ref().map(|s| s.path.as_str()),
            entry.lyrics_txt.as_ref().map(|s| s.path.as_str()),
            entry.lrc.as_ref().map(|s| s.path.as_str()),
            entry.video_mp4.as_ref().map(|s| s.path.as_str()),
        ]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        {
            if seen.insert(path.to_owned()) {
                to_stat.push((path.to_owned(), dest.join(path)));
            }
        }

        for state in entry.stems.values().filter(|s| !s.path.is_empty()) {
            if seen.insert(state.path.clone()) {
                to_stat.push((state.path.clone(), dest.join(&state.path)));
            }
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
        .filter(|s| !s.path.is_empty())
        {
            if seen.insert(state.path.clone()) {
                to_stat.push((state.path.clone(), dest.join(&state.path)));
            }
        }
    }

    for state in playlists.values().filter(|s| !s.path.is_empty()) {
        if seen.insert(state.path.clone()) {
            to_stat.push((state.path.clone(), dest.join(&state.path)));
        }
    }

    tokio::task::spawn_blocking(move || {
        to_stat
            .into_iter()
            .map(|(key, path)| {
                let meta = std::fs::metadata(&path).ok();
                let local = LocalFile {
                    exists: meta.is_some(),
                    size: meta.map(|m| m.len()).unwrap_or(0),
                };
                (key, local)
            })
            .collect()
    })
    .await
    .expect("stat_manifest blocking task panicked")
}

/// Whether a file extension names one of the audio formats we write.
fn is_audio_ext(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "flac" | "mp3" | "wav")
}

/// Walk `dest` recursively for audio files, returning their paths relative to
/// `dest` with forward slashes, for the orphan report. Best-effort and
/// read-only: an unreadable directory (or an absent `dest`) contributes
/// nothing, so a dry run never fails on a walk error.
pub(crate) fn walk_audio_files(dest: &Path) -> Vec<String> {
    fn recurse(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                recurse(root, &path, out);
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(is_audio_ext)
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    recurse(dest, dest, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::wallclock;
    use suno_core::SourceMode;

    #[tokio::test]
    async fn reconcile_run_reads_a_missing_destination_as_empty() {
        // The dry-run / check path reads through a missing destination as an
        // empty manifest without creating it, so it never touches disk.
        let dir = Path::new("target").join(format!(
            "run-nodir-{}-{}",
            std::process::id(),
            wallclock::now_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!dir.exists());
        let sources = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        }];
        let manifest = logs::load_manifest(&dir).unwrap();
        let plan = reconcile_run(&ReconcileInputs {
            manifest: &manifest,
            dest: &dir,
            desired: &[],
            albums_desired: &[],
            albums: &BTreeMap::new(),
            playlist_desired: &[],
            playlists: &BTreeMap::new(),
            sources: &sources,
            library_authoritative: false,
            playlists_enumerated: false,
        })
        .await;
        assert!(manifest.is_empty());
        assert!(plan.actions.is_empty());
        assert!(
            !dir.exists(),
            "dry-run path must not create the destination directory"
        );
    }
}
