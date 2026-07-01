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
//!   archive always win. This holds both for the clip's current selection
//!   (`Desired::modes`) and across runs through the persisted
//!   [`ManifestEntry::preserve`] marker, so a copy-held or private clip whose
//!   source is later deselected, or whose copy listing fails, is still kept.
//! - SYNC-9: never delete on an empty, failed, partial, or truncated listing.
//!   Deletion is allowed only when every selected source (mirror and copy) was
//!   fully enumerated, and only when at least one mirror source was selected.
//! - SYNC-10: a manifest path that is missing or zero length on disk is treated
//!   as missing and re-downloaded, even when its hashes still match.
//! - SYNC-12: a clip trashed in Suno is removed from the source and its local
//!   file is deleted under the same enumeration guard; a private or copy-held
//!   clip is kept.
//!
//! Every `Delete`, whether for a trashed clip or an absent orphan, flows through
//! one guard ([`delete_action`]): a manifest entry must exist with a non-empty,
//! non-preserved path, deletion must be allowed for the run, and the clip must
//! not be copy-held or private in the current selection. A final pass suppresses
//! any `Delete` whose path collides with a file another action writes this run.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;

use crate::config::AudioFormat;
use crate::lineage::LineageContext;
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
    /// The clip's resolved lineage, carried so the executor tags with the same
    /// root/parent/album that drove naming and the change hash.
    pub lineage: LineageContext,
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
        lineage: LineageContext,
        path: String,
        format: AudioFormat,
    },
    /// Render the clip to `path` in `to`, replacing the prior `from` rendering.
    ///
    /// A format change always changes the file extension, so the prior file at
    /// `from_path` is a different path that must be removed once the new file is
    /// written; carrying it keeps the plan a full account of disk mutations.
    Reformat {
        clip: Clip,
        path: String,
        from_path: String,
        from: AudioFormat,
        to: AudioFormat,
    },
    /// Re-tag the existing file at `path` to match current metadata or art.
    Retag {
        clip: Clip,
        lineage: LineageContext,
        path: String,
    },
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
/// source with its enumeration status, which gates every deletion this run.
///
/// Duplicate `desired` entries for one clip id (the same clip held by a mirror
/// and a copy source, say) are aggregated first: the result is private if any
/// is, copy-held if any is, and trashed only if all are, so a stray trashed
/// duplicate can never defeat a sibling's protection.
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

    // Aggregate duplicate ids, then process in clip-id order for determinism.
    let desired = aggregate_desired(desired);
    let desired_ids: BTreeSet<&str> = desired.iter().map(|d| d.clip.id.as_str()).collect();

    let can_delete = deletion_allowed(sources);

    for d in &desired {
        plan_desired(d, manifest, local, can_delete, &mut actions);
    }

    // Absent manifest entries, processed in clip-id order (BTreeMap is sorted).
    for (clip_id, _entry) in manifest.iter() {
        if desired_ids.contains(clip_id.as_str()) {
            continue;
        }
        match delete_action(clip_id, manifest, can_delete) {
            Some(action) => actions.push(action),
            // SYNC-9 / preserve / empty-path: absence is unreliable or the entry
            // is protected, so keep the file rather than delete it.
            None => actions.push(Action::Skip {
                clip_id: clip_id.clone(),
            }),
        }
    }

    suppress_path_aliasing(&mut actions);
    Plan { actions }
}

/// Whether clips may be deleted this run.
///
/// SYNC-9: deletion requires at least one selected `Mirror` source and every
/// selected source (mirror and copy alike) fully enumerated. A failed or partial
/// copy listing is just as unreliable as a mirror one, so it suppresses deletes
/// too. With no mirror source there is no authoritative listing to delete
/// against, and copy-only runs are additive.
fn deletion_allowed(sources: &[SourceStatus]) -> bool {
    let mut saw_mirror = false;
    for status in sources {
        if !status.fully_enumerated {
            return false;
        }
        if status.mode == SourceMode::Mirror {
            saw_mirror = true;
        }
    }
    saw_mirror
}

/// The single gate every `Delete` passes through.
///
/// Returns a [`Action::Delete`] only when deletion is allowed for the run, a
/// manifest entry exists for the clip, its path is non-empty, and the entry is
/// not preserve-marked. A `None` result means the caller must keep the file.
fn delete_action(clip_id: &str, manifest: &Manifest, can_delete: bool) -> Option<Action> {
    if !can_delete {
        return None;
    }
    let entry = manifest.get(clip_id)?;
    if entry.path.is_empty() || entry.preserve {
        return None;
    }
    Some(Action::Delete {
        path: entry.path.clone(),
        clip_id: clip_id.to_string(),
    })
}

/// Collapse duplicate desired entries for one clip id into a single record.
///
/// Safety folds are order-independent: `private` and copy-held are unions, and
/// `trashed` is an intersection. The non-safety fields (clip, path, format,
/// hashes) are taken from a deterministic representative so the result never
/// depends on input order.
fn aggregate_desired(desired: &[Desired]) -> Vec<Desired> {
    let mut by_id: BTreeMap<&str, Desired> = BTreeMap::new();
    for d in desired {
        match by_id.get_mut(d.clip.id.as_str()) {
            None => {
                by_id.insert(d.clip.id.as_str(), d.clone());
            }
            Some(acc) => {
                let take = rep_key(d) < rep_key(acc);
                acc.private = acc.private || d.private;
                acc.trashed = acc.trashed && d.trashed;
                for mode in &d.modes {
                    if !acc.modes.contains(mode) {
                        acc.modes.push(*mode);
                    }
                }
                if take {
                    acc.clip = d.clip.clone();
                    acc.path = d.path.clone();
                    acc.format = d.format;
                    acc.meta_hash = d.meta_hash.clone();
                    acc.art_hash = d.art_hash.clone();
                }
            }
        }
    }
    let mut out: Vec<Desired> = by_id.into_values().collect();
    for d in &mut out {
        // Normalise modes to a canonical order so aggregation is deterministic.
        let has_mirror = d.modes.contains(&SourceMode::Mirror);
        let has_copy = d.modes.contains(&SourceMode::Copy);
        d.modes.clear();
        if has_mirror {
            d.modes.push(SourceMode::Mirror);
        }
        if has_copy {
            d.modes.push(SourceMode::Copy);
        }
    }
    out
}

/// A deterministic, order-independent sort key for choosing the representative
/// non-safety fields when aggregating duplicate desired entries.
fn rep_key(d: &Desired) -> (&str, &str, &str, u8) {
    let format = match d.format {
        AudioFormat::Mp3 => 0,
        AudioFormat::Flac => 1,
        AudioFormat::Wav => 2,
    };
    (
        d.path.as_str(),
        d.meta_hash.as_str(),
        d.art_hash.as_str(),
        format,
    )
}

/// Downgrade any `Delete` whose path is also written by a `Download`,
/// `Reformat`, or `Rename` this run, so a deletion can never clobber a file the
/// same plan just produced.
fn suppress_path_aliasing(actions: &mut [Action]) {
    let targets: BTreeSet<String> = actions
        .iter()
        .filter_map(|a| match a {
            Action::Download { path, .. } | Action::Reformat { path, .. } => Some(path.clone()),
            Action::Rename { to, .. } => Some(to.clone()),
            _ => None,
        })
        .collect();
    for a in actions.iter_mut() {
        if let Action::Delete { path, clip_id } = a
            && targets.contains(path.as_str())
        {
            *a = Action::Skip {
                clip_id: clip_id.clone(),
            };
        }
    }
}

/// Append the action(s) for one desired clip.
fn plan_desired(
    d: &Desired,
    manifest: &Manifest,
    local: &HashMap<String, LocalFile>,
    can_delete: bool,
    out: &mut Vec<Action>,
) {
    let clip_id = d.clip.id.as_str();
    let copy_held = d.modes.contains(&SourceMode::Copy);

    // SYNC-12: a trashed clip is removed from the source, so its local file is
    // deleted, but only when neither private nor copy-held (protection beats
    // removal) and only through the shared delete guard. If the guard refuses
    // (deletion not allowed, no entry, empty path, or preserve-marked), keep the
    // file rather than fall through to a re-download of a clip that is gone.
    if d.trashed && !d.private && !copy_held {
        match delete_action(clip_id, manifest, can_delete) {
            Some(action) => out.push(action),
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
            lineage: d.lineage.clone(),
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
            lineage: d.lineage.clone(),
            path: d.path.clone(),
            format: d.format,
        });
        return;
    }

    if d.format != entry.format {
        // Replace via re-encode; never pre-delete the existing file. The old
        // file lives at a different extension, so carry it for cleanup.
        out.push(Action::Reformat {
            clip: d.clip.clone(),
            path: d.path.clone(),
            from_path: entry.path.clone(),
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
                lineage: d.lineage.clone(),
                path: d.path.clone(),
            });
        }
        return;
    }

    if meta_or_art_changed(d, entry) {
        out.push(Action::Retag {
            clip: d.clip.clone(),
            lineage: d.lineage.clone(),
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

    fn lineage(id: &str) -> LineageContext {
        LineageContext::own_root(&clip(id))
    }

    fn entry(path: &str, format: AudioFormat, meta: &str, art: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            format,
            meta_hash: meta.to_string(),
            art_hash: art.to_string(),
            size: 100,
            preserve: false,
        }
    }

    fn preserved_entry(path: &str, format: AudioFormat, meta: &str, art: &str) -> ManifestEntry {
        ManifestEntry {
            preserve: true,
            ..entry(path, format, meta, art)
        }
    }

    fn desired(id: &str, path: &str, format: AudioFormat, meta: &str, art: &str) -> Desired {
        Desired {
            clip: clip(id),
            lineage: lineage(id),
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
                lineage: lineage("a"),
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
                lineage: lineage("a"),
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
                lineage: lineage("a"),
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
                    lineage: lineage("a"),
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
                from_path: "a.flac".to_string(),
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

    // ── Item 1: persisted preserve marker ───────────────────────────

    #[test]
    fn orphan_with_preserve_marker_is_kept() {
        // A copy-held or private clip whose source was deselected is absent from
        // desired, but the persisted marker still protects it from deletion.
        let mut manifest = Manifest::new();
        manifest.insert(
            "gone",
            preserved_entry("gone.flac", AudioFormat::Flac, "m", "art"),
        );
        let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
        assert_eq!(plan.deletes(), 0);
        assert_eq!(
            plan.actions,
            vec![Action::Skip {
                clip_id: "gone".to_string()
            }]
        );
    }

    #[test]
    fn trashed_clip_with_preserve_marker_is_kept() {
        // The marker also defends the trashed path: a preserved entry is never
        // deleted even when the clip is trashed and fully enumerated.
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            preserved_entry("a.flac", AudioFormat::Flac, "m", "art"),
        );
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.trashed = true;
        let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.skips(), 1);
    }

    // ── Item 2: unified, enumeration-gated delete guard ─────────────

    #[test]
    fn trashed_clip_kept_when_a_mirror_is_not_enumerated() {
        // The trashed path now obeys the same enumeration guard as orphans.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.trashed = true;
        let sources = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        }];
        let plan = reconcile(&manifest, &[d], &local_present("a"), &sources);
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.skips(), 1);
    }

    #[test]
    fn trashed_clip_kept_when_sources_empty() {
        // With no sources there is no authoritative listing, so even a trashed
        // clip is kept rather than deleted.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.trashed = true;
        let plan = reconcile(&manifest, &[d], &local_present("a"), &[]);
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.skips(), 1);
    }

    #[test]
    fn failed_copy_listing_suppresses_orphan_deletion() {
        // A partial or failed copy listing is as unreliable as a mirror one and
        // must suppress deletes, even with a fully enumerated mirror present.
        let mut manifest = Manifest::new();
        manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
        let sources = vec![
            SourceStatus {
                mode: SourceMode::Mirror,
                fully_enumerated: true,
            },
            SourceStatus {
                mode: SourceMode::Copy,
                fully_enumerated: false,
            },
        ];
        let plan = reconcile(&manifest, &[], &HashMap::new(), &sources);
        assert_eq!(plan.deletes(), 0);
    }

    #[test]
    fn failed_copy_listing_suppresses_trashed_deletion() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.trashed = true;
        let sources = vec![
            SourceStatus {
                mode: SourceMode::Mirror,
                fully_enumerated: true,
            },
            SourceStatus {
                mode: SourceMode::Copy,
                fully_enumerated: false,
            },
        ];
        let plan = reconcile(&manifest, &[d], &local_present("a"), &sources);
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.skips(), 1);
    }

    #[test]
    fn empty_path_entry_never_deletes() {
        // A default or partially written manifest entry can have an empty path;
        // that must never become a Delete of the account root.
        let mut manifest = Manifest::new();
        manifest.insert("gone", entry("", AudioFormat::Flac, "m", "art"));
        let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
        assert_eq!(plan.deletes(), 0);
        assert_eq!(
            plan.actions,
            vec![Action::Skip {
                clip_id: "gone".to_string()
            }]
        );
    }

    // ── Item 3: path aliasing suppression ───────────────────────────

    #[test]
    fn delete_suppressed_when_path_aliases_rename_target() {
        // Clip "a" renames into the path that absent clip "b" recorded; deleting
        // "b" would clobber the file "a" was just moved to, so it is suppressed.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("old/a.flac", AudioFormat::Flac, "m", "art"));
        manifest.insert("b", entry("new/a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired("a", "new/a.flac", AudioFormat::Flac, "m", "art")];
        let local: HashMap<String, LocalFile> = [
            ("a".to_string(), present(100)),
            ("b".to_string(), present(100)),
        ]
        .into_iter()
        .collect();
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert!(plan.actions.contains(&Action::Rename {
            from: "old/a.flac".to_string(),
            to: "new/a.flac".to_string(),
        }));
        // No delete targets the renamed-to path.
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::Delete { path, .. } if path == "new/a.flac"))
        );
        assert!(plan.actions.contains(&Action::Skip {
            clip_id: "b".to_string()
        }));
    }

    #[test]
    fn delete_suppressed_when_path_aliases_download_target() {
        // A new clip downloads to the path an absent clip recorded.
        let mut manifest = Manifest::new();
        manifest.insert("b", entry("shared.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired("a", "shared.flac", AudioFormat::Flac, "m", "art")];
        let plan = reconcile(&manifest, &d, &HashMap::new(), &mirror_ok());
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::Delete { .. }))
        );
        assert_eq!(plan.downloads(), 1);
    }

    // ── Item 5: aggregation of duplicate desired ids ────────────────

    #[test]
    fn duplicate_trashed_does_not_defeat_copy_sibling() {
        // The same clip held by a copy source and reported trashed by a mirror:
        // copy wins, so it is kept, not deleted.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut copy_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        copy_entry.modes = vec![SourceMode::Copy];
        let mut trashed_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        trashed_entry.modes = vec![SourceMode::Mirror];
        trashed_entry.trashed = true;
        let plan = reconcile(
            &manifest,
            &[copy_entry, trashed_entry],
            &local_present("a"),
            &mirror_ok(),
        );
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.skips(), 1);
    }

    #[test]
    fn duplicate_trashed_does_not_defeat_private_sibling() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut private_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        private_entry.private = true;
        let mut trashed_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        trashed_entry.trashed = true;
        let plan = reconcile(
            &manifest,
            &[private_entry, trashed_entry],
            &local_present("a"),
            &mirror_ok(),
        );
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.skips(), 1);
    }

    #[test]
    fn duplicate_trashed_deletes_only_when_all_trashed() {
        // Every duplicate trashed and unprotected: a single delete results.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut first = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        first.trashed = true;
        let mut second = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        second.trashed = true;
        let plan = reconcile(
            &manifest,
            &[first, second],
            &local_present("a"),
            &mirror_ok(),
        );
        assert_eq!(plan.deletes(), 1);
    }

    #[test]
    fn duplicate_desired_unions_modes() {
        // Mirror and copy entries for one id aggregate to a copy-held clip.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut mirror_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        mirror_entry.modes = vec![SourceMode::Mirror];
        mirror_entry.trashed = true;
        let mut copy_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        copy_entry.modes = vec![SourceMode::Copy];
        let plan = reconcile(
            &manifest,
            &[mirror_entry, copy_entry],
            &local_present("a"),
            &mirror_ok(),
        );
        // Copy-held wins over the trashed mirror entry, so no delete.
        assert_eq!(plan.deletes(), 0);
    }

    // ── Item 6: private is deletion-exempt only ─────────────────────

    #[test]
    fn private_new_clip_downloads() {
        // Private no longer short-circuits to Skip: a missing private clip is
        // downloaded like any other.
        let manifest = Manifest::new();
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.private = true;
        let plan = reconcile(&manifest, &[d], &HashMap::new(), &mirror_ok());
        assert_eq!(plan.downloads(), 1);
    }

    #[test]
    fn private_zero_length_file_redownloads() {
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
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.private = true;
        let plan = reconcile(&manifest, &[d], &local, &mirror_ok());
        assert_eq!(plan.downloads(), 1);
    }

    #[test]
    fn private_meta_change_retags() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "old", "art"));
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "new", "art");
        d.private = true;
        let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
        assert_eq!(plan.retags(), 1);
        assert_eq!(plan.deletes(), 0);
    }

    #[test]
    fn absent_private_clip_protected_by_preserve_marker() {
        // Items 1 and 6 together: a private clip deselected from the run is
        // absent from desired, but its preserve marker keeps it across runs.
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            preserved_entry("a.flac", AudioFormat::Flac, "m", "art"),
        );
        let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.skips(), 1);
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

/// Property-based tests that lock the delete guard against random inputs.
///
/// These complement the deterministic unit tests above. The generators are
/// bounded (a small clip-id space, short paths and hashes) so the cases stay
/// cheap and CI stays stable, and failure persistence is disabled so a run
/// never leaves regression files behind.
///
/// The generators are fully random: `trashed`, `private`, source `modes`, and
/// the persisted `preserve` marker are all exercised, and the desired list may
/// hold duplicate ids so aggregation is covered too. The invariants below are
/// written to hold for every such input, so the trashed delete path is no
/// longer a special case hidden from the property tests.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::collection::{btree_map, hash_map, vec};
    use proptest::prelude::*;
    use std::collections::BTreeSet;

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

    // The manifest entry path is the source of every `Delete.path`, so it must
    // occasionally be empty for INV9 to actually exercise the empty-path guard.
    fn manifest_path() -> impl Strategy<Value = String> {
        prop_oneof![
            1 => Just(String::new()),
            6 => small_path(),
        ]
    }

    fn small_hash() -> impl Strategy<Value = String> {
        (0u8..4).prop_map(|n| format!("h{n}"))
    }

    fn manifest_entry() -> impl Strategy<Value = ManifestEntry> {
        (
            manifest_path(),
            audio_format(),
            small_hash(),
            small_hash(),
            0u64..4,
            any::<bool>(),
        )
            .prop_map(|(path, format, meta_hash, art_hash, size, preserve)| {
                ManifestEntry {
                    path,
                    format,
                    meta_hash,
                    art_hash,
                    size,
                    preserve,
                }
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

    fn desired_fields() -> impl Strategy<Value = DesiredFields> {
        (
            small_path(),
            audio_format(),
            small_hash(),
            small_hash(),
            vec(source_mode(), 1..3),
            any::<bool>(),
            any::<bool>(),
        )
    }

    fn build_desired(id: String, fields: DesiredFields) -> Desired {
        let (path, format, meta_hash, art_hash, modes, trashed, private) = fields;
        let clip = Clip {
            id,
            title: "t".to_string(),
            ..Default::default()
        };
        Desired {
            lineage: LineageContext::own_root(&clip),
            clip,
            path,
            format,
            meta_hash,
            art_hash,
            modes,
            trashed,
            private,
        }
    }

    // A desired list over the shared id space that may hold duplicate ids, so
    // aggregation and the trashed/private/copy folds are all exercised.
    fn desired_strategy() -> impl Strategy<Value = Vec<Desired>> {
        vec((clip_id(), desired_fields()), 0..10).prop_map(|items| {
            items
                .into_iter()
                .map(|(id, fields)| build_desired(id, fields))
                .collect()
        })
    }

    fn desired_ids(desired: &[Desired]) -> BTreeSet<&str> {
        desired.iter().map(|d| d.clip.id.as_str()).collect()
    }

    // Ids protected from deletion: any duplicate that is private or copy-held
    // protects the whole id, mirroring the aggregation's union semantics.
    fn protected_ids(desired: &[Desired]) -> BTreeSet<&str> {
        desired
            .iter()
            .filter(|d| d.private || d.modes.contains(&SourceMode::Copy))
            .map(|d| d.clip.id.as_str())
            .collect()
    }

    // Ids with at least one non-trashed duplicate: the trashed fold is an
    // intersection, so one live duplicate keeps the clip.
    fn non_trashed_ids(desired: &[Desired]) -> BTreeSet<&str> {
        desired
            .iter()
            .filter(|d| !d.trashed)
            .map(|d| d.clip.id.as_str())
            .collect()
    }

    fn delete_clip_ids(plan: &Plan) -> Vec<&str> {
        plan.actions
            .iter()
            .filter_map(|a| match a {
                Action::Delete { clip_id, .. } => Some(clip_id.as_str()),
                _ => None,
            })
            .collect()
    }

    fn write_target_paths(plan: &Plan) -> BTreeSet<&str> {
        plan.actions
            .iter()
            .filter_map(|a| match a {
                Action::Download { path, .. } | Action::Reformat { path, .. } => {
                    Some(path.as_str())
                }
                Action::Rename { to, .. } => Some(to.as_str()),
                _ => None,
            })
            .collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        // INVARIANT 1: a desired clip is deleted only when every one of its
        // duplicates is trashed; one live (non-trashed) duplicate keeps it.
        #[test]
        fn inv1_desired_clip_deleted_only_when_fully_trashed(
            manifest in manifest_strategy(),
            desired in desired_strategy(),
            local in local_strategy(),
            sources in sources_strategy(),
        ) {
            let plan = reconcile(&manifest, &desired, &local, &sources);
            let present = desired_ids(&desired);
            let live = non_trashed_ids(&desired);
            for id in delete_clip_ids(&plan) {
                prop_assert!(
                    !(present.contains(id) && live.contains(id)),
                    "deleted a desired clip with a non-trashed duplicate: {id}"
                );
            }
        }

        // INVARIANT 2: a single not-fully-enumerated mirror source (truncated,
        // partial, empty, or failed listing) suppresses every deletion, trashed
        // clips included.
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
            desired in desired_strategy(),
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

        // INVARIANT 5: every Delete names a clip that exists in the manifest.
        #[test]
        fn inv5_every_delete_is_in_the_manifest(
            manifest in manifest_strategy(),
            desired in desired_strategy(),
            local in local_strategy(),
            sources in sources_strategy(),
        ) {
            let plan = reconcile(&manifest, &desired, &local, &sources);
            for id in delete_clip_ids(&plan) {
                prop_assert!(manifest.contains(id), "deleted a clip absent from the manifest: {id}");
            }
        }

        // INVARIANT 6: never delete a copy-held or private clip, whether that
        // protection is in the current selection or persisted on the manifest.
        #[test]
        fn inv6_never_deletes_protected_clip(
            manifest in manifest_strategy(),
            desired in desired_strategy(),
            local in local_strategy(),
            sources in sources_strategy(),
        ) {
            let plan = reconcile(&manifest, &desired, &local, &sources);
            let protected = protected_ids(&desired);
            for id in delete_clip_ids(&plan) {
                prop_assert!(!protected.contains(id), "deleted a copy-held or private clip: {id}");
                let preserved = manifest.get(id).map(|e| e.preserve).unwrap_or(false);
                prop_assert!(!preserved, "deleted a preserve-marked clip: {id}");
            }
        }

        // INVARIANT 7: every Delete requires deletion to be allowed for the run,
        // so the trashed path is no longer an exception to the enumeration guard.
        #[test]
        fn inv7_no_delete_unless_deletion_allowed(
            manifest in manifest_strategy(),
            desired in desired_strategy(),
            local in local_strategy(),
            sources in sources_strategy(),
        ) {
            let plan = reconcile(&manifest, &desired, &local, &sources);
            if !deletion_allowed(&sources) {
                prop_assert_eq!(plan.deletes(), 0);
            }
        }

        // INVARIANT 8: at most one Delete per clip id.
        #[test]
        fn inv8_at_most_one_delete_per_clip(
            manifest in manifest_strategy(),
            desired in desired_strategy(),
            local in local_strategy(),
            sources in sources_strategy(),
        ) {
            let plan = reconcile(&manifest, &desired, &local, &sources);
            let ids = delete_clip_ids(&plan);
            let unique: BTreeSet<&str> = ids.iter().copied().collect();
            prop_assert_eq!(ids.len(), unique.len());
        }

        // INVARIANT 9: no Delete carries an empty path.
        #[test]
        fn inv9_no_delete_with_empty_path(
            manifest in manifest_strategy(),
            desired in desired_strategy(),
            local in local_strategy(),
            sources in sources_strategy(),
        ) {
            let plan = reconcile(&manifest, &desired, &local, &sources);
            for action in &plan.actions {
                if let Action::Delete { path, .. } = action {
                    prop_assert!(!path.is_empty(), "delete with an empty path");
                }
            }
        }

        // INVARIANT 10: no Delete path equals a file another action writes this
        // run, so a deletion can never clobber a just-written file.
        #[test]
        fn inv10_no_delete_aliases_a_write_target(
            manifest in manifest_strategy(),
            desired in desired_strategy(),
            local in local_strategy(),
            sources in sources_strategy(),
        ) {
            let plan = reconcile(&manifest, &desired, &local, &sources);
            let targets = write_target_paths(&plan);
            for action in &plan.actions {
                if let Action::Delete { path, .. } = action {
                    prop_assert!(
                        !targets.contains(path.as_str()),
                        "delete path {path} aliases a write target"
                    );
                }
            }
        }
    }
}
