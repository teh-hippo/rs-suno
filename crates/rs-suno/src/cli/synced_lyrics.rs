//! Synced-lyrics orchestration: fetch Suno's alignment, fill each clip's `.lrc`,
//! and record the resolution markers.
//!
//! Pure decisions (which clips to fetch, how to map a result onto the desired
//! artifact) live in `suno-core`; this module is only the IO glue. A fetch
//! failure never downgrades an existing `.lrc` and its warning never leaks the
//! clip id, request URL, or token.

use std::collections::HashMap;

use futures_util::stream::{self, StreamExt};
use suno_core::{AlignedLyrics, Manifest, SunoClient};

use crate::cli::task_output::eprint_t;
use crate::cli::wallclock;
use crate::clock::TokioClock;
use crate::http::ReqwestHttp;

/// The warning shown when a clip's alignment fetch fails. Deliberately carries
/// NO clip id, request URL, or error detail: a reqwest transport error's text
/// can include the full `/api/gen/{id}/...` URL, so the raw error is never
/// interpolated into any message (the clip id must not leak).
const SYNCED_LYRICS_FETCH_WARNING: &str = "could not fetch synced lyrics for a clip; its synced lyrics are skipped this run and retried next run";

/// Resolve this run's synced lyrics: fetch Suno's word/line alignment for the
/// clips that need it, fill each clip's `.lrc` artifact with its content-hashed
/// body, and return the per-clip alignment (for the executor's `SYLT`/plain
/// tags) plus the resolution checks to record after the writes land.
///
/// The pure [`synced_lyrics_targets`](suno_core::synced_lyrics_targets) decides
/// which clips to fetch (empty when the feature is off, and skipping clips
/// already resolved at this render version), and [`apply_synced_lrc`](suno_core::apply_synced_lrc)
/// maps each result onto the desired artifact; this function is only the IO glue.
/// A fetch failure keeps the clip's existing `.lrc`/tags untouched (no downgrade)
/// and is retried next run; its warning never prints the clip id, URL, or token.
pub(crate) async fn resolve_synced_lyrics(
    desired: &mut [suno_core::Desired],
    manifest: &Manifest,
    client: &SunoClient<TokioClock>,
    http: &ReqwestHttp,
    enabled: bool,
    verbosity: i8,
    concurrency: u32,
) -> (HashMap<String, AlignedLyrics>, Vec<suno_core::PendingCheck>) {
    let mut synced: HashMap<String, AlignedLyrics> = HashMap::new();
    let targets =
        suno_core::synced_lyrics_targets(desired, manifest, wallclock::now_secs(), enabled);
    let fetched = stream::iter(targets.iter())
        .map(|id| async move { (id.clone(), client.aligned_lyrics(http, id).await) })
        .buffered(concurrency.max(1) as usize)
        .collect::<Vec<_>>()
        .await;
    for (id, result) in fetched {
        match result {
            Ok(aligned) => {
                synced.insert(id, aligned);
            }
            Err(_) => {
                if verbosity >= -1 {
                    eprint_t!("warning: {SYNCED_LYRICS_FETCH_WARNING}");
                }
            }
        }
    }
    let pending = suno_core::apply_synced_lrc(desired, manifest, &synced);
    (synced, pending)
}

/// Record the synced-lyrics resolution markers after this run's `.lrc` writes.
///
/// An instrumental (empty) clip is marked unconditionally so it is not re-fetched
/// every run; a clip that produced a body is marked only once its `.lrc` slot
/// reflects that body's hash, so an interrupted or failed write leaves no marker
/// and is re-resolved next run rather than skipped.
pub(crate) fn record_synced_lyrics_checks(
    manifest: &mut Manifest,
    pending: &[suno_core::PendingCheck],
) {
    let now = wallclock::now_secs();
    for check in pending {
        let durable = if check.empty {
            true
        } else {
            match (&check.body_hash, manifest.get(&check.clip_id)) {
                (Some(hash), Some(entry)) => {
                    entry.lrc.as_ref().map(|slot| &slot.hash) == Some(hash)
                }
                _ => false,
            }
        };
        if !durable {
            continue;
        }
        if let Some(entry) = manifest.entries.get_mut(&check.clip_id) {
            entry.synced_lyrics = Some(suno_core::SyncedLyricsCheck {
                version: suno_core::SYNCED_LRC_VERSION,
                checked_unix: now,
                empty: check.empty,
                timed: check.timed,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_lyrics_fetch_warning_never_leaks_a_clip_id_or_url() {
        // The fetch-failure warning must not carry the request URL or clip id: a
        // reqwest transport error's text can include `/api/gen/{id}/...`, so the
        // raw error is never interpolated. This guards that redaction.
        let msg = SYNCED_LYRICS_FETCH_WARNING;
        assert!(!msg.contains("/api/gen/"));
        assert!(!msg.contains("aligned_lyrics"));
        assert!(!msg.contains('{'), "no interpolation placeholder");
        assert!(!msg.contains("http"));
    }
}
