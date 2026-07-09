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

/// Record the synced-lyrics resolution markers after this run's sidecar writes.
///
/// An instrumental (empty) clip is marked unconditionally so it is not re-fetched
/// every run. A clip that produced one or more bodies is marked only once EVERY
/// slot its fetch wrote (the `.lrc` and/or the `.lyrics.txt`) reflects that body's
/// hash, so a partial write (one slot ok, another failed or interrupted) leaves
/// no marker and is re-resolved next run rather than skipped. The missing slot
/// also re-targets the clip, so the two guards agree.
pub(crate) fn record_synced_lyrics_checks(
    manifest: &mut Manifest,
    pending: &[suno_core::PendingCheck],
) {
    let now = wallclock::now_secs();
    for check in pending {
        let durable = if check.empty {
            true
        } else if let Some(entry) = manifest.get(&check.clip_id) {
            // Durable only once EVERY written slot has landed. Match the kind
            // explicitly so a future artifact kind fails loud rather than
            // silently anchoring on the `.lyrics.txt` slot.
            !check.written_slots.is_empty()
                && check.written_slots.iter().all(|(kind, hash)| {
                    let slot = match kind {
                        suno_core::ArtifactKind::Lrc => entry.lrc.as_ref(),
                        suno_core::ArtifactKind::LyricsTxt => entry.lyrics_txt.as_ref(),
                        _ => None,
                    };
                    slot.map(|slot| &slot.hash) == Some(hash)
                })
        } else {
            false
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
    use suno_core::{ArtifactKind, ArtifactState, ManifestEntry, PendingCheck};

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

    #[test]
    fn lyrics_only_marker_persists_on_lyrics_txt_slot() {
        // A lyrics-only clip (no `.lrc`) whose body landed in the `.lyrics.txt`
        // slot records its durable marker anchored on that slot, so it converges
        // rather than re-resolving forever. Mirrors the `.lrc` durability.
        let mut manifest = Manifest::new();
        let entry = ManifestEntry {
            lyrics_txt: Some(ArtifactState {
                path: "a.lyrics.txt".to_string(),
                hash: "body-hash".to_string(),
            }),
            ..Default::default()
        };
        manifest.insert("a", entry);

        let pending = vec![PendingCheck {
            clip_id: "a".to_string(),
            empty: false,
            timed: true,
            written_slots: vec![(ArtifactKind::LyricsTxt, "body-hash".to_string())],
        }];
        record_synced_lyrics_checks(&mut manifest, &pending);

        let check = manifest.get("a").unwrap().synced_lyrics.clone();
        assert!(
            check.is_some(),
            "the marker persists off the `.lyrics.txt` slot"
        );
        assert!(check.unwrap().timed);
    }

    #[test]
    fn lyrics_only_marker_skipped_when_lyrics_txt_slot_missing_the_body() {
        // If the `.lyrics.txt` slot does not yet reflect the resolved body (an
        // interrupted or failed write), no marker is recorded, so the clip is
        // re-resolved next run rather than skipped with a stale sidecar.
        let mut manifest = Manifest::new();
        let entry = ManifestEntry {
            lyrics_txt: Some(ArtifactState {
                path: "a.lyrics.txt".to_string(),
                hash: "OLD".to_string(),
            }),
            ..Default::default()
        };
        manifest.insert("a", entry);

        let pending = vec![PendingCheck {
            clip_id: "a".to_string(),
            empty: false,
            timed: true,
            written_slots: vec![(ArtifactKind::LyricsTxt, "body-hash".to_string())],
        }];
        record_synced_lyrics_checks(&mut manifest, &pending);
        assert!(
            manifest.get("a").unwrap().synced_lyrics.is_none(),
            "no marker until the slot reflects the body -> retried"
        );
    }

    #[test]
    fn lyrics_txt_write_failure_is_retried_when_lrc_succeeded() {
        // Marker durability across both slots (#357 review): a both-sidecars fetch
        // where the `.lrc` write landed but the `.lyrics.txt` write failed
        // non-fatally. The marker lists BOTH slots, so it is durable only once
        // BOTH land; with the `.lyrics.txt` slot absent, no marker is recorded and
        // the clip is retried next run.
        let mut manifest = Manifest::new();
        let entry = ManifestEntry {
            lrc: Some(ArtifactState {
                path: "a.lrc".to_string(),
                hash: "lrc-hash".to_string(),
            }),
            // the `.lyrics.txt` write failed: no slot recorded for it.
            ..Default::default()
        };
        manifest.insert("a", entry);

        let pending = vec![PendingCheck {
            clip_id: "a".to_string(),
            empty: false,
            timed: true,
            written_slots: vec![
                (ArtifactKind::Lrc, "lrc-hash".to_string()),
                (ArtifactKind::LyricsTxt, "txt-hash".to_string()),
            ],
        }];
        record_synced_lyrics_checks(&mut manifest, &pending);
        assert!(
            manifest.get("a").unwrap().synced_lyrics.is_none(),
            "a partial write (only the `.lrc` landed) records no marker -> retried"
        );
    }

    #[test]
    fn both_slots_landed_records_the_marker() {
        // The convergent counterpart: once BOTH written slots reflect their body
        // hash, the clip is marked resolved (so it stops being a fetch target).
        let mut manifest = Manifest::new();
        let entry = ManifestEntry {
            lrc: Some(ArtifactState {
                path: "a.lrc".to_string(),
                hash: "lrc-hash".to_string(),
            }),
            lyrics_txt: Some(ArtifactState {
                path: "a.lyrics.txt".to_string(),
                hash: "txt-hash".to_string(),
            }),
            ..Default::default()
        };
        manifest.insert("a", entry);

        let pending = vec![PendingCheck {
            clip_id: "a".to_string(),
            empty: false,
            timed: true,
            written_slots: vec![
                (ArtifactKind::Lrc, "lrc-hash".to_string()),
                (ArtifactKind::LyricsTxt, "txt-hash".to_string()),
            ],
        }];
        record_synced_lyrics_checks(&mut manifest, &pending);
        assert!(
            manifest.get("a").unwrap().synced_lyrics.is_some(),
            "both slots landed -> resolved (converges)"
        );
    }
}
