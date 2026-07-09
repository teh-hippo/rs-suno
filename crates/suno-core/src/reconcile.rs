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
//! one guard (`delete_action`): a manifest entry must exist with a non-empty,
//! non-preserved path, deletion must be allowed for the run, and the clip must
//! not be copy-held or private in the current selection. Two final passes guard
//! path collisions: one suppresses any `Delete` whose path a write also targets
//! this run, and one (`suppress_target_clobber`) suppresses any write, rename,
//! or move that would land on a path another clip's file still holds, so a
//! rename can never delete a protected file by overwriting it.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;

use crate::album_art::{AlbumArt, PlaylistState};
use crate::lineage::LineageContext;
use crate::manifest::{ArtifactState, Manifest, ManifestEntry};
use crate::model::Clip;
use crate::pathkey::{canonical_path_key, same_fs_path};
use crate::vocab::{ArtifactKind, AudioFormat, SourceMode, StemFormat};

mod album;
mod playlist;
mod types;

pub use album::plan_album_artifacts;
pub use playlist::plan_playlist_artifacts;
pub use types::*;

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
    // Aggregate duplicate ids and order by clip id for deterministic output. A
    // normal run has unique ids with canonical modes, so there is nothing to
    // merge: sort borrowed references and clone nothing. The owned merge runs
    // only when a duplicate id or a non-canonical mode list is actually present.
    let merged: Vec<Desired>;
    let ordered: Vec<&Desired> = if needs_aggregation(desired) {
        merged = aggregate_desired(desired);
        merged.iter().collect()
    } else {
        let mut refs: Vec<&Desired> = desired.iter().collect();
        refs.sort_unstable_by(|a, b| a.clip.id.cmp(&b.clip.id));
        refs
    };
    let desired_ids: HashSet<&str> = ordered.iter().map(|d| d.clip.id.as_str()).collect();
    // One audio action per desired clip (plus its sidecars) and one per absent
    // manifest entry; pre-size to cut reallocations on a large library.
    let mut actions: Vec<Action> = Vec::with_capacity(ordered.len() + manifest.len());

    let can_delete = deletion_allowed(sources);

    for &d in &ordered {
        // Decide the audio action(s) first (unchanged), then reconcile the
        // clip's artifacts alongside. A clip whose audio is being deleted this
        // run has its sidecars co-deleted under the same gate; otherwise its
        // desired artifacts are written, any removed kind reconciled, and any
        // sidecar or stem stranded by a retitle (or a pre-fix run) relocated to
        // the current audio base (#355).
        let before = actions.len();
        plan_desired(d, manifest, local, can_delete, &mut actions);
        let audio_deleted = actions[before..]
            .iter()
            .any(|a| matches!(a, Action::Delete { .. }));
        if audio_deleted {
            co_delete_artifacts(d.clip.id.as_str(), manifest, can_delete, &mut actions);
            co_delete_stems(d.clip.id.as_str(), manifest, can_delete, &mut actions);
        } else {
            plan_clip_artifacts(d, manifest, local, can_delete, &mut actions);
            plan_clip_stems(d, manifest, local, can_delete, &mut actions);
        }
    }

    // Absent manifest entries, processed in clip-id order (BTreeMap is sorted).
    for (clip_id, _entry) in manifest.iter() {
        if desired_ids.contains(clip_id.as_str()) {
            continue;
        }
        match delete_action(clip_id, manifest, can_delete) {
            Some(action) => {
                actions.push(action);
                // Co-delete the absent clip's sidecars and stems under the same
                // gate, so neither a sidecar nor the `.stems` folder is stranded.
                co_delete_artifacts(clip_id, manifest, can_delete, &mut actions);
                co_delete_stems(clip_id, manifest, can_delete, &mut actions);
            }
            // SYNC-9 / preserve / empty-path: absence is unreliable or the entry
            // is protected, so keep the file rather than delete it.
            None => actions.push(Action::Skip {
                clip_id: clip_id.clone(),
            }),
        }
    }

    suppress_target_clobber(&mut actions, manifest);
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
///
/// This is the single deletion verdict for the run; the CLI threads the same
/// value into [`plan_album_artifacts`] so folder-art deletes share it.
pub fn deletion_allowed(sources: &[SourceStatus]) -> bool {
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

/// Whether an area listing is authoritative for deletion, before the
/// empty-mirror guard.
///
/// Any area -- library, liked feed, or playlist -- is authoritative only when
/// its listing drained completely (`complete`), no member was lost to the
/// downloadable filter (`any_filtered`), and no `--limit`/`--since` narrowing
/// was applied (`narrowed`). A member that transiently fails the filter would
/// otherwise look absent and see its master deleted, so any filter loss disarms
/// deletion uniformly across every source (#148, #248); a narrowing likewise
/// always defers deletion to a full run.
pub fn area_authoritative(complete: bool, any_filtered: bool, narrowed: bool) -> bool {
    complete && !any_filtered && !narrowed
}

/// Whether an area that was not deliberately narrowed is fully enumerated after
/// applying the empty-mirror guard (§5).
///
/// An empty Mirror area is never authoritative: an empty listing and a dropped
/// listing are indistinguishable, so this guard is always applied. An empty Copy
/// area is authoritative (it protects nothing) and is treated as fully
/// enumerated so it does not suppress deletion.
///
/// `authoritative` is the area's completeness verdict before this guard (the
/// listing drained, no narrowing, no filter loss). `clips_empty` is whether the
/// area returned zero clips. `mode` is the area's final mode after any
/// copy-verb override.
pub fn area_fully_enumerated(authoritative: bool, clips_empty: bool, mode: SourceMode) -> bool {
    authoritative && !(clips_empty && mode == SourceMode::Mirror)
}

/// Whether `--limit`/`--since` may narrow the download selection.
///
/// Only a run that neither deletes nor lists an authoritative full library may
/// truncate the union: narrowing while a mirror is armed would drop a
/// mirror/protector clip into a deletion (D2), and narrowing when a full library
/// is listed would regress the library index and folder art built from the
/// complete set. So truncation structurally implies no deletion.
pub fn narrows_downloads(can_delete: bool, library_authoritative: bool) -> bool {
    !can_delete && !library_authoritative
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

/// The deletion floor every per-kind delete gate shares.
///
/// A delete may be authorised only on a deletion-enabled run (`can_delete`, the
/// shared [`deletion_allowed`] verdict) and never against an empty `path` (which
/// could otherwise target the account root). Each gate ANDs its own specific
/// predicates on top of this floor.
fn delete_gate_open(can_delete: bool, path: &str) -> bool {
    can_delete && !path.is_empty()
}

/// The manifest-backed deletion floor shared by the per-clip artifact and stem
/// gates.
///
/// The run floor ([`delete_gate_open`]) plus a live, non-`preserve`-marked owning
/// manifest entry: a preserved clip's sidecars and stems are preserved too, and
/// an untracked owner is never delete-reconciled.
fn clip_owned_delete_open(
    owner_id: &str,
    path: &str,
    manifest: &Manifest,
    can_delete: bool,
) -> bool {
    delete_gate_open(can_delete, path)
        && manifest.get(owner_id).is_some_and(|entry| !entry.preserve)
}

/// The gate every per-clip-sidecar `DeleteArtifact` passes through.
///
/// This is the artifact analogue of [`delete_action`] and deliberately shares
/// the audio deletion safety: it returns a [`Action::DeleteArtifact`] only when
/// deletion is allowed for the run (`can_delete`, the same
/// [`deletion_allowed`] verdict), the owning manifest entry exists, the sidecar
/// `path` is non-empty (so an empty path can never delete the account root), and
/// the owning entry is not `preserve`-marked (a preserved clip's artifacts are
/// preserved too). A `None` result means the caller must keep the sidecar.
fn delete_artifact_action(
    owner_id: &str,
    kind: ArtifactKind,
    path: &str,
    manifest: &Manifest,
    can_delete: bool,
) -> Option<Action> {
    clip_owned_delete_open(owner_id, path, manifest, can_delete).then(|| Action::DeleteArtifact {
        kind,
        path: path.to_string(),
        owner_id: owner_id.to_string(),
    })
}

/// Whether a no-longer-desired ("removed kind") artifact may be delete-reconciled
/// while its owning clip's audio is kept this run.
///
/// The static `CoverJpg` deliberately opts out: a clip's art URL can be
/// transiently absent for a run (the feed omits it, or a fetch fails), and the
/// desired set then simply lacks that cover. Treating that absence as a removal
/// and deleting the on-disk sidecar would churn a perfectly good cover, so an
/// empty/transient URL must KEEP the existing file. `CoverJpg` is therefore
/// removed only by [`co_delete_artifacts`], when the owning clip leaves every
/// mirror source and its audio is deleted (a fully gated path).
///
/// `CoverWebp` is the opposite: it is a RETIRED kind. The per-clip animated
/// cover is now embedded in the audio file, never written as a `<track>.webp`
/// sidecar, so a desired set NEVER contains `CoverWebp` — its absence is
/// unconditional, not transient. It is therefore delete-eligible so any
/// `.webp` sidecar left by an older version is cleaned up on the next
/// deletion-enabled run, through the same gate every other delete passes.
///
/// The text sidecars split on totality. [`render_clip_details`](crate::render_clip_details)
/// is TOTAL (always renders), so a desired `DetailsTxt` is absent only when the
/// feature is off — an unambiguous removal that is safe to delete through the
/// shared gate. [`render_clip_lyrics`](crate::render_clip_lyrics) is PARTIAL
/// (`None` on empty lyrics), so an absent `LyricsTxt` is ambiguous (feature off
/// OR a transient empty-lyrics read); it opts out cover-style, so turning the
/// lyrics feature off leaves existing `.lyrics.txt` files in place. The untimed
/// [`Lrc`](ArtifactKind::Lrc) sidecar is partial the same way and opts out too.
///
/// [`VideoMp4`](ArtifactKind::VideoMp4) also opts out: `video_url` can be
/// transiently absent, and the video is a large binary a user would not expect
/// a run to delete merely because the feature was switched off. Like a cover, it
/// is removed only when its owning audio is deleted.
fn removed_kind_delete_eligible(kind: ArtifactKind) -> bool {
    match kind {
        ArtifactKind::CoverJpg
        | ArtifactKind::LyricsTxt
        | ArtifactKind::Lrc
        | ArtifactKind::VideoMp4 => false,
        ArtifactKind::CoverWebp
        | ArtifactKind::DetailsTxt
        | ArtifactKind::FolderJpg
        | ArtifactKind::FolderWebp
        | ArtifactKind::FolderMp4
        | ArtifactKind::Playlist => true,
    }
}

/// The per-clip artifacts an entry currently records, paired with their kind, in
/// a stable order. Only the per-song sidecars live on the entry today.
fn manifest_artifacts(entry: &ManifestEntry) -> Vec<(ArtifactKind, &ArtifactState)> {
    let mut out = Vec::new();
    if let Some(state) = &entry.cover_jpg {
        out.push((ArtifactKind::CoverJpg, state));
    }
    if let Some(state) = &entry.cover_webp {
        out.push((ArtifactKind::CoverWebp, state));
    }
    if let Some(state) = &entry.details_txt {
        out.push((ArtifactKind::DetailsTxt, state));
    }
    if let Some(state) = &entry.lyrics_txt {
        out.push((ArtifactKind::LyricsTxt, state));
    }
    if let Some(state) = &entry.lrc {
        out.push((ArtifactKind::Lrc, state));
    }
    if let Some(state) = &entry.video_mp4 {
        out.push((ArtifactKind::VideoMp4, state));
    }
    out
}

/// Set (or clear) the manifest slot for a per-clip artifact kind.
///
/// The executor calls this after a [`Action::WriteArtifact`] (with the new
/// state) or a [`Action::DeleteArtifact`] (with `None`), so the kind-to-field
/// mapping lives in exactly one place. Album/library classes have no per-clip
/// slot yet and are no-ops.
pub(crate) fn set_manifest_artifact(
    entry: &mut ManifestEntry,
    kind: ArtifactKind,
    state: Option<ArtifactState>,
) {
    match kind {
        ArtifactKind::CoverJpg => entry.cover_jpg = state,
        ArtifactKind::CoverWebp => entry.cover_webp = state,
        ArtifactKind::DetailsTxt => entry.details_txt = state,
        ArtifactKind::LyricsTxt => entry.lyrics_txt = state,
        ArtifactKind::Lrc => entry.lrc = state,
        ArtifactKind::VideoMp4 => entry.video_mp4 = state,
        ArtifactKind::FolderJpg
        | ArtifactKind::FolderWebp
        | ArtifactKind::FolderMp4
        | ArtifactKind::Playlist => {}
    }
}

/// Set (or clear) one stem slot in a clip's keyed stem map.
///
/// The executor calls this after a [`Action::WriteStem`] (with the new state)
/// or a [`Action::DeleteStem`] (with `None`), so the map mutation lives in one
/// place. Clearing the last stem leaves an empty map, which serialises away.
pub(crate) fn set_manifest_stem(
    entry: &mut ManifestEntry,
    key: &str,
    state: Option<ArtifactState>,
) {
    match state {
        Some(state) => {
            entry.stems.insert(key.to_string(), state);
        }
        None => {
            entry.stems.remove(key);
        }
    }
}

fn needs_write_drift(
    stored: Option<(&str, &str)>,
    want_hash: &str,
    want_path: &str,
    local: &HashMap<String, LocalFile>,
) -> bool {
    match stored {
        None => true,
        Some((stored_hash, stored_path)) => {
            stored_hash != want_hash
                || !same_fs_path(stored_path, want_path)
                || local
                    .get(stored_path)
                    .is_some_and(|f| !f.exists || f.size == 0)
        }
    }
}

/// The extensionless audio base of a rendered path (the value the sidecars and
/// the `.stems` folder are built from). `path` ends with `.{format.ext()}` by
/// construction in `build_desired`, so strip that exact suffix rather than the
/// last `.`, keeping a base whose own name contains a dot intact
/// ("Artist/Song 2.0.flac" -> "Artist/Song 2.0"). `None` when the extension is
/// absent (a hand-edited manifest), so a malformed input is skipped, never
/// panicked (#355).
fn audio_base(path: &str, format: AudioFormat) -> Option<&str> {
    path.strip_suffix(format!(".{}", format.ext()).as_str())
}

/// Reparent a stem's folder segment onto `new_base`, preserving the inner file
/// name verbatim. `None` when `old` carries no `.stems/` segment (#355).
fn relocated_stem_path(new_base: &str, old: &str) -> Option<String> {
    let idx = old.rfind(".stems/")?;
    Some(format!(
        "{new_base}.stems/{}",
        &old[idx + ".stems/".len()..]
    ))
}

/// Reconcile the artifacts of a clip whose audio is kept this run.
///
/// Writes each desired per-clip artifact that the manifest lacks, whose stored
/// hash drifts, whose stored path drifts (the audio moved), or whose file is
/// absent on disk. Delete-reconciles each manifest artifact whose kind is no
/// longer desired (a removed kind) through the shared [`delete_artifact_action`]
/// gate, unless the clip is protected this run, and unless the kind opts out of
/// removed-kind deletion ([`removed_kind_delete_eligible`]) — cover art does, so
/// a transient empty URL keeps its sidecar rather than deleting it.
///
/// Finally, a stranded-sidecar relocation pass (#355) reparents to the current
/// audio base any tracked per-clip sidecar the desired-write loop did not move
/// and the delete loop is not deleting, so an audio retitle no longer strands
/// the cover or text sidecars at an old base. It is anchored on the CURRENT
/// desired base, so it heals a historical strand too, and is idempotent.
///
/// `local` is the same path-keyed probe map that [`reconcile`] received,
/// extended by the caller to include the artifact paths in the manifest. A
/// manifest slot whose path resolves to a missing or zero-size file forces
/// `needs_write = true`. A path absent from `local` (probe unavailable) falls
/// back to hash/path comparison only.
fn plan_clip_artifacts(
    d: &Desired,
    manifest: &Manifest,
    local: &HashMap<String, LocalFile>,
    can_delete: bool,
    out: &mut Vec<Action>,
) {
    let owner_id = d.clip.id.as_str();
    let entry = manifest.get(owner_id);

    for artifact in &d.artifacts {
        // Per-clip reconcile owns the per-clip sidecars (cover art, details,
        // lyrics, .lrc, video). Album/library classes (folder art, playlists)
        // belong to later phases; ignore them here so they are not rewritten
        // every run.
        if !artifact.kind.is_per_clip() {
            continue;
        }
        // A write is needed when the manifest lacks the sidecar, its bytes drift
        // (hash), the clip moved so the sidecar belongs at a new path, or the
        // tracked file is absent (or empty) on disk. A pure relocation (same
        // bytes, new path, old file present) is emitted as a MoveArtifact below,
        // which renames rather than re-fetching (#141).
        let state = entry.and_then(|e| e.artifact(artifact.kind));
        let needs_write = needs_write_drift(
            state.map(|state| (state.hash.as_str(), state.path.as_str())),
            artifact.hash.as_str(),
            artifact.path.as_str(),
            local,
        );
        if needs_write {
            // Downgrade a pure relocation to a rename: only the path drifted (a
            // retitle), the bytes are unchanged, the kind is fetched (an inline
            // rewrite is already free), and the old file is confirmed present, so
            // move it rather than re-fetch or re-transcode (#141). The executor
            // falls back to a fetch-and-write if the old file has since vanished.
            if let Some(state) = state
                && state.hash == artifact.hash
                && !same_fs_path(&state.path, &artifact.path)
                && artifact.content.is_none()
                && local
                    .get(&state.path)
                    .is_some_and(|f| f.exists && f.size > 0)
            {
                out.push(Action::MoveArtifact {
                    kind: artifact.kind,
                    from: state.path.clone(),
                    to: artifact.path.clone(),
                    source_url: artifact.source_url.clone(),
                    hash: artifact.hash.clone(),
                    owner_id: owner_id.to_string(),
                });
            } else {
                out.push(Action::WriteArtifact {
                    kind: artifact.kind,
                    path: artifact.path.clone(),
                    source_url: artifact.source_url.clone(),
                    hash: artifact.hash.clone(),
                    owner_id: owner_id.to_string(),
                    content: artifact.content.clone(),
                });
            }
        }
    }

    // A clip protected THIS run (private or copy-held) keeps its sidecars even
    // when a kind is no longer desired, regardless of the persisted preserve
    // marker (which may still be false on the run that first protects the clip).
    // Preserve wins, so no removed-kind delete is emitted for it.
    let protected_now = d.private || d.modes.contains(&SourceMode::Copy);
    if !protected_now && let Some(entry) = entry {
        let desired_kinds: BTreeSet<ArtifactKind> = d
            .artifacts
            .iter()
            .filter(|a| a.kind.is_per_clip())
            .map(|a| a.kind)
            .collect();
        for (kind, state) in manifest_artifacts(entry) {
            // Cover kinds opt out of removed-kind deletion (see
            // `removed_kind_delete_eligible`): an absent desired cover means an
            // empty/transient URL, which must KEEP the on-disk sidecar, never
            // delete it. Only a co-delete (audio gone) removes a cover. The loop
            // and gate stay in place for any future kind that opts back in.
            if removed_kind_delete_eligible(kind)
                && !desired_kinds.contains(&kind)
                && let Some(action) =
                    delete_artifact_action(owner_id, kind, &state.path, manifest, can_delete)
            {
                out.push(action);
            }
        }
    }

    // #355: relocate any per-clip sidecar the desired-write loop did not, to the
    // current audio base. A kept kind absent from `d.artifacts` (cover, lyrics,
    // .lrc, video), or a delete-eligible kind on a run that is not deleting it,
    // would otherwise strand at an old base after a retitle - or stay stranded
    // from a pre-fix run that advanced `entry.path` without moving it. Reparenting
    // to `{new_base}{suffix}` heals both; it is idempotent (a correctly placed
    // file equals its expected path, so `same_fs_path` short-circuits). Guarded
    // on the old file being present (no source_url is known, so a fetch fallback
    // cannot help) and rides `suppress_target_clobber`, so it can never clobber a
    // kept file.
    // NOTE: !d.trashed is a conservative superset; see plan "Review deltas" #1 (a
    // trashed+private+renamed clip's sidecar heal is deferred, never a
    // delete/clobber).
    if !d.trashed
        && let Some(entry) = entry
        && let Some(new_base) = audio_base(&d.path, d.format).filter(|b| !b.is_empty())
    {
        let desired_kinds: BTreeSet<ArtifactKind> = d
            .artifacts
            .iter()
            .filter(|a| a.kind.is_per_clip())
            .map(|a| a.kind)
            .collect();
        for (kind, state) in manifest_artifacts(entry) {
            if desired_kinds.contains(&kind) {
                continue; // owned by the desired-write loop above
            }
            // A kind actually being deleted this run must not also be moved. Gate
            // on the REAL delete verdict (not a `can_delete` proxy), which also
            // refuses for a protected clip or a preserve-marked entry, so those
            // kinds are healed rather than stranded. The gate is pure, so this
            // second evaluation has no side effect.
            let deleted_this_run = !protected_now
                && removed_kind_delete_eligible(kind)
                && delete_artifact_action(owner_id, kind, &state.path, manifest, can_delete)
                    .is_some();
            if deleted_this_run {
                continue;
            }
            if let Some(suffix) = kind.sidecar_suffix() {
                let to = format!("{new_base}{suffix}");
                if !same_fs_path(&state.path, &to)
                    && local
                        .get(&state.path)
                        .is_some_and(|f| f.exists && f.size > 0)
                {
                    out.push(Action::MoveArtifact {
                        kind,
                        from: state.path.clone(),
                        to,
                        source_url: String::new(),
                        hash: state.hash.clone(),
                        owner_id: owner_id.to_string(),
                    });
                }
            }
        }
    }
}

/// Co-delete every sidecar of a clip whose audio is being deleted this run.
///
/// Each removal flows through the shared [`delete_artifact_action`] gate, so a
/// sidecar is co-deleted only when the audio delete itself was allowed; on an
/// incomplete listing or a preserved entry nothing is emitted.
fn co_delete_artifacts(
    owner_id: &str,
    manifest: &Manifest,
    can_delete: bool,
    out: &mut Vec<Action>,
) {
    let Some(entry) = manifest.get(owner_id) else {
        return;
    };
    for (kind, state) in manifest_artifacts(entry) {
        if let Some(action) =
            delete_artifact_action(owner_id, kind, &state.path, manifest, can_delete)
        {
            out.push(action);
        }
    }
}

/// The single gate every [`Action::DeleteStem`] passes through.
///
/// The keyed-stem analogue of [`delete_artifact_action`], sharing the exact
/// audio deletion safety: it returns a delete only when deletion is allowed for
/// the run (`can_delete`), the owning manifest entry exists, the stem `path` is
/// non-empty (so an empty path can never delete the account root), and the
/// owning entry is not `preserve`-marked (a preserved clip's stems are preserved
/// too). A `None` result means the caller must keep the stem file.
fn delete_stem_action(
    clip_id: &str,
    key: &str,
    path: &str,
    manifest: &Manifest,
    can_delete: bool,
) -> Option<Action> {
    clip_owned_delete_open(clip_id, path, manifest, can_delete).then(|| Action::DeleteStem {
        clip_id: clip_id.to_string(),
        key: key.to_string(),
        path: path.to_string(),
    })
}

/// Reconcile the keyed stems of a clip whose audio is kept this run.
///
/// When `d.stems` is `None` the listing is not authoritative (feature off,
/// `has_stem` false, or a disabled/failed/partial/`400` listing), so nothing is
/// written and nothing is deleted — a paging error is never read as "no stems".
/// It does, however, relocate any tracked stem to the current `.stems` folder so
/// a retitle (or a pre-fix strand) no longer orphans it (#355). When `d.stems`
/// is `Some(set)`, the set is authoritative:
///
/// - each desired stem the manifest lacks, whose stored hash drifts, or whose
///   stored path drifts (the song moved), is written or, when only the path
///   drifts and the old file is present, relocated with a rename (#141); and
/// - each tracked stem whose key is absent from the authoritative set is
///   delete-reconciled through the shared [`delete_stem_action`] gate, unless
///   the clip is protected this run (private or copy-held).
///
/// A protected clip keeps every stem regardless of the persisted `preserve`
/// marker (which may still be false on the run that first protects the clip).
fn plan_clip_stems(
    d: &Desired,
    manifest: &Manifest,
    local: &HashMap<String, LocalFile>,
    can_delete: bool,
    out: &mut Vec<Action>,
) {
    let clip_id = d.clip.id.as_str();
    let entry = manifest.get(clip_id);
    let Some(desired_stems) = &d.stems else {
        // #355: no authoritative stem listing this run, so write nothing and
        // delete nothing; but relocate any tracked stem to the current `.stems`
        // folder rather than stranding it after a retitle (or from a pre-fix
        // run). Folder-only reparent: the inner filename keeps the old title and
        // self-heals to the new one on the next `--download-stems` run via the
        // existing path-drift MoveStem. Guarded on the old file being present;
        // rides the clobber backstop.
        // NOTE: !d.trashed is a conservative superset; see plan "Review deltas"
        // #1 (a trashed+private+renamed clip's sidecar heal is deferred, never a
        // delete/clobber).
        if !d.trashed
            && let Some(entry) = entry
            && let Some(new_base) = audio_base(&d.path, d.format).filter(|b| !b.is_empty())
        {
            for (key, state) in &entry.stems {
                if let Some(to) = relocated_stem_path(new_base, &state.path)
                    && !same_fs_path(&state.path, &to)
                    && local
                        .get(&state.path)
                        .is_some_and(|f| f.exists && f.size > 0)
                {
                    out.push(Action::MoveStem {
                        clip_id: clip_id.to_string(),
                        key: key.clone(),
                        stem_id: String::new(),
                        from: state.path.clone(),
                        to,
                        source_url: String::new(),
                        format: StemFormat::Mp3,
                        hash: state.hash.clone(),
                    });
                }
            }
        }
        return;
    };

    for stem in desired_stems {
        let state = entry.and_then(|e| e.stems.get(&stem.key));
        let needs_write = match state {
            None => true,
            Some(state) => state.hash != stem.hash || !same_fs_path(&state.path, &stem.path),
        };
        if needs_write {
            // Downgrade a pure relocation to a rename: only the path drifted and
            // the bytes are unchanged, so move the raw stem rather than re-render
            // a WAV via convert_wav or re-fetch an MP3 (#141). The executor falls
            // back to a fetch-and-write if the old file has since vanished.
            if let Some(state) = state
                && state.hash == stem.hash
                && !same_fs_path(&state.path, &stem.path)
                && local
                    .get(&state.path)
                    .is_some_and(|f| f.exists && f.size > 0)
            {
                out.push(Action::MoveStem {
                    clip_id: clip_id.to_string(),
                    key: stem.key.clone(),
                    stem_id: stem.stem_id.clone(),
                    from: state.path.clone(),
                    to: stem.path.clone(),
                    source_url: stem.source_url.clone(),
                    format: stem.format,
                    hash: stem.hash.clone(),
                });
            } else {
                out.push(Action::WriteStem {
                    clip_id: clip_id.to_string(),
                    key: stem.key.clone(),
                    stem_id: stem.stem_id.clone(),
                    path: stem.path.clone(),
                    source_url: stem.source_url.clone(),
                    format: stem.format,
                    hash: stem.hash.clone(),
                });
            }
        }
    }

    let protected_now = d.private || d.modes.contains(&SourceMode::Copy);
    if !protected_now && let Some(entry) = entry {
        let desired_keys: BTreeSet<&str> = desired_stems.iter().map(|s| s.key.as_str()).collect();
        for (key, state) in &entry.stems {
            // A tracked stem the authoritative listing no longer contains is a
            // genuine removal (the stem was deleted on Suno), reconciled through
            // the shared gate. This fires ONLY for an authoritative set, so an
            // empty/partial/paged-error listing (`d.stems == None`) never reaches
            // here and can never delete a stem.
            if !desired_keys.contains(key.as_str())
                && let Some(action) =
                    delete_stem_action(clip_id, key, &state.path, manifest, can_delete)
            {
                out.push(action);
            }
        }
    }
}

/// Co-delete every stem of a clip whose audio is being deleted this run.
///
/// Each removal flows through the shared [`delete_stem_action`] gate, so a stem
/// is co-deleted only when the audio delete itself was allowed; on an incomplete
/// listing or a preserved entry nothing is emitted. This is what keeps a
/// `.stems` sub-folder from being orphaned when its song is deleted: the stem
/// files are removed alongside the audio, and the now-empty folder is pruned.
fn co_delete_stems(clip_id: &str, manifest: &Manifest, can_delete: bool, out: &mut Vec<Action>) {
    let Some(entry) = manifest.get(clip_id) else {
        return;
    };
    for (key, state) in &entry.stems {
        if let Some(action) = delete_stem_action(clip_id, key, &state.path, manifest, can_delete) {
            out.push(action);
        }
    }
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
                    acc.embedded_lyrics_hash = d.embedded_lyrics_hash.clone();
                    acc.artifacts = d.artifacts.clone();
                    acc.stems = d.stems.clone();
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

/// Whether [`aggregate_desired`] must build an owned, merged copy: true when a
/// clip id repeats or any entry's modes are not already in canonical
/// `[Mirror, Copy]` order. When this is false the input is already the
/// aggregated result and can be used as-is.
fn needs_aggregation(desired: &[Desired]) -> bool {
    let mut seen: HashSet<&str> = HashSet::with_capacity(desired.len());
    desired
        .iter()
        .any(|d| !seen.insert(d.clip.id.as_str()) || !modes_are_canonical(&d.modes))
}

/// Whether a mode list is already in the canonical, deduplicated order that the
/// owned merge would produce (`[Mirror]`, `[Copy]`, `[Mirror, Copy]`, or empty).
fn modes_are_canonical(modes: &[SourceMode]) -> bool {
    matches!(
        modes,
        [] | [SourceMode::Mirror | SourceMode::Copy] | [SourceMode::Mirror, SourceMode::Copy]
    )
}

/// A deterministic, order-independent sort key for choosing the representative
/// non-safety fields when aggregating duplicate desired entries.
fn rep_key(d: &Desired) -> (&str, &str, &str, u8) {
    let format = match d.format {
        AudioFormat::Mp3 => 0,
        AudioFormat::Flac => 1,
        AudioFormat::Wav => 2,
        AudioFormat::Alac => 3,
    };
    (
        d.path.as_str(),
        d.meta_hash.as_str(),
        d.art_hash.as_str(),
        format,
    )
}

/// Downgrade any delete whose path is also written or relocated to by a
/// `Download`, `Reformat`, `Rename`, `WriteArtifact`, `WriteStem`,
/// `MoveArtifact`, or `MoveStem` this run, so a deletion can never clobber a
/// file the same plan just produced. This covers the audio [`Action::Delete`],
/// every artifact [`Action::DeleteArtifact`] class, and every
/// [`Action::DeleteStem`].
///
/// Paths are compared by their filesystem-canonical key (NFC + lowercase, see
/// [`canonical_path_key`]), not byte-exact, so a departed clip's delete is
/// suppressed even when it differs from a kept clip's fresh write target only by
/// letter case or Unicode normalisation. On a case-insensitive or NFC-folding
/// filesystem those name the same file, and a byte-exact match would miss the
/// alias and delete the file the run just wrote.
fn suppress_path_aliasing(actions: &mut [Action]) {
    // Collect the delete indices whose path a write or move also targets this
    // run. Only aliased deletes are rewritten below, so the common (no-alias)
    // case does no extra work beyond building the canonical target set.
    let aliased: Vec<usize> = {
        let targets: BTreeSet<String> = actions
            .iter()
            .filter_map(|a| match a {
                Action::Download { path, .. }
                | Action::Reformat { path, .. }
                | Action::WriteArtifact { path, .. }
                | Action::WriteStem { path, .. } => Some(path.as_str()),
                Action::Rename { to, .. }
                | Action::MoveArtifact { to, .. }
                | Action::MoveStem { to, .. } => Some(to.as_str()),
                _ => None,
            })
            .map(canonical_path_key)
            .collect();
        actions
            .iter()
            .enumerate()
            .filter_map(|(index, a)| match a {
                Action::Delete { path, .. }
                | Action::DeleteArtifact { path, .. }
                | Action::DeleteStem { path, .. } => {
                    targets.contains(&canonical_path_key(path)).then_some(index)
                }
                _ => None,
            })
            .collect()
    };
    for index in aliased {
        actions[index] = match &actions[index] {
            Action::Delete { clip_id, .. } | Action::DeleteStem { clip_id, .. } => Action::Skip {
                clip_id: clip_id.clone(),
            },
            Action::DeleteArtifact { owner_id, .. } => Action::Skip {
                clip_id: owner_id.clone(),
            },
            _ => unreachable!("only delete actions are collected as aliased"),
        };
    }
}

/// Refuse any write, rename, or move whose destination is a path a *different*
/// clip's file currently holds, so a run can never destroy a file the deletion
/// gate protects.
///
/// [`plan_desired`] emits a `Rename`/`Download`/`Reformat` (and the artifact and
/// stem planners a `Move`/`Write`) at each clip's freshly rendered path without
/// checking that another clip does not already occupy it. A naming collision, a
/// narrowed-out twin still on disk, or two clips swapping names can therefore
/// point a replacing write at a kept clip's file, destroying it with no `Delete`
/// in the plan at all: a delete via the write vector, bypassing every deletion
/// guard. This final pass downgrades such an action to a `Skip`. Losing a move
/// is recoverable and self-heals once the occupant moves away on a later run,
/// whereas overwriting the file is not; a genuine two-clip name swap stays
/// safely unmoved rather than clobbering.
///
/// Two occupants are *not* clobbers. A clip writing onto its own tracked path (a
/// re-download, a retag, or a case- or normalisation-only self-rename) is fine,
/// so the guard keys on the owning clip id, not the bare path. And a path whose
/// occupant is being deleted this run is a safe hand-off: the delete is atomic-
/// replaced by the write, or suppressed alongside it by
/// [`suppress_path_aliasing`], so this pass runs first, while the deletes are
/// still visible. A rename-*away* does not count, because plan order cannot
/// guarantee the source moves before the destination is filled.
fn suppress_target_clobber(actions: &mut [Action], manifest: &Manifest) {
    // The clip that currently owns each on-disk path, by filesystem-canonical
    // key (NFC + lowercase), across audio and every sidecar and stem.
    let mut owner: HashMap<String, String> = HashMap::new();
    for (clip_id, entry) in manifest.iter() {
        owner
            .entry(canonical_path_key(&entry.path))
            .or_insert_with(|| clip_id.clone());
        for path in entry.artifact_paths() {
            owner
                .entry(canonical_path_key(path))
                .or_insert_with(|| clip_id.clone());
        }
    }

    // Paths a delete frees this run: a write there is a safe hand-off, not a
    // clobber. Only deletes count, never a rename source, whose move plan order
    // cannot guarantee lands before the destination is filled.
    let vacated: HashSet<String> = actions
        .iter()
        .filter_map(|a| match a {
            Action::Delete { path, .. }
            | Action::DeleteArtifact { path, .. }
            | Action::DeleteStem { path, .. } => Some(canonical_path_key(path)),
            _ => None,
        })
        .collect();

    for action in actions.iter_mut() {
        let Some((target_key, acting)) = clobber_probe(action, &owner) else {
            continue;
        };
        if vacated.contains(&target_key) {
            continue;
        }
        let occupant = owner.get(&target_key);
        let clobbers = match (occupant, acting.as_deref()) {
            (Some(held_by), Some(actor)) => held_by != actor,
            // The destination is held but the actor is unknown: refuse it too.
            (Some(_), None) => true,
            (None, _) => false,
        };
        if clobbers {
            let clip_id = acting.or_else(|| occupant.cloned()).unwrap_or_default();
            *action = Action::Skip { clip_id };
        }
    }
}

/// The canonical destination path key and the acting clip id for a write-class
/// action, or `None` for an action that occupies no new path (a delete, a skip,
/// or an in-place retag). A `Rename` carries no clip id, so its actor is whoever
/// owns the `from` path in the manifest-derived `owner` map.
fn clobber_probe(
    action: &Action,
    owner: &HashMap<String, String>,
) -> Option<(String, Option<String>)> {
    let (target, acting): (&str, Option<String>) = match action {
        Action::Download { clip, path, .. } | Action::Reformat { clip, path, .. } => {
            (path, Some(clip.id.clone()))
        }
        Action::WriteArtifact { path, owner_id, .. } => (path, Some(owner_id.clone())),
        Action::WriteStem { path, clip_id, .. } => (path, Some(clip_id.clone())),
        Action::MoveArtifact { to, owner_id, .. } => (to, Some(owner_id.clone())),
        Action::MoveStem { to, clip_id, .. } => (to, Some(clip_id.clone())),
        Action::Rename { from, to } => (to, owner.get(&canonical_path_key(from)).cloned()),
        _ => return None,
    };
    Some((canonical_path_key(target), acting))
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

    if !same_fs_path(&d.path, &entry.path) {
        out.push(Action::Rename {
            from: entry.path.clone(),
            to: d.path.clone(),
        });
        // A rename still needs a retag when the metadata or art drifted.
        if meta_art_or_lyrics_changed(d, entry) {
            out.push(Action::Retag {
                clip: d.clip.clone(),
                lineage: d.lineage.clone(),
                path: d.path.clone(),
            });
        }
        return;
    }

    if meta_art_or_lyrics_changed(d, entry) {
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

/// Whether any tag-bearing input differs from the manifest: metadata, cover art,
/// or the embedded aligned lyrics. Each has its own sentinel, so a drift in one
/// re-tags without depending on the others (the lyrics clause back-fills Suno's
/// fetched alignment into the tag, #354).
fn meta_art_or_lyrics_changed(d: &Desired, entry: &ManifestEntry) -> bool {
    d.meta_hash != entry.meta_hash
        || d.art_hash != entry.art_hash
        || d.embedded_lyrics_hash != entry.embedded_lyrics_hash
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod proptests;
