//! The pure reconcile engine: it decides what to download, retag, rename,
//! reformat, and delete.
//!
//! This is the highest-risk module in the project. It is intentionally pure:
//! no IO, no clock, no network. The caller supplies every input (the prior
//! [`Manifest`], the desired selection, the on-disk probe for each manifest
//! path, and the per-source enumeration status) and [`reconcile`] returns a
//! [`Plan`] that the CLI executes later. The plan is itself the dry-run
//! recording, so there is never an `if dry_run` branch.
//!
//! Deletion safety is paramount. The guards encoded here are:
//!
//! - SYNC-8: a clip held by any `Copy` source is never deleted; copy and
//!   archive always win.
//! - SYNC-9: never delete on an empty, failed, partial, or truncated listing.
//!   Deletion of an absent clip is allowed only when every selected `Mirror`
//!   source was fully enumerated, and only when at least one mirror source was
//!   selected at all.
//! - SYNC-10: a manifest path that is missing or zero length on disk is treated
//!   as missing and re-downloaded, even when its hashes still match.
//! - SYNC-12: a clip trashed in Suno is removed from the source and its local
//!   file is deleted; a private clip is always kept.

use std::collections::BTreeSet;
use std::collections::HashMap;

use crate::config::AudioFormat;
use crate::manifest::{Manifest, ManifestEntry};
use crate::model::Clip;

/// How a selected source treats its clips: mirror with deletion, or additive copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceMode {
    /// Mirror the source, deleting local files that leave it (rclone `sync`).
    Mirror,
    /// Copy additively; never delete (rclone `copy`).
    Copy,
}

/// One desired clip in the current selection.
///
/// The caller has already deduped per account and resolved naming and format,
/// so each entry is the authoritative target state for one clip. `modes` lists
/// every selected source that currently holds the clip, so a clip can be held
/// by a `Mirror` and a `Copy` source at once.
#[derive(Debug, Clone, PartialEq)]
pub struct Desired {
    /// The clip itself, carried so actions can be executed without a re-fetch.
    pub clip: Clip,
    /// Resolved relative target path for the file.
    pub path: String,
    /// Resolved target format.
    pub format: AudioFormat,
    /// Hash of the clip's tag-bearing metadata.
    pub meta_hash: String,
    /// Hash of the clip's cover art.
    pub art_hash: String,
    /// Every selected source that currently holds this clip.
    pub modes: Vec<SourceMode>,
    /// True when the clip is trashed in Suno (removed from the source).
    pub trashed: bool,
    /// True when the clip is private; private clips are always kept.
    pub private: bool,
}

/// The caller's on-disk probe of one manifest path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalFile {
    /// Whether the file exists on disk.
    pub exists: bool,
    /// Size of the file in bytes (zero when absent).
    pub size: u64,
}

/// Per-source enumeration status for one selected source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceStatus {
    /// The source's mode.
    pub mode: SourceMode,
    /// Whether this source was completely and successfully enumerated.
    pub fully_enumerated: bool,
}

/// One executable step in a [`Plan`].
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Download the clip to `path` in `format` (new, missing, or zero length).
    Download {
        clip: Clip,
        path: String,
        format: AudioFormat,
    },
    /// Render the clip to `path` in `to`, replacing the prior `from` rendering.
    Reformat {
        clip: Clip,
        path: String,
        from: AudioFormat,
        to: AudioFormat,
    },
    /// Re-tag the existing file at `path` to match current metadata or art.
    Retag { clip: Clip, path: String },
    /// Move the file from one relative path to another.
    Rename { from: String, to: String },
    /// Delete the local file for a clip that has left every mirror source.
    Delete { path: String, clip_id: String },
    /// Take no action for a clip; recorded so the plan is a full account.
    Skip { clip_id: String },
}

/// The reconcile output: an ordered, deterministic list of actions.
///
/// The plan is the dry-run recording. The convenience counts let the CLI
/// summarise a run without re-walking the action list by hand.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// The actions, in stable order.
    pub actions: Vec<Action>,
}

impl Plan {
    /// Total number of actions.
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// True when there are no actions.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Number of [`Action::Download`] actions.
    pub fn downloads(&self) -> usize {
        self.count(|a| matches!(a, Action::Download { .. }))
    }

    /// Number of [`Action::Reformat`] actions.
    pub fn reformats(&self) -> usize {
        self.count(|a| matches!(a, Action::Reformat { .. }))
    }

    /// Number of [`Action::Retag`] actions.
    pub fn retags(&self) -> usize {
        self.count(|a| matches!(a, Action::Retag { .. }))
    }

    /// Number of [`Action::Rename`] actions.
    pub fn renames(&self) -> usize {
        self.count(|a| matches!(a, Action::Rename { .. }))
    }

    /// Number of [`Action::Delete`] actions.
    pub fn deletes(&self) -> usize {
        self.count(|a| matches!(a, Action::Delete { .. }))
    }

    /// Number of [`Action::Skip`] actions.
    pub fn skips(&self) -> usize {
        self.count(|a| matches!(a, Action::Skip { .. }))
    }

    fn count(&self, pred: impl Fn(&Action) -> bool) -> usize {
        self.actions.iter().filter(|a| pred(a)).count()
    }
}

/// Decide the plan for one reconcile run.
///
/// `local` maps a clip id to the probe of that clip's manifest path; entries
/// are expected for clips present in `manifest`. `sources` lists every selected
/// source with its enumeration status, which gates deletion of absent clips.
///
/// The output order is stable: desired clips are processed in clip-id order,
/// then absent manifest entries in clip-id order. No output depends on hash-map
/// iteration order.
pub fn reconcile(
    manifest: &Manifest,
    desired: &[Desired],
    local: &HashMap<String, LocalFile>,
    sources: &[SourceStatus],
) -> Plan {
    let mut actions: Vec<Action> = Vec::new();

    // Desired clips, processed in clip-id order for determinism.
    let mut ordered: Vec<&Desired> = desired.iter().collect();
    ordered.sort_by(|a, b| a.clip.id.cmp(&b.clip.id));

    let desired_ids: BTreeSet<&str> = ordered.iter().map(|d| d.clip.id.as_str()).collect();

    for d in &ordered {
        plan_desired(d, manifest, local, &mut actions);
    }

    // Absent manifest entries, processed in clip-id order (BTreeMap is sorted).
    let deletion_allowed = deletion_allowed(sources);
    for (clip_id, entry) in manifest.iter() {
        if desired_ids.contains(clip_id.as_str()) {
            continue;
        }
        if deletion_allowed {
            actions.push(Action::Delete {
                path: entry.path.clone(),
                clip_id: clip_id.clone(),
            });
        } else {
            // SYNC-9: absence is unreliable when any mirror listing was
            // incomplete, so keep the file rather than delete it.
            actions.push(Action::Skip {
                clip_id: clip_id.clone(),
            });
        }
    }

    Plan { actions }
}

/// Whether absent clips may be deleted this run.
///
/// SYNC-9: deletion requires at least one selected `Mirror` source and every
/// selected mirror source fully enumerated. With no mirror source there is no
/// authoritative listing to delete against, and copy-only runs are additive.
fn deletion_allowed(sources: &[SourceStatus]) -> bool {
    let mut saw_mirror = false;
    for status in sources {
        if status.mode == SourceMode::Mirror {
            saw_mirror = true;
            if !status.fully_enumerated {
                return false;
            }
        }
    }
    saw_mirror
}

/// Append the action(s) for one desired clip.
fn plan_desired(
    d: &Desired,
    manifest: &Manifest,
    local: &HashMap<String, LocalFile>,
    out: &mut Vec<Action>,
) {
    let clip_id = d.clip.id.as_str();
    let copy_held = d.modes.contains(&SourceMode::Copy);

    // SYNC-12 / SYNC-8: protection beats removal. A private clip is always
    // kept, and a copy-held clip is never deleted, so neither is ever turned
    // into a delete even when trashed.
    if d.private {
        out.push(Action::Skip {
            clip_id: clip_id.to_string(),
        });
        return;
    }

    if d.trashed && !copy_held {
        // Trashed in Suno means removed from the source: delete the local file
        // when we have one, otherwise there is nothing on disk to remove.
        match manifest.get(clip_id) {
            Some(entry) => out.push(Action::Delete {
                path: entry.path.clone(),
                clip_id: clip_id.to_string(),
            }),
            None => out.push(Action::Skip {
                clip_id: clip_id.to_string(),
            }),
        }
        return;
    }

    let Some(entry) = manifest.get(clip_id) else {
        // Not in the manifest: a fresh download.
        out.push(Action::Download {
            clip: d.clip.clone(),
            path: d.path.clone(),
            format: d.format,
        });
        return;
    };

    // SYNC-10: a missing or zero-length file is treated as missing and
    // re-downloaded, even when the hashes still match.
    let missing = local.get(clip_id).is_none_or(|f| !f.exists || f.size == 0);
    if missing {
        out.push(Action::Download {
            clip: d.clip.clone(),
            path: d.path.clone(),
            format: d.format,
        });
        return;
    }

    if d.format != entry.format {
        // Replace via re-encode; never pre-delete the existing file.
        out.push(Action::Reformat {
            clip: d.clip.clone(),
            path: d.path.clone(),
            from: entry.format,
            to: d.format,
        });
        return;
    }

    if d.path != entry.path {
        out.push(Action::Rename {
            from: entry.path.clone(),
            to: d.path.clone(),
        });
        // A rename still needs a retag when the metadata or art drifted.
        if meta_or_art_changed(d, entry) {
            out.push(Action::Retag {
                clip: d.clip.clone(),
                path: d.path.clone(),
            });
        }
        return;
    }

    if meta_or_art_changed(d, entry) {
        out.push(Action::Retag {
            clip: d.clip.clone(),
            path: entry.path.clone(),
        });
        return;
    }

    out.push(Action::Skip {
        clip_id: clip_id.to_string(),
    });
}

/// Whether the desired metadata or art hash differs from the manifest entry.
fn meta_or_art_changed(d: &Desired, entry: &ManifestEntry) -> bool {
    d.meta_hash != entry.meta_hash || d.art_hash != entry.art_hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(id: &str) -> Clip {
        Clip {
            id: id.to_string(),
            title: "Song".to_string(),
            ..Default::default()
        }
    }

    fn entry(path: &str, format: AudioFormat, meta: &str, art: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            format,
            meta_hash: meta.to_string(),
            art_hash: art.to_string(),
            size: 100,
        }
    }

    fn desired(id: &str, path: &str, format: AudioFormat, meta: &str, art: &str) -> Desired {
        Desired {
            clip: clip(id),
            path: path.to_string(),
            format,
            meta_hash: meta.to_string(),
            art_hash: art.to_string(),
            modes: vec![SourceMode::Mirror],
            trashed: false,
            private: false,
        }
    }

    fn present(size: u64) -> LocalFile {
        LocalFile { exists: true, size }
    }

    fn local_present(id: &str) -> HashMap<String, LocalFile> {
        [(id.to_string(), present(100))].into_iter().collect()
    }

    fn mirror_ok() -> Vec<SourceStatus> {
        vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: true,
        }]
    }

    // ── Per-clip classification ─────────────────────────────────────

    #[test]
    fn not_in_manifest_downloads() {
        let manifest = Manifest::new();
        let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "art")];
        let plan = reconcile(&manifest, &d, &HashMap::new(), &mirror_ok());
        assert_eq!(
            plan.actions,
            vec![Action::Download {
                clip: clip("a"),
                path: "a.flac".to_string(),
                format: AudioFormat::Flac,
            }]
        );
    }

    #[test]
    fn unchanged_clip_skips() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "art")];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(
            plan.actions,
            vec![Action::Skip {
                clip_id: "a".to_string()
            }]
        );
    }

    #[test]
    fn meta_change_retags_in_place() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "old", "art"));
        let d = vec![desired("a", "a.flac", AudioFormat::Flac, "new", "art")];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(
            plan.actions,
            vec![Action::Retag {
                clip: clip("a"),
                path: "a.flac".to_string(),
            }]
        );
    }

    #[test]
    fn art_change_retags_in_place() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "old-art"));
        let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "new-art")];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(
            plan.actions,
            vec![Action::Retag {
                clip: clip("a"),
                path: "a.flac".to_string(),
            }]
        );
    }

    #[test]
    fn rename_when_path_changes() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("old/a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired("a", "new/a.flac", AudioFormat::Flac, "m", "art")];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(
            plan.actions,
            vec![Action::Rename {
                from: "old/a.flac".to_string(),
                to: "new/a.flac".to_string(),
            }]
        );
    }

    #[test]
    fn rename_with_meta_change_also_retags() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("old/a.flac", AudioFormat::Flac, "old", "art"));
        let d = vec![desired("a", "new/a.flac", AudioFormat::Flac, "new", "art")];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(
            plan.actions,
            vec![
                Action::Rename {
                    from: "old/a.flac".to_string(),
                    to: "new/a.flac".to_string(),
                },
                Action::Retag {
                    clip: clip("a"),
                    path: "new/a.flac".to_string(),
                },
            ]
        );
    }

    #[test]
    fn rename_without_meta_change_does_not_retag() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("old/a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired("a", "new/a.flac", AudioFormat::Flac, "m", "art")];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.renames(), 1);
        assert_eq!(plan.retags(), 0);
    }

    #[test]
    fn format_change_reformats() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired("a", "a.mp3", AudioFormat::Mp3, "m", "art")];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(
            plan.actions,
            vec![Action::Reformat {
                clip: clip("a"),
                path: "a.mp3".to_string(),
                from: AudioFormat::Flac,
                to: AudioFormat::Mp3,
            }]
        );
    }

    #[test]
    fn format_change_takes_precedence_over_rename_and_retag() {
        // Format, path, and metadata all changed at once: a single reformat
        // replaces the file, so no separate rename or retag is emitted.
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry("old/a.flac", AudioFormat::Flac, "old", "old-art"),
        );
        let d = vec![desired(
            "a",
            "new/a.mp3",
            AudioFormat::Mp3,
            "new",
            "new-art",
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.reformats(), 1);
        assert_eq!(plan.renames(), 0);
        assert_eq!(plan.retags(), 0);
    }

    // ── SYNC-10: zero-length / missing local file ───────────────────

    #[test]
    fn zero_length_file_downloads_even_when_hashes_match() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let local: HashMap<String, LocalFile> = [(
            "a".to_string(),
            LocalFile {
                exists: true,
                size: 0,
            },
        )]
        .into_iter()
        .collect();
        let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "art")];
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert_eq!(plan.downloads(), 1);
        assert_eq!(plan.skips(), 0);
    }

    #[test]
    fn missing_file_downloads_even_when_hashes_match() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let local: HashMap<String, LocalFile> = [(
            "a".to_string(),
            LocalFile {
                exists: false,
                size: 0,
            },
        )]
        .into_iter()
        .collect();
        let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "art")];
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert_eq!(plan.downloads(), 1);
    }

    #[test]
    fn absent_local_probe_treated_as_missing() {
        // A manifest clip with no probe entry is conservatively re-downloaded.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "art")];
        let plan = reconcile(&manifest, &d, &HashMap::new(), &mirror_ok());
        assert_eq!(plan.downloads(), 1);
    }

    #[test]
    fn missing_file_download_wins_over_format_difference() {
        // A missing file is re-downloaded directly in the desired format rather
        // than reformatted from a file that is not there.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let local: HashMap<String, LocalFile> = [(
            "a".to_string(),
            LocalFile {
                exists: false,
                size: 0,
            },
        )]
        .into_iter()
        .collect();
        let d = vec![desired("a", "a.mp3", AudioFormat::Mp3, "m", "art")];
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert_eq!(plan.downloads(), 1);
        assert_eq!(plan.reformats(), 0);
    }

    // ── SYNC-12: trashed and private ────────────────────────────────

    #[test]
    fn trashed_clip_deletes_local_file() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.trashed = true;
        let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
        assert_eq!(
            plan.actions,
            vec![Action::Delete {
                path: "a.flac".to_string(),
                clip_id: "a".to_string(),
            }]
        );
    }

    #[test]
    fn trashed_clip_not_in_manifest_skips() {
        // Nothing on disk to remove, so trashing is a no-op.
        let manifest = Manifest::new();
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.trashed = true;
        let plan = reconcile(&manifest, &[d], &HashMap::new(), &mirror_ok());
        assert_eq!(
            plan.actions,
            vec![Action::Skip {
                clip_id: "a".to_string()
            }]
        );
    }

    #[test]
    fn private_clip_is_kept() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.private = true;
        let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
        assert_eq!(
            plan.actions,
            vec![Action::Skip {
                clip_id: "a".to_string()
            }]
        );
    }

    #[test]
    fn private_beats_trashed_never_deletes() {
        // Safety first: a clip that is both trashed and private is kept.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.trashed = true;
        d.private = true;
        let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.skips(), 1);
    }

    #[test]
    fn copy_held_trashed_clip_is_not_deleted() {
        // SYNC-8: copy always wins, so a trashed clip still held by a copy
        // source is kept and synced rather than deleted.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.modes = vec![SourceMode::Copy];
        d.trashed = true;
        let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
        assert_eq!(plan.deletes(), 0);
        assert_eq!(
            plan.actions,
            vec![Action::Skip {
                clip_id: "a".to_string()
            }]
        );
    }

    // ── Deletion pass: absent manifest entries ──────────────────────

    #[test]
    fn absent_clip_deleted_when_all_mirrors_enumerated() {
        let mut manifest = Manifest::new();
        manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
        let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
        assert_eq!(
            plan.actions,
            vec![Action::Delete {
                path: "gone.flac".to_string(),
                clip_id: "gone".to_string(),
            }]
        );
    }

    #[test]
    fn absent_clip_kept_when_any_mirror_not_enumerated() {
        let mut manifest = Manifest::new();
        manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
        let sources = vec![
            SourceStatus {
                mode: SourceMode::Mirror,
                fully_enumerated: true,
            },
            SourceStatus {
                mode: SourceMode::Mirror,
                fully_enumerated: false,
            },
        ];
        let plan = reconcile(&manifest, &[], &HashMap::new(), &sources);
        assert_eq!(plan.deletes(), 0);
        assert_eq!(
            plan.actions,
            vec![Action::Skip {
                clip_id: "gone".to_string()
            }]
        );
    }

    #[test]
    fn empty_listing_cannot_cause_deletion() {
        // A failed or truncated listing presents as a not-fully-enumerated
        // mirror source: absence must never delete in that case.
        let mut manifest = Manifest::new();
        manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
        let sources = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        }];
        let plan = reconcile(&manifest, &[], &HashMap::new(), &sources);
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.skips(), 1);
    }

    #[test]
    fn no_mirror_sources_means_no_deletion() {
        // Copy-only or sourceless runs are additive: nothing is deleted.
        let mut manifest = Manifest::new();
        manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
        let copy_only = vec![SourceStatus {
            mode: SourceMode::Copy,
            fully_enumerated: true,
        }];
        assert_eq!(
            reconcile(&manifest, &[], &HashMap::new(), &copy_only).deletes(),
            0
        );
        assert_eq!(reconcile(&manifest, &[], &HashMap::new(), &[]).deletes(), 0);
    }

    #[test]
    fn copy_source_with_unenumerated_mirror_still_suppresses_deletion() {
        let mut manifest = Manifest::new();
        manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
        let sources = vec![
            SourceStatus {
                mode: SourceMode::Copy,
                fully_enumerated: true,
            },
            SourceStatus {
                mode: SourceMode::Mirror,
                fully_enumerated: false,
            },
        ];
        assert_eq!(
            reconcile(&manifest, &[], &HashMap::new(), &sources).deletes(),
            0
        );
    }

    #[test]
    fn copy_held_clip_in_desired_is_never_a_deletion_candidate() {
        // SYNC-8 falls out naturally: a copy-held clip is in the desired set,
        // so it is classified there (Skip) and never reaches the delete pass,
        // even while a sibling clip is being deleted.
        let mut manifest = Manifest::new();
        manifest.insert("keep", entry("keep.flac", AudioFormat::Flac, "m", "art"));
        manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
        let mut held = desired("keep", "keep.flac", AudioFormat::Flac, "m", "art");
        held.modes = vec![SourceMode::Copy];
        let local: HashMap<String, LocalFile> = [
            ("keep".to_string(), present(100)),
            ("gone".to_string(), present(100)),
        ]
        .into_iter()
        .collect();
        let plan = reconcile(&manifest, &[held], &local, &mirror_ok());
        assert!(plan.actions.contains(&Action::Skip {
            clip_id: "keep".to_string()
        }));
        assert!(plan.actions.contains(&Action::Delete {
            path: "gone.flac".to_string(),
            clip_id: "gone".to_string(),
        }));
        // The copy-held clip is never deleted.
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::Delete { clip_id, .. } if clip_id == "keep"))
        );
    }

    // ── Determinism and robustness ──────────────────────────────────

    #[test]
    fn output_is_deterministic_regardless_of_input_order() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        manifest.insert("b", entry("b.flac", AudioFormat::Flac, "old", "art"));
        manifest.insert("z", entry("z.flac", AudioFormat::Flac, "m", "art"));
        let local: HashMap<String, LocalFile> = ["a", "b", "z"]
            .iter()
            .map(|id| (id.to_string(), present(100)))
            .collect();

        let forward = vec![
            desired("a", "a.flac", AudioFormat::Flac, "m", "art"),
            desired("b", "b.flac", AudioFormat::Flac, "new", "art"),
            desired("c", "c.flac", AudioFormat::Flac, "m", "art"),
        ];
        let mut reversed = forward.clone();
        reversed.reverse();

        let p1 = reconcile(&manifest, &forward, &local, &mirror_ok());
        let p2 = reconcile(&manifest, &reversed, &local, &mirror_ok());
        assert_eq!(p1.actions, p2.actions);

        // And the order is clip-id sorted: a (skip), b (retag), c (download),
        // then absent z (delete).
        let ids: Vec<&str> = p1
            .actions
            .iter()
            .map(|a| match a {
                Action::Skip { clip_id } => clip_id.as_str(),
                Action::Retag { clip, .. } => clip.id.as_str(),
                Action::Download { clip, .. } => clip.id.as_str(),
                Action::Delete { clip_id, .. } => clip_id.as_str(),
                Action::Reformat { clip, .. } => clip.id.as_str(),
                Action::Rename { to, .. } => to.as_str(),
            })
            .collect();
        assert_eq!(ids, ["a", "b", "c", "z"]);
    }

    #[test]
    fn empty_inputs_do_not_panic() {
        let plan = reconcile(&Manifest::new(), &[], &HashMap::new(), &[]);
        assert!(plan.is_empty());
        assert_eq!(plan.len(), 0);
    }

    #[test]
    fn empty_desired_with_full_manifest_deletes_all() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        manifest.insert("b", entry("b.flac", AudioFormat::Flac, "m", "art"));
        let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
        assert_eq!(plan.deletes(), 2);
    }

    #[test]
    fn full_desired_with_empty_manifest_downloads_all() {
        let d = vec![
            desired("a", "a.flac", AudioFormat::Flac, "m", "art"),
            desired("b", "b.flac", AudioFormat::Flac, "m", "art"),
        ];
        let plan = reconcile(&Manifest::new(), &d, &HashMap::new(), &mirror_ok());
        assert_eq!(plan.downloads(), 2);
    }

    #[test]
    fn plan_counts_sum_to_len() {
        let mut manifest = Manifest::new();
        manifest.insert("skip", entry("skip.flac", AudioFormat::Flac, "m", "art"));
        manifest.insert(
            "retag",
            entry("retag.flac", AudioFormat::Flac, "old", "art"),
        );
        manifest.insert(
            "reformat",
            entry("reformat.flac", AudioFormat::Flac, "m", "art"),
        );
        manifest.insert(
            "rename",
            entry("old/rename.flac", AudioFormat::Flac, "m", "art"),
        );
        manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
        let local: HashMap<String, LocalFile> = ["skip", "retag", "reformat", "rename", "gone"]
            .iter()
            .map(|id| (id.to_string(), present(100)))
            .collect();
        let d = vec![
            desired("skip", "skip.flac", AudioFormat::Flac, "m", "art"),
            desired("retag", "retag.flac", AudioFormat::Flac, "new", "art"),
            desired("reformat", "reformat.mp3", AudioFormat::Mp3, "m", "art"),
            desired("rename", "new/rename.flac", AudioFormat::Flac, "m", "art"),
            desired("download", "download.flac", AudioFormat::Flac, "m", "art"),
        ];
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        let summed = plan.downloads()
            + plan.reformats()
            + plan.retags()
            + plan.renames()
            + plan.deletes()
            + plan.skips();
        assert_eq!(summed, plan.len());
        assert_eq!(plan.downloads(), 1);
        assert_eq!(plan.reformats(), 1);
        assert_eq!(plan.retags(), 1);
        assert_eq!(plan.renames(), 1);
        assert_eq!(plan.deletes(), 1);
        assert_eq!(plan.skips(), 1);
    }
}

/// Property-based tests that lock the mass-delete guard against random inputs.
///
/// These complement the deterministic unit tests above. The generators are
/// bounded (a small clip-id space, short paths and hashes) so the cases stay
/// cheap and CI stays stable, and failure persistence is disabled so a run
/// never leaves regression files behind.
///
/// Scope note: the safety invariants INV1 to INV3 concern the orphan-delete
/// pass, which is the only place a single run can delete many files. A trashed
/// clip is a bounded, explicit, per-clip removal (covered by the unit tests),
/// so the INV1 to INV3 generators hold `trashed` false to keep those
/// invariants exact; the determinism invariant INV4 generates `trashed` freely.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::collection::{btree_map, hash_map, vec};
    use proptest::prelude::*;
    use proptest::strategy::BoxedStrategy;
    use std::collections::{BTreeMap, BTreeSet};

    type DesiredFields = (
        String,
        AudioFormat,
        String,
        String,
        Vec<SourceMode>,
        bool,
        bool,
    );

    fn audio_format() -> impl Strategy<Value = AudioFormat> {
        prop_oneof![
            Just(AudioFormat::Mp3),
            Just(AudioFormat::Flac),
            Just(AudioFormat::Wav),
        ]
    }

    fn source_mode() -> impl Strategy<Value = SourceMode> {
        prop_oneof![Just(SourceMode::Mirror), Just(SourceMode::Copy)]
    }

    // A small id space forces overlap between the manifest and the desired set,
    // so deletes, renames, retags, and downloads all get exercised.
    fn clip_id() -> impl Strategy<Value = String> {
        (0u8..8).prop_map(|n| format!("c{n}"))
    }

    fn small_path() -> impl Strategy<Value = String> {
        (0u8..6).prop_map(|n| format!("path{n}"))
    }

    fn small_hash() -> impl Strategy<Value = String> {
        (0u8..4).prop_map(|n| format!("h{n}"))
    }

    fn manifest_entry() -> impl Strategy<Value = ManifestEntry> {
        (
            small_path(),
            audio_format(),
            small_hash(),
            small_hash(),
            0u64..4,
        )
            .prop_map(|(path, format, meta_hash, art_hash, size)| ManifestEntry {
                path,
                format,
                meta_hash,
                art_hash,
                size,
            })
    }

    fn manifest_strategy() -> impl Strategy<Value = Manifest> {
        btree_map(clip_id(), manifest_entry(), 0..8).prop_map(|entries| Manifest { entries })
    }

    fn local_file() -> impl Strategy<Value = LocalFile> {
        (any::<bool>(), 0u64..4).prop_map(|(exists, size)| LocalFile { exists, size })
    }

    fn local_strategy() -> impl Strategy<Value = HashMap<String, LocalFile>> {
        hash_map(clip_id(), local_file(), 0..8)
    }

    fn source_status() -> impl Strategy<Value = SourceStatus> {
        (source_mode(), any::<bool>()).prop_map(|(mode, fully_enumerated)| SourceStatus {
            mode,
            fully_enumerated,
        })
    }

    fn sources_strategy() -> impl Strategy<Value = Vec<SourceStatus>> {
        vec(source_status(), 0..5)
    }

    fn copy_sources_strategy() -> impl Strategy<Value = Vec<SourceStatus>> {
        vec(
            any::<bool>().prop_map(|fully_enumerated| SourceStatus {
                mode: SourceMode::Copy,
                fully_enumerated,
            }),
            1..5,
        )
    }

    fn desired_fields(trashed: BoxedStrategy<bool>) -> impl Strategy<Value = DesiredFields> {
        (
            small_path(),
            audio_format(),
            small_hash(),
            small_hash(),
            vec(source_mode(), 1..3),
            trashed,
            any::<bool>(),
        )
    }

    fn build_desired(id: String, fields: DesiredFields) -> Desired {
        let (path, format, meta_hash, art_hash, modes, trashed, private) = fields;
        Desired {
            clip: Clip {
                id,
                title: "t".to_string(),
                ..Default::default()
            },
            path,
            format,
            meta_hash,
            art_hash,
            modes,
            trashed,
            private,
        }
    }

    fn to_desired_vec(map: BTreeMap<String, DesiredFields>) -> Vec<Desired> {
        map.into_iter()
            .map(|(id, f)| build_desired(id, f))
            .collect()
    }

    // Desired clips with `trashed` fixed false, for the delete-safety invariants.
    fn desired_strategy() -> impl Strategy<Value = Vec<Desired>> {
        btree_map(clip_id(), desired_fields(Just(false).boxed()), 0..8).prop_map(to_desired_vec)
    }

    // Desired clips with `trashed` random, for the determinism invariant.
    fn desired_any_strategy() -> impl Strategy<Value = Vec<Desired>> {
        btree_map(clip_id(), desired_fields(any::<bool>().boxed()), 0..8).prop_map(to_desired_vec)
    }

    fn desired_ids(desired: &[Desired]) -> BTreeSet<&str> {
        desired.iter().map(|d| d.clip.id.as_str()).collect()
    }

    fn copy_held_ids(desired: &[Desired]) -> BTreeSet<&str> {
        desired
            .iter()
            .filter(|d| d.modes.contains(&SourceMode::Copy))
            .map(|d| d.clip.id.as_str())
            .collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        // INVARIANT 1: never delete a clip that is still desired, and never
        // delete a copy-held clip.
        #[test]
        fn inv1_never_deletes_desired_or_copy_held(
            manifest in manifest_strategy(),
            desired in desired_strategy(),
            local in local_strategy(),
            sources in sources_strategy(),
        ) {
            let plan = reconcile(&manifest, &desired, &local, &sources);
            let present = desired_ids(&desired);
            let copy_held = copy_held_ids(&desired);
            for action in &plan.actions {
                if let Action::Delete { clip_id, .. } = action {
                    prop_assert!(
                        !present.contains(clip_id.as_str()),
                        "deleted a desired clip: {clip_id}"
                    );
                    prop_assert!(
                        !copy_held.contains(clip_id.as_str()),
                        "deleted a copy-held clip: {clip_id}"
                    );
                }
            }
        }

        // INVARIANT 2: a single not-fully-enumerated mirror source (truncated,
        // partial, empty, or failed listing) suppresses every deletion.
        #[test]
        fn inv2_no_delete_when_any_mirror_unenumerated(
            manifest in manifest_strategy(),
            desired in desired_strategy(),
            local in local_strategy(),
            mut sources in sources_strategy(),
        ) {
            sources.push(SourceStatus {
                mode: SourceMode::Mirror,
                fully_enumerated: false,
            });
            let plan = reconcile(&manifest, &desired, &local, &sources);
            prop_assert_eq!(plan.deletes(), 0);
        }

        // INVARIANT 3: a copy-only run is additive and never deletes.
        #[test]
        fn inv3_all_copy_sources_means_no_deletes(
            manifest in manifest_strategy(),
            desired in desired_strategy(),
            local in local_strategy(),
            sources in copy_sources_strategy(),
        ) {
            let plan = reconcile(&manifest, &desired, &local, &sources);
            prop_assert_eq!(plan.deletes(), 0);
        }

        // INVARIANT 4: identical inputs always yield an identical plan, and the
        // plan does not depend on the order of the desired or source lists.
        #[test]
        fn inv4_plan_is_deterministic(
            manifest in manifest_strategy(),
            desired in desired_any_strategy(),
            local in local_strategy(),
            sources in sources_strategy(),
        ) {
            let plan = reconcile(&manifest, &desired, &local, &sources);

            let again = reconcile(&manifest, &desired, &local, &sources);
            prop_assert_eq!(&plan, &again);

            let mut desired_rev = desired.clone();
            desired_rev.reverse();
            let mut sources_rev = sources.clone();
            sources_rev.reverse();
            let shuffled = reconcile(&manifest, &desired_rev, &local, &sources_rev);
            prop_assert_eq!(&plan, &shuffled);
        }
    }
}
