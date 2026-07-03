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
//! manifest — except that a clip that resolved to no lyrics (an instrumental) is
//! re-checked after [`SYNCED_LRC_RECHECK_SECS`] to pick up alignment Suno may
//! compute after generation, and a clip whose audio is renamed is re-fetched so
//! its `.lrc` moves with it. A version bump re-resolves everything.

use std::collections::{BTreeSet, HashMap};

use crate::extras::{render_clip_lrc, render_synced_lrc};
use crate::hash::{SYNCED_LRC_VERSION, content_hash, synced_lrc_source_hash};
use crate::lyrics::AlignedLyrics;
use crate::manifest::{Manifest, ManifestEntry};
use crate::reconcile::{ArtifactKind, Desired};

/// How long a clip that resolved to no lyrics is trusted before its alignment is
/// re-checked (14 days). Bounds the re-fetch of instrumentals to catch alignment
/// Suno may compute shortly after a clip is generated.
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
    match &entry.synced_lyrics {
        // Never resolved (e.g. a clip downloaded before the feature existed).
        None => true,
        Some(check) => {
            if check.version != SYNCED_LRC_VERSION {
                return true; // the render changed -> re-resolve and re-render
            }
            if check.empty {
                // An instrumental: re-check only once the window elapses.
                now_unix.saturating_sub(check.checked_unix) > SYNCED_LRC_RECHECK_SECS
            } else {
                // Written: re-fetch only to move the `.lrc` when the audio is
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
        if needs_fetch(manifest.get(&d.clip.id), path, now_unix) {
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
/// actually changes (including an untimed→timed upgrade after a re-check).
pub fn apply_synced_lrc(
    desired: &mut [Desired],
    manifest: &Manifest,
    successes: &HashMap<String, AlignedLyrics>,
) -> Vec<PendingCheck> {
    let mut pending = Vec::new();
    for d in desired.iter_mut() {
        let Some(idx) = d.artifacts.iter().position(|a| a.kind == ArtifactKind::Lrc) else {
            continue;
        };
        let clip_id = d.clip.id.clone();
        let slot_hash = manifest
            .get(&clip_id)
            .and_then(|e| e.lrc.as_ref())
            .map(|slot| slot.hash.clone());

        if let Some(aligned) = successes.get(&clip_id) {
            let body = if aligned.is_empty() {
                render_clip_lrc(&d.clip, &d.lineage)
            } else {
                render_synced_lrc(&d.clip, &d.lineage, aligned)
            };
            match body {
                Some(text) => {
                    let hash = content_hash(&text);
                    let artifact = &mut d.artifacts[idx];
                    artifact.hash = hash.clone();
                    artifact.content = Some(text);
                    pending.push(PendingCheck {
                        clip_id,
                        empty: false,
                        body_hash: Some(hash),
                    });
                }
                None => {
                    d.artifacts.remove(idx);
                    pending.push(PendingCheck {
                        clip_id,
                        empty: true,
                        body_hash: None,
                    });
                }
            }
        } else {
            // Not fetched this run (resolved recently) or the fetch failed: keep
            // whatever is already on disk. Reuse the stored slot hash so reconcile
            // skips the write; drop the artifact when nothing was ever written.
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
        let Some(idx) = d.artifacts.iter().position(|a| a.kind == ArtifactKind::Lrc) else {
            continue;
        };
        if targets.contains(&d.clip.id) {
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
    use crate::config::AudioFormat;
    use crate::lineage::LineageContext;
    use crate::manifest::{ArtifactState, SyncedLyricsCheck};
    use crate::model::Clip;
    use crate::reconcile::DesiredArtifact;

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
            modes: vec![crate::reconcile::SourceMode::Mirror],
            trashed: false,
            private: false,
            artifacts: vec![lrc_artifact(id)],
            clip: c,
            stems: None,
        }
    }

    fn one_line_alignment() -> AlignedLyrics {
        AlignedLyrics::from_json(&serde_json::json!({
            "aligned_words": [],
            "aligned_lyrics": [
                {"text": "hi there", "start_s": 0.5, "end_s": 1.2, "section": "Verse 1",
                 "words": [
                     {"text": "hi", "start_s": 0.5, "end_s": 0.8},
                     {"text": "there", "start_s": 0.9, "end_s": 1.2}
                 ]}
            ]
        }))
    }

    fn entry(lrc: Option<ArtifactState>, check: Option<SyncedLyricsCheck>) -> ManifestEntry {
        ManifestEntry {
            path: "song.flac".to_string(),
            format: AudioFormat::Flac,
            lrc,
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
        // `done` was resolved (written) at the current version; `new` is unseen.
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
                body_hash: Some(content_hash(body)),
            }]
        );
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
        // The clip previously wrote an untimed body (stored slot hash); a later
        // fetch returns alignment, so the timed body's content hash differs and
        // reconcile will rewrite (the artifact carries the new content).
        let mut d = vec![desired("a", "")];
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
                    empty: true,
                }),
            ),
        );
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        apply_synced_lrc(&mut d, &manifest, &successes);
        let art = &d[0].artifacts[0];
        assert!(
            art.content
                .as_deref()
                .unwrap()
                .contains("[00:00.50]hi there")
        );
        assert_ne!(art.hash, untimed_hash, "a changed body triggers a rewrite");
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
                }),
            ),
        );
        preview_synced_lrc(&mut d, &manifest, 2_000, true);
        // `new` keeps a pending hash (would write); `done` reuses its slot hash.
        assert_eq!(d[0].artifacts[0].hash, synced_lrc_source_hash("new"));
        assert_eq!(d[1].artifacts[0].hash, "slot-hash");
    }
}
