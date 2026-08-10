//! Lyric orchestration: fetch Suno's alignment for baseline audio metadata and
//! optional sidecars, then record the durable sidecar markers.
//!
//! Pure decisions (which clips to fetch, how to map a result onto the desired
//! artifact) live in `suno-core`; this module is only the IO glue. A fetch
//! failure never downgrades an existing `.lrc` and its warning never leaks the
//! clip id, request URL, or token.
//!
//! Every run mode shares [`resolve_synced_lyrics`]: `check` and `--dry-run`
//! resolve exactly what an executing run resolves, then drop the durable
//! markers, so a report predicts the writes a run would make (#537). Only an
//! executing run reaches [`record_synced_lyrics_checks`], and only after its
//! writes have landed.

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
/// interpolated into any message (the clip id must not leak). Worded for every
/// run mode, because reporting and executing runs share this fetch.
const LYRICS_FETCH_WARNING: &str = "could not fetch lyrics for one or more clips; their existing tags and sidecars are unchanged, this run's lyric view is incomplete, and the lookup will be retried next run";

/// This run's resolved lyrics.
///
/// `aligned` is the fetched alignment the executor re-reads while tagging;
/// `pending` holds the durable markers an EXECUTING run records once its writes
/// land (a reporting run drops them, so nothing is stamped); `failed` counts the
/// REQUIRED requests that errored.
///
/// A failed request stays explicit rather than becoming a success-shaped
/// placeholder: the clip is simply absent from `aligned`, so `suno-core` keeps
/// its stored state and records no marker, and `failed` lets the caller say the
/// run's lyric view is incomplete instead of silently reporting convergence.
pub(crate) struct ResolvedLyrics {
    /// Alignment for every clip whose fetch succeeded, keyed by clip id.
    pub(crate) aligned: HashMap<String, AlignedLyrics>,
    /// Durable markers to record after the writes land (executing runs only).
    pub(crate) pending: Vec<suno_core::PendingCheck>,
    /// How many required alignment requests failed.
    failed: usize,
}

impl ResolvedLyrics {
    /// Whether every required alignment request succeeded, so this run's lyric
    /// state (and any report built from it) is complete.
    pub(crate) fn is_complete(&self) -> bool {
        self.failed == 0
    }
}

/// Resolve this run's lyrics: fetch Suno's word/line alignment for clips missing
/// plain embedded lyrics or needing a sidecar, fill generated sidecar bodies,
/// and return the alignment plus the checks to record after writes land.
///
/// The pure [`synced_lyrics_targets`](suno_core::synced_lyrics_targets) decides
/// which clips to fetch, and
/// [`apply_synced_lrc`](suno_core::apply_synced_lrc) maps each result onto audio
/// intent and desired artifacts. Both are mode-independent, so `check`,
/// `--dry-run`, and an executing run issue the same requests and reach the same
/// desired state for the same manifest and upstream snapshot. A failure keeps
/// existing tags and sidecars untouched and is retried next run; its warning
/// prints no id, URL, or token.
pub(crate) async fn resolve_synced_lyrics(
    desired: &mut [suno_core::Desired],
    manifest: &Manifest,
    client: &SunoClient<TokioClock>,
    http: &ReqwestHttp,
    verbosity: i8,
    concurrency: u32,
    timing: suno_core::LyricsTiming,
) -> ResolvedLyrics {
    let targets = suno_core::synced_lyrics_targets_with_timing(
        desired,
        manifest,
        wallclock::now_secs(),
        timing,
    );
    let fetched = stream::iter(targets.iter())
        .map(|id| async move { (id.clone(), client.aligned_lyrics(http, id).await) })
        .buffered(concurrency.max(1) as usize)
        .collect::<Vec<_>>()
        .await;
    collect_resolution(desired, manifest, fetched, timing, verbosity)
}

/// Fold this run's fetch results onto the desired state: successes resolve their
/// clip, failures are dropped so `suno-core` keeps the stored state, and the
/// failure count travels back with the result.
///
/// Split from the fetch above so the mapping (and the warning's redaction) is
/// unit-tested without a network.
fn collect_resolution(
    desired: &mut [suno_core::Desired],
    manifest: &Manifest,
    fetched: Vec<(String, suno_core::Result<AlignedLyrics>)>,
    timing: suno_core::LyricsTiming,
    verbosity: i8,
) -> ResolvedLyrics {
    let mut aligned: HashMap<String, AlignedLyrics> = HashMap::new();
    let mut failed = 0usize;
    for (id, result) in fetched {
        match result {
            Ok(lyrics) => {
                aligned.insert(id, lyrics);
            }
            Err(_) => {
                failed += 1;
            }
        }
    }
    if failed > 0 && verbosity >= -1 {
        eprint_t!("warning: {LYRICS_FETCH_WARNING} ({failed} failed)");
    }
    let pending = suno_core::apply_synced_lrc_with_timing(desired, manifest, &aligned, timing);
    ResolvedLyrics {
        aligned,
        pending,
        failed,
    }
}

/// Record the synced-lyrics resolution markers after this run's sidecar writes.
///
/// Only an executing run reaches here: a reporting run drops the markers, so no
/// `checked_unix` is stamped and its manifest is untouched. An empty result is
/// marked unconditionally, so the negative result is trusted for the re-check
/// window (a known instrumental stops costing a request every run). A clip that
/// produced sidecar bodies is marked only once every slot reflects the expected
/// hash, so a partial write is retried.
pub(crate) fn record_synced_lyrics_checks(
    manifest: &mut Manifest,
    pending: &[suno_core::PendingCheck],
) {
    let now = wallclock::now_secs();
    for check in pending {
        let slots_durable = if check.empty {
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
                    // `source_hash()`, not the raw field: a verified state packs
                    // the committed content hash into the same field, and the
                    // written slot is identified by its source hash.
                    slot.map(|slot| slot.source_hash()) == Some(hash.as_str())
                })
        } else {
            false
        };
        let timed_embed_durable = check.timed_embed_hash.as_ref().is_none_or(|hash| {
            manifest
                .get(&check.clip_id)
                .is_some_and(|entry| &entry.embedded_timed_lyrics_hash == hash)
        });
        if !slots_durable || !timed_embed_durable {
            continue;
        }
        if let Some(entry) = manifest.entries.get_mut(&check.clip_id) {
            let prior = entry.synced_lyrics.as_ref();
            let preserve_timed_state = check.timing.is_none() && prior.is_some();
            let prior_timed_version = prior.map(|state| state.timed_version).unwrap_or_default();
            let prior_timing = prior.and_then(|state| state.timing);
            let empty = if preserve_timed_state {
                prior.is_some_and(|state| state.empty)
            } else {
                check.empty
            };
            let timed = if preserve_timed_state {
                prior.is_some_and(|state| state.timed)
            } else {
                check.timed
            };
            entry.synced_lyrics = Some(suno_core::SyncedLyricsCheck {
                version: suno_core::SYNCED_LRC_VERSION,
                checked_unix: now,
                empty,
                timed,
                timed_version: if check.timing.is_some() {
                    suno_core::TIMED_LYRICS_VERSION
                } else {
                    prior_timed_version
                },
                timing: check.timing.or(prior_timing),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::task_output::{capture_task_stderr, flush_task_stderr};
    use suno_core::{
        AlignedLine, AlignedLyrics, ArtifactKind, ArtifactState, AudioFormat, Clip, Desired,
        DesiredArtifact, LineageContext, LyricsTiming, ManifestEntry, PendingCheck, SourceMode,
        SyncedLyricsCheck,
    };

    /// A clip desired as FLAC with a `.lrc` sidecar and no inline lyrics, so it
    /// depends entirely on the fetched alignment.
    fn desired(id: &str) -> Desired {
        let clip = Clip {
            id: id.to_string(),
            title: "Song".to_string(),
            prompt: "a prompt".to_string(),
            ..Default::default()
        };
        Desired {
            lineage: LineageContext::own_root(&clip),
            path: format!("{id}.flac"),
            format: AudioFormat::Flac,
            meta_hash: "m".to_string(),
            art_hash: "a".to_string(),
            embedded_lyrics_hash: String::new(),
            embedded_timed_lyrics_hash: String::new(),
            lyrics_reencode_safe: true,
            modes: vec![SourceMode::Mirror],
            trashed: false,
            private: false,
            artifacts: vec![DesiredArtifact {
                kind: ArtifactKind::Lrc,
                path: format!("{id}.lrc"),
                source_url: String::new(),
                hash: "pending".to_string(),
                content: None,
            }],
            clip,
            stems: None,
        }
    }

    fn one_line_alignment() -> AlignedLyrics {
        AlignedLyrics {
            lines: vec![AlignedLine {
                text: "hi there".to_owned(),
                start_s: 0.5,
                end_s: 1.2,
                section: "Verse 1".to_owned(),
                words: Vec::new(),
            }],
            ..Default::default()
        }
    }

    /// The state a resolution leaves behind that drives reconcile's actions:
    /// the clip id, both embed fingerprints, and each artifact's path and hash.
    #[derive(Debug, PartialEq, Eq)]
    struct Fingerprint {
        clip_id: String,
        embedded_lyrics_hash: String,
        embedded_timed_lyrics_hash: String,
        artifacts: Vec<(String, String)>,
    }

    fn fingerprint(desired: &[Desired]) -> Vec<Fingerprint> {
        desired
            .iter()
            .map(|d| Fingerprint {
                clip_id: d.clip.id.clone(),
                embedded_lyrics_hash: d.embedded_lyrics_hash.clone(),
                embedded_timed_lyrics_hash: d.embedded_timed_lyrics_hash.clone(),
                artifacts: d
                    .artifacts
                    .iter()
                    .map(|a| (a.path.clone(), a.hash.clone()))
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn synced_lyrics_fetch_warning_never_leaks_a_clip_id_or_url() {
        // The fetch-failure warning must not carry the request URL or clip id: a
        // reqwest transport error's text can include `/api/gen/{id}/...`, so the
        // raw error is never interpolated. This guards that redaction.
        let msg = LYRICS_FETCH_WARNING;
        assert!(!msg.contains("/api/gen/"));
        assert!(!msg.contains("aligned_lyrics"));
        assert!(!msg.contains('{'), "no interpolation placeholder");
        assert!(!msg.contains("http"));
    }

    #[test]
    fn reporting_and_executing_resolution_are_the_same_resolution() {
        // #537: `check`/`--dry-run` and an executing run fold the SAME fetch
        // results onto the desired state, so their action-driving inputs match
        // exactly. Only the executing run goes on to record the markers; the
        // reporting run drops `pending`, leaving its manifest as it loaded it.
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            ManifestEntry {
                path: "a.flac".to_string(),
                format: AudioFormat::Flac,
                ..Default::default()
            },
        );
        let fetched = || {
            vec![(
                "a".to_string(),
                Ok(one_line_alignment()) as suno_core::Result<AlignedLyrics>,
            )]
        };

        let mut report = vec![desired("a")];
        let report_resolved = collect_resolution(
            &mut report,
            &manifest,
            fetched(),
            LyricsTiming::Line,
            -2, // quiet: the warning path is asserted separately
        );
        let mut execute = vec![desired("a")];
        let execute_resolved =
            collect_resolution(&mut execute, &manifest, fetched(), LyricsTiming::Line, -2);

        assert_eq!(fingerprint(&report), fingerprint(&execute));
        assert_eq!(report_resolved.pending, execute_resolved.pending);
        assert!(report_resolved.is_complete() && execute_resolved.is_complete());
        assert!(
            !report[0].artifacts[0].hash.is_empty() && report[0].artifacts[0].content.is_some(),
            "the report holds the real body, not a placeholder"
        );

        // The reporting run stops here: nothing is stamped.
        let reporting_manifest = manifest.clone();
        assert!(reporting_manifest.get("a").unwrap().synced_lyrics.is_none());

        // The executing run records once its write has landed.
        let mut executed = manifest.clone();
        executed.entries.get_mut("a").unwrap().lrc = Some(ArtifactState {
            path: "a.lrc".to_string(),
            hash: execute[0].artifacts[0].hash.clone(),
        });
        record_synced_lyrics_checks(&mut executed, &execute_resolved.pending);
        assert!(
            executed.get("a").unwrap().synced_lyrics.is_some(),
            "the executing run stamps the marker once the slot landed"
        );
    }

    #[test]
    fn a_failed_fetch_keeps_state_and_is_reported_as_incomplete() {
        // A required lookup that errors must never become a success-shaped
        // placeholder: the clip keeps its stored slot hash (so no rewrite and no
        // downgrade), records no marker (so it is retried), and the resolution
        // reports itself incomplete so the caller can say the run's lyric view
        // is only a lower bound.
        const ID: &str = "secret-clip-id";
        let mut manifest = Manifest::new();
        manifest.insert(
            ID,
            ManifestEntry {
                path: format!("{ID}.flac"),
                format: AudioFormat::Flac,
                lrc: Some(ArtifactState {
                    path: format!("{ID}.lrc"),
                    hash: "stored".to_string(),
                }),
                embedded_lyrics_hash: "kept".to_string(),
                synced_lyrics: Some(SyncedLyricsCheck {
                    version: suno_core::SYNCED_LRC_VERSION,
                    checked_unix: 1,
                    empty: false,
                    timed: false,
                    timed_version: suno_core::TIMED_LYRICS_VERSION,
                    timing: Some(LyricsTiming::Line),
                }),
                ..Default::default()
            },
        );
        let mut desired = vec![desired(ID)];
        let fetched = vec![(
            ID.to_string(),
            Err(suno_core::Error::Api(format!(
                "GET /api/gen/{ID}/aligned_lyrics/v2/ failed"
            ))),
        )];

        capture_task_stderr();
        let resolved = collect_resolution(&mut desired, &manifest, fetched, LyricsTiming::Line, 0);
        let lines = flush_task_stderr();

        assert!(!resolved.is_complete(), "the failure stays explicit");
        assert!(resolved.aligned.is_empty());
        assert!(resolved.pending.is_empty(), "no marker -> retried next run");
        assert_eq!(desired[0].artifacts[0].hash, "stored", "no rewrite");
        assert_eq!(desired[0].artifacts[0].content, None);
        assert_eq!(desired[0].embedded_lyrics_hash, "kept", "no retag");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("(1 failed)"));
        assert!(!lines[0].contains(ID), "the clip id never leaks");
        assert!(!lines[0].contains("/api/gen/"), "the URL never leaks");
    }

    #[test]
    fn a_recorded_empty_result_stops_the_every_run_probe() {
        // The negative marker an executing run records must stop the embed probe
        // for the re-check window: a known instrumental is a target once, then
        // not again until the window elapses.
        let mut manifest = Manifest::new();
        manifest.insert(
            "instr",
            ManifestEntry {
                path: "instr.flac".to_string(),
                format: AudioFormat::Flac,
                ..Default::default()
            },
        );
        let mut desired = vec![desired("instr")];
        desired[0].artifacts.clear(); // embed-only: no sidecars enabled

        let now = 100_000;
        assert!(
            suno_core::synced_lyrics_targets(&desired, &manifest, now).contains("instr"),
            "an unresolved embed is probed"
        );
        let resolved = collect_resolution(
            &mut desired,
            &manifest,
            vec![("instr".to_string(), Ok(AlignedLyrics::default()))],
            LyricsTiming::Line,
            -2,
        );
        record_synced_lyrics_checks(&mut manifest, &resolved.pending);
        assert!(
            manifest.get("instr").unwrap().synced_lyrics.is_some(),
            "the negative result is recorded"
        );

        let stamped = manifest
            .get("instr")
            .unwrap()
            .synced_lyrics
            .as_ref()
            .unwrap()
            .checked_unix;
        assert!(
            suno_core::synced_lyrics_targets(&desired, &manifest, stamped + 1).is_empty(),
            "inside the window the known instrumental costs no request"
        );
        assert!(
            suno_core::synced_lyrics_targets(
                &desired,
                &manifest,
                stamped + suno_core::SYNCED_LRC_RECHECK_SECS + 1
            )
            .contains("instr"),
            "past the window it is probed again"
        );
    }

    #[test]
    fn a_timing_migration_is_one_shot_through_the_recorded_marker() {
        // A legacy MP3 whose marker predates the timing field is a target once;
        // after the run's writes land and the marker is recorded, it converges.
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            ManifestEntry {
                path: "a.mp3".to_string(),
                format: AudioFormat::Mp3,
                lrc: Some(ArtifactState {
                    path: "a.lrc".to_string(),
                    hash: "legacy-body".to_string(),
                }),
                embedded_lyrics_hash: "plain".to_string(),
                embedded_timed_lyrics_hash: "legacy-sylt".to_string(),
                synced_lyrics: Some(SyncedLyricsCheck {
                    version: suno_core::SYNCED_LRC_VERSION,
                    checked_unix: 1,
                    empty: false,
                    timed: true,
                    timed_version: 0,
                    timing: None,
                }),
                ..Default::default()
            },
        );
        let mut desired = vec![desired("a")];
        desired[0].format = AudioFormat::Mp3;
        desired[0].path = "a.mp3".to_string();

        assert!(
            suno_core::synced_lyrics_targets_with_timing(
                &desired,
                &manifest,
                100_000,
                LyricsTiming::Line
            )
            .contains("a"),
            "the migration targets the clip once"
        );
        let resolved = collect_resolution(
            &mut desired,
            &manifest,
            vec![("a".to_string(), Ok(one_line_alignment()))],
            LyricsTiming::Line,
            -2,
        );

        // The writes land: the sidecar slot and the timed embed take the resolved
        // hashes, then the marker is recorded.
        let entry = manifest.entries.get_mut("a").unwrap();
        entry.lrc = Some(ArtifactState {
            path: "a.lrc".to_string(),
            hash: desired[0].artifacts[0].hash.clone(),
        });
        entry.embedded_timed_lyrics_hash = desired[0].embedded_timed_lyrics_hash.clone();
        entry.embedded_lyrics_hash = desired[0].embedded_lyrics_hash.clone();
        record_synced_lyrics_checks(&mut manifest, &resolved.pending);

        let state = manifest.get("a").unwrap().synced_lyrics.as_ref().unwrap();
        assert_eq!(state.timing, Some(LyricsTiming::Line));
        assert_eq!(state.timed_version, suno_core::TIMED_LYRICS_VERSION);
        assert!(
            suno_core::synced_lyrics_targets_with_timing(
                &desired,
                &manifest,
                100_000,
                LyricsTiming::Line
            )
            .is_empty(),
            "the migration does not repeat"
        );
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
            timing: None,
            timed_embed_hash: None,
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
    fn a_verified_slot_still_records_its_marker() {
        // The executor now records a committed sidecar as a verified state, which
        // packs the source and content hashes into the one field. The durability
        // check reads it back with `source_hash()`, so the marker still lands; the
        // raw field would never match and the clip would re-fetch every run.
        let mut manifest = Manifest::new();
        let entry = ManifestEntry {
            lrc: Some(ArtifactState::verified("a.lrc", "body-hash", "bytes-hash")),
            lyrics_txt: Some(ArtifactState::verified(
                "a.lyrics.txt",
                "txt-hash",
                "txt-bytes",
            )),
            ..Default::default()
        };
        manifest.insert("a", entry);

        let pending = vec![PendingCheck {
            clip_id: "a".to_string(),
            empty: false,
            timed: true,
            written_slots: vec![
                (ArtifactKind::Lrc, "body-hash".to_string()),
                (ArtifactKind::LyricsTxt, "txt-hash".to_string()),
            ],
            timing: Some(suno_core::LyricsTiming::Line),
            timed_embed_hash: None,
        }];
        record_synced_lyrics_checks(&mut manifest, &pending);

        let check = manifest
            .get("a")
            .unwrap()
            .synced_lyrics
            .clone()
            .expect("a verified slot is durable");
        assert!(check.timed);
        assert!(!check.empty);
    }

    #[test]
    fn embed_only_empty_probe_records_a_negative_marker() {
        let mut manifest = Manifest::new();
        manifest.insert("a", ManifestEntry::default());
        let pending = vec![PendingCheck {
            clip_id: "a".to_string(),
            empty: true,
            timed: false,
            written_slots: Vec::new(),
            timing: None,
            timed_embed_hash: None,
        }];

        record_synced_lyrics_checks(&mut manifest, &pending);

        let check = manifest.get("a").unwrap().synced_lyrics.as_ref().unwrap();
        assert!(check.empty);
        assert!(!check.timed);
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
            timing: None,
            timed_embed_hash: None,
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
            timing: Some(suno_core::LyricsTiming::Line),
            timed_embed_hash: None,
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
            timing: Some(suno_core::LyricsTiming::Line),
            timed_embed_hash: None,
        }];
        record_synced_lyrics_checks(&mut manifest, &pending);
        assert!(
            manifest.get("a").unwrap().synced_lyrics.is_some(),
            "both slots landed -> resolved (converges)"
        );
    }

    #[test]
    fn timed_mode_waits_for_the_audio_embed_to_land() {
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            ManifestEntry {
                lrc: Some(ArtifactState {
                    path: "a.lrc".to_owned(),
                    hash: "lrc-hash".to_owned(),
                }),
                embedded_timed_lyrics_hash: "old".to_owned(),
                ..Default::default()
            },
        );
        let pending = vec![PendingCheck {
            clip_id: "a".to_owned(),
            empty: false,
            timed: true,
            written_slots: vec![(ArtifactKind::Lrc, "lrc-hash".to_owned())],
            timing: Some(suno_core::LyricsTiming::Line),
            timed_embed_hash: Some("new".to_owned()),
        }];

        record_synced_lyrics_checks(&mut manifest, &pending);
        assert!(manifest.get("a").unwrap().synced_lyrics.is_none());

        manifest
            .entries
            .get_mut("a")
            .unwrap()
            .embedded_timed_lyrics_hash = "new".to_owned();
        record_synced_lyrics_checks(&mut manifest, &pending);
        let state = manifest.get("a").unwrap().synced_lyrics.as_ref().unwrap();
        assert_eq!(state.timing, Some(suno_core::LyricsTiming::Line));
        assert_eq!(state.timed_version, suno_core::TIMED_LYRICS_VERSION);
    }

    #[test]
    fn plain_sidecar_result_does_not_erase_prior_timed_state() {
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            ManifestEntry {
                lyrics_txt: Some(ArtifactState {
                    path: "a.lyrics.txt".to_owned(),
                    hash: "txt-hash".to_owned(),
                }),
                synced_lyrics: Some(suno_core::SyncedLyricsCheck {
                    version: suno_core::SYNCED_LRC_VERSION,
                    checked_unix: 1,
                    empty: false,
                    timed: true,
                    timed_version: 0,
                    timing: None,
                }),
                ..Default::default()
            },
        );
        let pending = vec![PendingCheck {
            clip_id: "a".to_owned(),
            empty: false,
            timed: false,
            written_slots: vec![(ArtifactKind::LyricsTxt, "txt-hash".to_owned())],
            timing: None,
            timed_embed_hash: None,
        }];

        record_synced_lyrics_checks(&mut manifest, &pending);

        let state = manifest.get("a").unwrap().synced_lyrics.as_ref().unwrap();
        assert!(state.timed);
        assert!(!state.empty);
        assert_eq!(state.timed_version, 0);
        assert_eq!(state.timing, None);
    }
}
