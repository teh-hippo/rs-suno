//! Pure synced-lyrics resolution: which clips to fetch alignment for, and how
//! each fetched result maps onto a clip's desired `.lrc` artifact.
//!
//! The alignment fetch itself is IO and lives in the CLI (through the `Http`
//! port); everything here is pure so the fetch-gating, the timed/untimed body
//! choice, the "keep existing on failure" rule, and the instrumental "checked"
//! marker are unit-tested without a network.
//!
//! Suno's forced alignment for a clip is immutable in practice (the audio and
//! its lyrics are fixed once generated), so a clip is fetched at most once per
//! render [`SYNCED_LRC_VERSION`] — recorded by [`SyncedLyricsCheck`] on the
//! manifest — except that a clip that resolved to no lyrics (an instrumental) or
//! to an untimed plain-text fallback is re-checked after [`SYNCED_LRC_RECHECK_SECS`]
//! to pick up alignment Suno may compute after generation, and a clip whose audio
//! is renamed is re-fetched so its `.lrc` moves with it. A version bump
//! re-resolves everything.

use std::collections::{BTreeSet, HashMap};

use crate::hash::{SYNCED_LRC_VERSION, content_hash, synced_lrc_source_hash};
use crate::lyrics::{AlignedLyrics, render_clip_lrc, render_synced_lrc};
use crate::manifest::{Manifest, ManifestEntry};
use crate::reconcile::Desired;
use crate::vocab::ArtifactKind;

/// How long a clip that resolved to no lyrics (instrumental) or to an untimed
/// plain-text fallback is trusted before its alignment is re-checked (14 days).
/// Bounds the re-fetch to catch alignment Suno may compute after generation.
pub const SYNCED_LRC_RECHECK_SECS: u64 = 14 * 24 * 60 * 60;

/// One clip's synced-lyrics outcome this run, for the caller to record as a
/// manifest [`SyncedLyricsCheck`](crate::SyncedLyricsCheck) once the `.lrc` write
/// (if any) has safely landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCheck {
    /// The clip this outcome concerns.
    pub clip_id: String,
    /// Whether the clip resolved to no lyrics (an instrumental).
    pub empty: bool,
    /// Whether the written `.lrc` body carries timed alignment (as opposed to
    /// an untimed plain-text fallback). Only meaningful when `empty` is false.
    pub timed: bool,
    /// The content hash of the rendered `.lrc` body, when one was produced. The
    /// caller records the marker only once the manifest slot reflects this hash,
    /// so an interrupted or failed write is re-resolved next run.
    pub body_hash: Option<String>,
}

/// The relative `.lrc` path a clip's desired artifact targets, if it has one.
fn desired_lrc(desired: &Desired) -> Option<&str> {
    desired
        .artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::Lrc)
        .map(|a| a.path.as_str())
}

/// Whether a clip's alignment must be (re)fetched this run.
fn needs_fetch(entry: Option<&ManifestEntry>, desired_lrc_path: &str, now_unix: u64) -> bool {
    let Some(entry) = entry else {
        return true; // never downloaded -> resolve on first sight
    };
    // One-time back-fill (#354): a persisted `.lrc` exists but the audio tag's
    // embed is missing or stale relative to it. Fetching re-embeds; once the
    // embed stamps `embedded_lyrics_hash = lrc.hash` this is false again, so the
    // back-fill is bounded to one run per drift. A clip with no `.lrc` slot (an
    // instrumental, or the feature off) has both sides empty and is never a
    // target for this reason.
    if let Some(slot) = entry.lrc.as_ref()
        && slot.hash != entry.embedded_lyrics_hash
    {
        return true;
    }
    match &entry.synced_lyrics {
        // Never resolved (e.g. a clip downloaded before the feature existed).
        None => true,
        Some(check) => {
            if check.version != SYNCED_LRC_VERSION {
                return true; // the render changed -> re-resolve and re-render
            }
            if check.empty || !check.timed {
                // Instrumental (no `.lrc`) or untimed fallback: re-check once
                // the window elapses, to pick up alignment Suno adds later.
                now_unix.saturating_sub(check.checked_unix) > SYNCED_LRC_RECHECK_SECS
            } else {
                // Timed: re-fetch only to move the `.lrc` when the audio is
                // renamed (its `.lrc` path drifts), or if the slot is somehow
                // missing (an interrupted prior write).
                entry
                    .lrc
                    .as_ref()
                    .map(|slot| slot.path != desired_lrc_path)
                    .unwrap_or(true)
            }
        }
    }
}

/// The clip ids whose alignment must be fetched this run, in a stable order.
///
/// Empty when `enabled` is false, so the synced-lyrics feature being off means
/// zero alignment fetches. Only clips carrying a desired `.lrc` artifact (a
/// lyric signal) are considered; each is fetched at most once per render version
/// (see [`needs_fetch`]).
pub fn synced_lyrics_targets(
    desired: &[Desired],
    manifest: &Manifest,
    now_unix: u64,
    enabled: bool,
) -> BTreeSet<String> {
    if !enabled {
        return BTreeSet::new();
    }
    let mut out = BTreeSet::new();
    for d in desired {
        let Some(path) = desired_lrc(d) else {
            continue;
        };
        let entry = manifest.get(&d.clip.id);
        // Reformat re-embed (#354): a format change re-encodes the audio and
        // drops any embedded lyrics, so re-fetch when the format will change AND
        // a persisted `.lrc` is worth re-embedding (an already-migrated clip is
        // neither a back-fill nor a rename target, so nothing else would refetch
        // it). Pure and bounded: after the Reformat commits `entry.format ==
        // d.format`, so it fires once. A clip with no `.lrc` never fetches here.
        let reformat_reembed = entry.is_some_and(|e| e.format != d.format && e.lrc.is_some());
        if reformat_reembed || needs_fetch(entry, path, now_unix) {
            out.insert(d.clip.id.clone());
        }
    }
    out
}

/// Resolve each clip's desired `.lrc` artifact from the fetched alignment,
/// returning the checks to persist for the clips that were successfully fetched.
///
/// `successes` holds the alignment for clips whose fetch returned `200` (an empty
/// value for an instrumental); a clip absent from it either was not fetched
/// (resolved recently) or its fetch FAILED. In both of those cases the existing
/// `.lrc` is KEPT untouched — the artifact's hash is reset to the stored slot so
/// reconcile skips it (no rewrite, no downgrade of a timed file to untimed), or
/// the artifact is dropped when there is nothing on disk yet — and no check is
/// returned, so a failed fetch is simply retried next run.
///
/// For a successful fetch the body is the timed render when Suno has alignment,
/// else the untimed lyrics as a fallback; an instrumental (no body) drops the
/// artifact and records an empty check. A produced body sets the artifact's
/// content and its content hash, so reconcile rewrites only when the body
/// actually changes (including an untimed→timed upgrade after a re-check). The
/// `timed` flag in the returned check distinguishes timed alignment from an
/// untimed fallback: only timed clips are exempt from the periodic re-check.
pub fn apply_synced_lrc(
    desired: &mut [Desired],
    manifest: &Manifest,
    successes: &HashMap<String, AlignedLyrics>,
) -> Vec<PendingCheck> {
    let mut pending = Vec::new();
    for d in desired.iter_mut() {
        // Carry-forward baseline (#354, the loop-freedom crux): every clip keeps
        // its persisted embed fingerprint unless it is actually fetched this run
        // (the overrides below). So a lyrics-driven `Retag` can only fire when
        // alignment was fetched, never stamping a matching hash over an empty
        // write. This assignment MUST stay ABOVE the `.lrc`-artifact `continue`:
        // a clip with no desired `.lrc` (the sidecar off, or an instrumental)
        // must still carry its value forward, otherwise its sentinel would drift
        // to the default and it would spuriously retag with an empty `synced` map.
        d.embedded_lyrics_hash = manifest
            .get(&d.clip.id)
            .map(|e| e.embedded_lyrics_hash.clone())
            .unwrap_or_default();

        let Some(idx) = d.artifacts.iter().position(|a| a.kind == ArtifactKind::Lrc) else {
            continue;
        };
        let clip_id = d.clip.id.clone();
        let slot_hash = manifest
            .get(&clip_id)
            .and_then(|e| e.lrc.as_ref())
            .map(|slot| slot.hash.clone());

        if let Some(aligned) = successes.get(&clip_id) {
            let timed = !aligned.is_empty();
            let body = if timed {
                render_synced_lrc(&d.clip, &d.lineage, aligned)
            } else {
                render_clip_lrc(&d.clip, &d.lineage)
            };
            match body {
                Some(text) => {
                    let hash = content_hash(&text);
                    // The embed is rendered from this same fetched alignment, so
                    // its fingerprint moves in lock-step with the `.lrc` body.
                    d.embedded_lyrics_hash = hash.clone();
                    let artifact = &mut d.artifacts[idx];
                    artifact.hash = hash.clone();
                    artifact.content = Some(text);
                    pending.push(PendingCheck {
                        clip_id,
                        empty: false,
                        timed,
                        body_hash: Some(hash),
                    });
                }
                None => {
                    // Fetched but the clip is an instrumental: nothing embedded.
                    d.embedded_lyrics_hash = String::new();
                    d.artifacts.remove(idx);
                    pending.push(PendingCheck {
                        clip_id,
                        empty: true,
                        timed: false,
                        body_hash: None,
                    });
                }
            }
        } else {
            // Not fetched this run (resolved recently) or the fetch failed: keep
            // whatever is already on disk. Reuse the stored slot hash so reconcile
            // skips the write; drop the artifact when nothing was ever written.
            // `embedded_lyrics_hash` keeps its carry-forward baseline, so a failed
            // back-fill neither retags nor stamps and is simply retried next run.
            match slot_hash {
                Some(hash) => {
                    let artifact = &mut d.artifacts[idx];
                    artifact.hash = hash;
                    artifact.content = None;
                }
                None => {
                    d.artifacts.remove(idx);
                }
            }
        }
    }
    pending
}

/// Adjust each clip's desired `.lrc` artifact for a dry run, without any fetch.
///
/// A clip that WOULD be fetched (a target) keeps a distinct pending hash so the
/// previewed plan reports its `.lrc` write; a clip already resolved reuses its
/// stored slot hash (so it shows as skipped) or drops the artifact when it is a
/// known instrumental. The preview is therefore an upper bound on synced `.lrc`
/// writes (it cannot know which targets will turn out to be instrumentals).
pub fn preview_synced_lrc(
    desired: &mut [Desired],
    manifest: &Manifest,
    now_unix: u64,
    enabled: bool,
) {
    let targets = synced_lyrics_targets(desired, manifest, now_unix, enabled);
    for d in desired.iter_mut() {
        // Carry-forward baseline (#354): mirror `apply_synced_lrc`. This MUST stay
        // ABOVE the `.lrc`-artifact `continue` so a non-target (and any clip with
        // no desired `.lrc`: the sidecar off, or an instrumental) keeps its
        // persisted embed fingerprint and previews as skipped, rather than
        // drifting to the default and reporting a spurious retag.
        d.embedded_lyrics_hash = manifest
            .get(&d.clip.id)
            .map(|e| e.embedded_lyrics_hash.clone())
            .unwrap_or_default();

        let Some(idx) = d.artifacts.iter().position(|a| a.kind == ArtifactKind::Lrc) else {
            continue;
        };
        if targets.contains(&d.clip.id) {
            // A fetch target previews the expected post-fetch embed: the persisted
            // `.lrc` slot hash, so a pending back-fill reports drift (not "up to
            // date"). It converges to the apply value once alignment is fetched.
            d.embedded_lyrics_hash = manifest
                .get(&d.clip.id)
                .and_then(|e| e.lrc.as_ref())
                .map(|s| s.hash.clone())
                .unwrap_or_default();
            d.artifacts[idx].hash = synced_lrc_source_hash(&d.clip.id);
            continue;
        }
        match manifest.get(&d.clip.id).and_then(|e| e.lrc.as_ref()) {
            Some(slot) => d.artifacts[idx].hash = slot.hash.clone(),
            None => {
                d.artifacts.remove(idx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lineage::LineageContext;
    use crate::lyrics::{AlignedLine, AlignedLineWord};
    use crate::manifest::{ArtifactState, SyncedLyricsCheck};
    use crate::model::Clip;
    use crate::reconcile::DesiredArtifact;
    use crate::vocab::AudioFormat;

    fn clip(id: &str, lyrics: &str) -> Clip {
        Clip {
            id: id.to_string(),
            title: "Song".to_string(),
            lyrics: lyrics.to_string(),
            prompt: "a prompt".to_string(),
            ..Default::default()
        }
    }

    fn lrc_artifact(clip_id: &str) -> DesiredArtifact {
        DesiredArtifact {
            kind: ArtifactKind::Lrc,
            path: format!("{clip_id}.lrc"),
            source_url: String::new(),
            hash: synced_lrc_source_hash(clip_id),
            content: None,
        }
    }

    fn desired(id: &str, lyrics: &str) -> Desired {
        let c = clip(id, lyrics);
        Desired {
            lineage: LineageContext::own_root(&c),
            path: format!("{id}.flac"),
            format: AudioFormat::Flac,
            meta_hash: "m".to_string(),
            art_hash: "a".to_string(),
            embedded_lyrics_hash: String::new(),
            modes: vec![crate::vocab::SourceMode::Mirror],
            trashed: false,
            private: false,
            artifacts: vec![lrc_artifact(id)],
            clip: c,
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
                words: vec![
                    AlignedLineWord {
                        text: "hi".to_owned(),
                        start_s: 0.5,
                        end_s: 0.8,
                    },
                    AlignedLineWord {
                        text: "there".to_owned(),
                        start_s: 0.9,
                        end_s: 1.2,
                    },
                ],
            }],
            ..Default::default()
        }
    }

    fn entry(lrc: Option<ArtifactState>, check: Option<SyncedLyricsCheck>) -> ManifestEntry {
        // Default to a fully-migrated clip: the embed fingerprint matches the
        // `.lrc` slot hash, so an ordinarily-resolved clip is NOT a #354 back-fill
        // target. Back-fill/instrumental tests override `embedded_lyrics_hash`.
        let embedded_lyrics_hash = lrc.as_ref().map(|s| s.hash.clone()).unwrap_or_default();
        ManifestEntry {
            path: "song.flac".to_string(),
            format: AudioFormat::Flac,
            lrc,
            embedded_lyrics_hash,
            synced_lyrics: check,
            ..Default::default()
        }
    }

    #[test]
    fn targets_empty_when_feature_off() {
        let d = vec![desired("a", "")];
        let manifest = Manifest::new();
        assert!(synced_lyrics_targets(&d, &manifest, 0, false).is_empty());
    }

    #[test]
    fn targets_new_clip_but_not_a_recently_resolved_one() {
        let d = vec![desired("new", ""), desired("done", "")];
        let mut manifest = Manifest::new();
        // `done` was timed-resolved at the current version; `new` is unseen.
        manifest.insert(
            "done",
            entry(
                Some(ArtifactState {
                    path: "done.lrc".to_string(),
                    hash: "h".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: true,
                }),
            ),
        );
        let targets = synced_lyrics_targets(&d, &manifest, 2_000, true);
        assert!(targets.contains("new"));
        assert!(!targets.contains("done"));
    }

    #[test]
    fn instrumental_is_rechecked_only_after_the_window() {
        let d = vec![desired("instr", "")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "instr",
            entry(
                None,
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: true,
                    timed: false,
                }),
            ),
        );
        // Within the window: not re-fetched (this is the fix for forever-refetch).
        let soon = 1_000 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, soon, true).is_empty());
        // Past the window: re-checked, to pick up late alignment.
        let later = 1_001 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, later, true).contains("instr"));
    }

    #[test]
    fn untimed_fallback_is_rechecked_after_the_window() {
        // A clip that previously resolved to an untimed fallback (empty alignment
        // but non-empty lyrics) must be re-checked after the window so a later
        // Suno alignment upgrades it to a timed `.lrc`.
        let d = vec![desired("a", "some lyrics")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry(
                Some(ArtifactState {
                    path: "a.lrc".to_string(),
                    hash: "untimed-hash".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: false,
                }),
            ),
        );
        // Within the window: no re-fetch (avoids churn on every run).
        let soon = 1_000 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, soon, true).is_empty());
        // Past the window: re-checked, to upgrade to timed if alignment arrived.
        let later = 1_001 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, later, true).contains("a"));
    }

    #[test]
    fn timed_clip_is_not_rechecked_without_rename() {
        // A timed clip must not be re-fetched just because the window elapsed;
        // only a rename (path drift) or missing slot should trigger a re-fetch.
        let d = vec![desired("a", "")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry(
                Some(ArtifactState {
                    path: "a.lrc".to_string(),
                    hash: "h".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 0, // maximally stale
                    empty: false,
                    timed: true,
                }),
            ),
        );
        // Even long after the window: still not re-fetched.
        let very_late = 2 * SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, very_late, true).is_empty());
    }

    #[test]
    fn version_bump_refetches_everything() {
        let d = vec![desired("done", "")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "done",
            entry(
                Some(ArtifactState {
                    path: "done.lrc".to_string(),
                    hash: "h".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION + 1, // resolved at a different version
                    checked_unix: 1_000,
                    empty: false,
                    timed: true,
                }),
            ),
        );
        assert!(synced_lyrics_targets(&d, &manifest, 2_000, true).contains("done"));
    }

    #[test]
    fn rename_refetches_a_written_clip() {
        let mut d = vec![desired("a", "")];
        // The audio (and so the `.lrc`) moved to a new path.
        d[0].artifacts[0].path = "new/a.lrc".to_string();
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry(
                Some(ArtifactState {
                    path: "old/a.lrc".to_string(),
                    hash: "h".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: true,
                }),
            ),
        );
        assert!(synced_lyrics_targets(&d, &manifest, 2_000, true).contains("a"));
    }

    #[test]
    fn apply_sets_timed_body_and_content_hash() {
        let mut d = vec![desired("a", "")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        let art = &d[0].artifacts[0];
        let body = art.content.as_deref().unwrap();
        assert!(body.contains("[00:00.50]hi there"));
        assert_eq!(art.hash, content_hash(body));
        assert_eq!(
            pending,
            vec![PendingCheck {
                clip_id: "a".to_string(),
                empty: false,
                timed: true,
                body_hash: Some(content_hash(body)),
            }]
        );
    }

    #[test]
    fn apply_untimed_fallback_marks_not_timed() {
        // When Suno returns empty alignment but the clip has lyrics, the untimed
        // plain-text fallback is written but `timed` is false so the check is
        // subject to the periodic re-check window.
        let mut d = vec![desired("a", "some lyrics")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), AlignedLyrics::default());
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        let art = &d[0].artifacts[0];
        assert!(art.content.is_some(), "untimed body written");
        let check = &pending[0];
        assert!(!check.empty, "clip has lyrics, not an instrumental");
        assert!(!check.timed, "alignment was empty -> untimed fallback");
    }

    #[test]
    fn apply_drops_instrumental_and_marks_empty() {
        let mut d = vec![desired("instr", "")];
        let mut successes = HashMap::new();
        successes.insert("instr".to_string(), AlignedLyrics::default());
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        assert!(d[0].artifacts.iter().all(|a| a.kind != ArtifactKind::Lrc));
        assert_eq!(
            pending,
            vec![PendingCheck {
                clip_id: "instr".to_string(),
                empty: true,
                timed: false,
                body_hash: None,
            }]
        );
    }

    #[test]
    fn apply_keeps_existing_on_fetch_failure_no_downgrade() {
        // The clip has an existing timed `.lrc` (slot present) but its fetch
        // failed this run (absent from successes). The artifact is reset to the
        // stored slot hash with no content, so reconcile skips it — the good
        // timed file is neither rewritten nor downgraded — and no check is
        // recorded, so it is retried next run.
        let mut d = vec![desired("a", "")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry(
                Some(ArtifactState {
                    path: "a.lrc".to_string(),
                    hash: "timed-hash".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: true,
                }),
            ),
        );
        let pending = apply_synced_lrc(&mut d, &manifest, &HashMap::new());

        let art = &d[0].artifacts[0];
        assert_eq!(art.hash, "timed-hash");
        assert_eq!(art.content, None);
        assert!(
            pending.is_empty(),
            "no check recorded on failure -> retried"
        );
    }

    #[test]
    fn apply_drops_write_on_failure_when_nothing_on_disk() {
        // A brand-new clip whose fetch failed: no slot to keep, so the write is
        // dropped (retried next run) rather than written empty.
        let mut d = vec![desired("a", "")];
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &HashMap::new());
        assert!(d[0].artifacts.iter().all(|a| a.kind != ArtifactKind::Lrc));
        assert!(pending.is_empty());
    }

    #[test]
    fn apply_upgrades_untimed_to_timed_when_alignment_appears() {
        // The clip previously resolved to an untimed fallback (empty alignment,
        // body written, timed: false). A re-check now returns alignment, so the
        // timed body's content hash differs and reconcile will rewrite.
        let mut d = vec![desired("a", "some lyrics")];
        let untimed_hash = "untimed".to_string();
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry(
                Some(ArtifactState {
                    path: "a.lrc".to_string(),
                    hash: untimed_hash.clone(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: false,
                }),
            ),
        );
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        let pending = apply_synced_lrc(&mut d, &manifest, &successes);
        let art = &d[0].artifacts[0];
        assert!(
            art.content
                .as_deref()
                .unwrap()
                .contains("[00:00.50]hi there")
        );
        assert_ne!(art.hash, untimed_hash, "a changed body triggers a rewrite");
        assert!(pending[0].timed, "upgraded to timed");
    }

    #[test]
    fn preview_shows_write_for_targets_and_skips_resolved() {
        let mut d = vec![desired("new", ""), desired("done", "")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "done",
            entry(
                Some(ArtifactState {
                    path: "done.lrc".to_string(),
                    hash: "slot-hash".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: true,
                }),
            ),
        );
        preview_synced_lrc(&mut d, &manifest, 2_000, true);
        // `new` keeps a pending hash (would write); `done` reuses its slot hash.
        assert_eq!(d[0].artifacts[0].hash, synced_lrc_source_hash("new"));
        assert_eq!(d[1].artifacts[0].hash, "slot-hash");
    }

    // ---- #354: embedded aligned-lyrics back-fill (embedded_lyrics_hash) ----

    /// A timed, resolved marker at the current version (the stable-clip baseline).
    fn timed_check() -> SyncedLyricsCheck {
        SyncedLyricsCheck {
            version: SYNCED_LRC_VERSION,
            checked_unix: 1_000,
            empty: false,
            timed: true,
        }
    }

    fn slot(path: &str, hash: &str) -> Option<ArtifactState> {
        Some(ArtifactState {
            path: path.to_string(),
            hash: hash.to_string(),
        })
    }

    #[test]
    fn needs_fetch_backfills_when_embed_missing() {
        // A timed, resolved clip whose `.lrc` slot has a hash but whose persisted
        // embed is empty is a one-time back-fill target.
        let mut e = entry(slot("a.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = String::new();
        assert!(needs_fetch(Some(&e), "a.lrc", 2_000));
    }

    #[test]
    fn needs_fetch_skips_when_embed_matches_lrc() {
        // Already back-filled: the embed matches the `.lrc` slot, so no re-fetch.
        let mut e = entry(slot("a.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = "H".to_string();
        assert!(!needs_fetch(Some(&e), "a.lrc", 2_000));
    }

    #[test]
    fn needs_fetch_no_backfill_without_lrc_slot() {
        // Instrumental: no `.lrc` slot and an empty embed, so the back-fill clause
        // is skipped (both sides empty) and, within the re-check window, the
        // instrumental clause is false too.
        let mut e = entry(
            None,
            Some(SyncedLyricsCheck {
                empty: true,
                timed: false,
                ..timed_check()
            }),
        );
        e.embedded_lyrics_hash = String::new();
        let within_window = 1_000 + SYNCED_LRC_RECHECK_SECS;
        assert!(!needs_fetch(Some(&e), "a.lrc", within_window));
    }

    #[test]
    fn apply_sets_embedded_lyrics_hash_from_body() {
        // A timed fetch stamps the sentinel with the `.lrc` body content hash,
        // equal to the resolved `.lrc` artifact hash (lock-step with the embed).
        let mut d = vec![desired("a", "")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        let art = &d[0].artifacts[0];
        let body = art.content.as_deref().unwrap();
        assert_eq!(d[0].embedded_lyrics_hash, content_hash(body));
        assert_eq!(d[0].embedded_lyrics_hash, art.hash);
    }

    #[test]
    fn apply_clears_embedded_lyrics_hash_for_instrumental() {
        // A previously-embedded clip that resolves to no lyrics this run drops the
        // artifact and clears the sentinel to "" (nothing embedded now).
        let mut d = vec![desired("instr", "")];
        let mut manifest = Manifest::new();
        let mut e = entry(slot("instr.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = "H".to_string();
        manifest.insert("instr", e);
        let mut successes = HashMap::new();
        successes.insert("instr".to_string(), AlignedLyrics::default());
        apply_synced_lrc(&mut d, &manifest, &successes);

        assert!(d[0].artifacts.iter().all(|a| a.kind != ArtifactKind::Lrc));
        assert_eq!(d[0].embedded_lyrics_hash, "");
    }

    #[test]
    fn apply_carries_forward_embedded_lyrics_hash_when_not_fetched() {
        // No fetch this run: the sentinel carries the PERSISTED embed value, not
        // the `.lrc` slot hash. They differ in the not-yet-embedded / failed
        // cases, which is exactly why the field is required (loop-freedom).
        let mut d = vec![desired("a", "")];
        let mut manifest = Manifest::new();
        let mut e = entry(slot("a.lrc", "slot"), Some(timed_check()));
        e.embedded_lyrics_hash = "embed".to_string();
        manifest.insert("a", e);
        apply_synced_lrc(&mut d, &manifest, &HashMap::new());

        assert_eq!(
            d[0].embedded_lyrics_hash, "embed",
            "carry-forward, not the slot hash"
        );
        // The `.lrc` artifact hash resets to the slot so reconcile skips the write.
        assert_eq!(d[0].artifacts[0].hash, "slot");
    }

    #[test]
    fn apply_carries_forward_for_clip_without_lrc_artifact() {
        // A clip with no desired `.lrc` artifact (feature off / instrumental) keeps
        // its persisted embed value and never spuriously retags.
        let mut d = vec![desired("a", "")];
        d[0].artifacts.clear();
        let mut manifest = Manifest::new();
        let mut e = entry(None, None);
        e.embedded_lyrics_hash = "H".to_string();
        manifest.insert("a", e);
        apply_synced_lrc(&mut d, &manifest, &HashMap::new());

        assert_eq!(d[0].embedded_lyrics_hash, "H");
    }

    #[test]
    fn preview_marks_backfill_target_as_pending() {
        // A stale-embed clip (lrc.hash = H, embed = "") previews the expected
        // post-fetch value H so `check` reports drift; a resolved clip carries its
        // value forward and matches the manifest (skipped).
        let mut d = vec![desired("stale", ""), desired("done", "")];
        let mut manifest = Manifest::new();
        let mut stale = entry(slot("stale.lrc", "H"), Some(timed_check()));
        stale.embedded_lyrics_hash = String::new();
        manifest.insert("stale", stale);
        let mut done = entry(slot("done.lrc", "D"), Some(timed_check()));
        done.embedded_lyrics_hash = "D".to_string();
        manifest.insert("done", done);

        preview_synced_lrc(&mut d, &manifest, 2_000, true);
        assert_eq!(
            d[0].embedded_lyrics_hash, "H",
            "target previews the back-fill"
        );
        assert_ne!(
            d[0].embedded_lyrics_hash, "",
            "differs from the manifest embed -> drift reported"
        );
        assert_eq!(
            d[1].embedded_lyrics_hash, "D",
            "resolved clip carries forward"
        );
    }

    #[test]
    fn reformat_makes_migrated_clip_a_target() {
        // An already-migrated clip (embed == lrc.hash == H, entry.format = FLAC) is
        // neither a back-fill nor a rename target, but a pending FLAC->MP3 reformat
        // re-encodes and drops the embed, so the reformat re-embed trigger fires.
        let mut manifest = Manifest::new();
        let mut e = entry(slot("a.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = "H".to_string(); // entry.format is FLAC (helper)
        manifest.insert("a", e);

        let mut reformat = vec![desired("a", "")];
        reformat[0].format = AudioFormat::Mp3;
        assert!(
            synced_lyrics_targets(&reformat, &manifest, 2_000, true).contains("a"),
            "a format change re-embeds a migrated clip"
        );

        // No format change: the same stable clip is not a target.
        let stable = vec![desired("a", "")]; // FLAC == entry.format
        assert!(
            synced_lyrics_targets(&stable, &manifest, 2_000, true).is_empty(),
            "no reformat, no back-fill -> no fetch"
        );

        // A clip with no persisted `.lrc` and a format change is not a target
        // (nothing to re-embed).
        let mut no_lrc = Manifest::new();
        no_lrc.insert(
            "a",
            entry(
                None,
                Some(SyncedLyricsCheck {
                    empty: true,
                    timed: false,
                    ..timed_check()
                }),
            ),
        );
        let mut reformat_no_lrc = vec![desired("a", "")];
        reformat_no_lrc[0].format = AudioFormat::Mp3;
        assert!(
            synced_lyrics_targets(&reformat_no_lrc, &no_lrc, 2_000, true).is_empty(),
            "no `.lrc` to re-embed -> not a target despite the reformat"
        );
    }
}
