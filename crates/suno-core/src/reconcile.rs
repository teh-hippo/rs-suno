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
use std::collections::HashSet;

use crate::album_art::{AlbumArt, PlaylistState};
use crate::hash::{art_hash, art_url_hash, webp_art_hash};
use crate::lineage::LineageContext;
use crate::manifest::{ArtifactState, Manifest, ManifestEntry};
use crate::model::Clip;
use crate::pathkey::{canonical_path_key, same_fs_path};
use crate::vocab::{ArtifactKind, AudioFormat, SourceMode, StemFormat, WebpEncodeSettings};

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
    /// The clip's desired external artifacts (cover.jpg, cover.webp, ...).
    ///
    /// This is the authoritative target set of sidecars for the clip: an
    /// artifact present here is written when missing or changed, and a manifest
    /// artifact absent here is a removed kind and reconciled for deletion. It
    /// defaults to empty; later phases populate it (P7 covers per-song art), so
    /// for now every production caller passes an empty vec and only tests set it.
    pub artifacts: Vec<DesiredArtifact>,
    /// The clip's desired stem set, when stems are being mirrored.
    ///
    /// Tri-state, encoding stem deletion safety:
    /// - `None` — the stem listing is not authoritative this run (the feature is
    ///   off, `has_stem` is false/absent, or the listing was disabled, failed,
    ///   partial, `400`, or otherwise indeterminate). Existing local stems are
    ///   KEPT and never deleted; a paging error is never read as "no stems".
    /// - `Some(set)` — an AUTHORITATIVE, fully enumerated set. Stems missing from
    ///   it are written, drifted ones rewritten, and a tracked stem absent from
    ///   it is delete-reconciled through the shared deletion gate.
    ///
    /// Defaults to `None`, so any caller that does not mirror stems leaves local
    /// stems untouched.
    pub stems: Option<Vec<DesiredStem>>,
}

/// One desired stem for a clip.
///
/// Carries the stable per-stem key (the manifest map key), where the stem file
/// should live, where to fetch it, and a source change hash that drives rewrite
/// detection against the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredStem {
    /// The stable key for this stem (server stem id, else label), unique within
    /// the clip. This is the manifest map key, so add/rewrite/remove target the
    /// right stem without disturbing the others.
    pub key: String,
    /// The stem's own server clip id, used to render its lossless WAV through the
    /// free `convert_wav` flow. Empty only for a degenerate listing with no id,
    /// in which case the stem is stored as MP3 (WAV needs an id to render).
    pub stem_id: String,
    /// Resolved relative target path for the stem file, inside the song's
    /// `.stems` sub-folder. Its extension matches [`format`](Self::format).
    pub path: String,
    /// The public CDN MP3 URL for the stem (a free GET). Downloaded directly for
    /// an MP3 stem; for a WAV stem it is the source-of-truth for the rewrite
    /// hash while the bytes come from the rendered WAV.
    pub source_url: String,
    /// The container the stem is stored in (WAV by default, or MP3). Stems are
    /// always stored RAW; this is never FLAC.
    pub format: StemFormat,
    /// Source change hash; a change from the manifest triggers a rewrite.
    pub hash: String,
}

/// One desired external artifact for a clip.
///
/// Carries where the sidecar should live, where to fetch it, and the content or
/// source change hash that drives rewrite detection against the manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct DesiredArtifact {
    /// Which artifact class this is.
    pub kind: ArtifactKind,
    /// Resolved relative target path for the sidecar.
    pub path: String,
    /// The URL the sidecar's bytes are fetched from. Empty for a generated
    /// artifact that carries its body inline via `content`.
    pub source_url: String,
    /// Content/source change hash; a change from the manifest triggers a write.
    pub hash: String,
    /// Inline body for a *generated* artifact (the text sidecars). When `Some`,
    /// the executor writes these exact bytes and never touches the network;
    /// fetched artifacts (covers) leave it `None`.
    pub content: Option<String>,
}

/// The desired folder-art target for one album (one stable root id).
///
/// Folder art is album-scoped, so it is reconciled against the album store
/// ([`AlbumArt`]) rather than the per-clip manifest. Each present kind carries a
/// [`DesiredArtifact`] whose `hash` is the *content* hash of the chosen art, not
/// the source clip id: a most-played flip that yields the same art content is a
/// no-op (HARDENING H1). A `None` kind means the album desires no art of that
/// kind this run (no art-bearing clip, no animated source, or the feature is
/// off), which delete-reconciles any stored art of that kind under the shared
/// deletion gate.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumDesired {
    /// The album's stable key: the resolved root ancestor id (HARDENING H2).
    pub root_id: String,
    /// The desired static `folder.jpg`, from the most-played art-bearing variant.
    pub folder_jpg: Option<DesiredArtifact>,
    /// The desired animated `cover.webp`, from the first-created animated variant.
    pub folder_webp: Option<DesiredArtifact>,
    /// The desired raw `cover.mp4`: the same variant's `video_cover_url` kept
    /// verbatim (no transcode). `None` unless raw cover retention is enabled.
    pub folder_mp4: Option<DesiredArtifact>,
}

/// The desired `.m3u8` target for one playlist (a Suno playlist, or the
/// synthetic liked feed).
///
/// A playlist's body is *generated* from this run's rendered audio paths, not
/// fetched, so it is reconciled by a single content [`hash`](Self::hash) over
/// the full rendered text (HARDENING B1: the name, member order, and every
/// member's path/title/duration feed it). The rendered body is carried inline
/// in [`content`](Self::content) so the executor writes it without a network
/// round-trip. [`path`](Self::path) is `<sanitised name>.m3u8` at the library
/// root, tracked so a rename removes the stale file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistDesired {
    /// The playlist's stable key: its Suno id (the synthetic `"liked"` id for
    /// the liked feed).
    pub id: String,
    /// The playlist's display name, as shown on Suno.
    pub name: String,
    /// The `.m3u8` file's library-relative path (`<sanitised name>.m3u8`).
    pub path: String,
    /// The fully rendered `.m3u8` body, written inline (no fetch).
    pub content: String,
    /// The content hash over `content`, driving rewrite detection.
    pub hash: String,
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
    /// Write (or rewrite) an external sidecar artifact for its owning clip.
    ///
    /// Emitted when the manifest lacks the artifact or its stored hash differs
    /// from `hash`. A write is additive and never gated by deletion safety.
    ///
    /// `content` carries an inline body for *generated* artifacts (playlists):
    /// when `Some`, the executor writes those exact bytes atomically and skips
    /// the network entirely; when `None`, it fetches (and transcodes) from
    /// `source_url` as before. A fetched artifact leaves `source_url` set and
    /// `content` `None`; a generated one leaves `source_url` empty and `content`
    /// `Some`.
    WriteArtifact {
        kind: ArtifactKind,
        path: String,
        source_url: String,
        hash: String,
        owner_id: String,
        content: Option<String>,
    },
    /// Relocate a fetched sidecar from `from` to `to` without re-fetching, when
    /// only its path drifts (a retitle) and its content hash is unchanged.
    ///
    /// The executor renames the existing file (a local move), so a retitle no
    /// longer re-downloads a cover or re-transcodes an animated WebP. If the old
    /// file has vanished by commit time, or the rename fails, the executor falls
    /// back to the ordinary fetch-and-write at `to` using `source_url`. Never
    /// emitted for inline-content kinds (playlists, text), where a rewrite from
    /// the in-hand bytes is already free.
    MoveArtifact {
        kind: ArtifactKind,
        from: String,
        to: String,
        source_url: String,
        hash: String,
        owner_id: String,
    },
    /// Delete an external sidecar artifact (a removed kind, or a co-deleted
    /// sidecar of a clip whose audio is being deleted).
    ///
    /// Only ever emitted through [`delete_artifact_action`], which shares the
    /// audio `can_delete` gate and the owning entry's `preserve` marker, so a
    /// sidecar is never removed on an incomplete listing or for a preserved clip.
    DeleteArtifact {
        kind: ArtifactKind,
        path: String,
        owner_id: String,
    },
    /// Write (or rewrite) one stem file for its owning clip.
    ///
    /// Emitted when the clip's manifest stem map lacks this `key`, or its stored
    /// hash or path drifts (the song moved, or the stem format changed). A write
    /// is additive and never gated by deletion safety. Stems are stored RAW in
    /// their native container and never transcoded to FLAC: a `Wav` stem is
    /// rendered through the free `convert_wav` flow keyed on `stem_id`, an `Mp3`
    /// stem is fetched straight from `source_url`. `key` is the stable stem key,
    /// so the executor updates the right slot in the clip's keyed stem map.
    WriteStem {
        clip_id: String,
        key: String,
        stem_id: String,
        path: String,
        source_url: String,
        format: StemFormat,
        hash: String,
    },
    /// Relocate a stem file from `from` to `to` without re-rendering, when only
    /// its path drifts (a retitle) and its content hash is unchanged.
    ///
    /// The executor renames the existing file, so a retitle no longer re-renders
    /// a WAV stem through `convert_wav` or re-fetches an MP3 stem. If the old
    /// file has vanished by commit time, or the rename fails, the executor falls
    /// back to the ordinary fetch-and-write at `to`.
    MoveStem {
        clip_id: String,
        key: String,
        stem_id: String,
        from: String,
        to: String,
        source_url: String,
        format: StemFormat,
        hash: String,
    },
    /// Delete one stem file and clear its slot in the clip's keyed stem map.
    ///
    /// Only ever emitted through [`delete_stem_action`], which shares the audio
    /// `can_delete` gate and the owning entry's `preserve` marker, so a stem is
    /// never removed on an incomplete listing or for a preserved clip. Emitted
    /// either when an AUTHORITATIVE stem listing no longer contains `key`, or as
    /// a co-delete when the owning clip's audio is deleted (so the `.stems`
    /// folder is never orphaned).
    DeleteStem {
        clip_id: String,
        key: String,
        path: String,
    },
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

    /// Number of [`Action::WriteArtifact`] actions.
    pub fn artifact_writes(&self) -> usize {
        self.count(|a| matches!(a, Action::WriteArtifact { .. }))
    }

    /// Number of [`Action::DeleteArtifact`] actions.
    pub fn artifact_deletes(&self) -> usize {
        self.count(|a| matches!(a, Action::DeleteArtifact { .. }))
    }

    /// Number of [`Action::WriteStem`] actions.
    pub fn stem_writes(&self) -> usize {
        self.count(|a| matches!(a, Action::WriteStem { .. }))
    }

    /// Number of [`Action::MoveArtifact`] actions (a sidecar relocated without a
    /// re-fetch).
    pub fn artifact_moves(&self) -> usize {
        self.count(|a| matches!(a, Action::MoveArtifact { .. }))
    }

    /// Number of [`Action::MoveStem`] actions (a stem relocated without a
    /// re-render).
    pub fn stem_moves(&self) -> usize {
        self.count(|a| matches!(a, Action::MoveStem { .. }))
    }

    /// Number of [`Action::DeleteStem`] actions.
    pub fn stem_deletes(&self) -> usize {
        self.count(|a| matches!(a, Action::DeleteStem { .. }))
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
        // desired artifacts are written and any removed kind reconciled.
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

/// The single gate every `DeleteArtifact` passes through.
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
    if !can_delete {
        return None;
    }
    let entry = manifest.get(owner_id)?;
    if path.is_empty() || entry.preserve {
        return None;
    }
    Some(Action::DeleteArtifact {
        kind,
        path: path.to_string(),
        owner_id: owner_id.to_string(),
    })
}

/// Whether an artifact kind is a per-clip sidecar reconciled per clip.
///
/// The per-clip sidecars (cover art, details, lyrics, `.lrc`, video) live on the
/// manifest entry; album/library classes (folder art, playlists) are owned by
/// later phases and reconciled elsewhere, so per-clip planning ignores them.
fn is_per_clip_kind(kind: ArtifactKind) -> bool {
    matches!(
        kind,
        ArtifactKind::CoverJpg
            | ArtifactKind::CoverWebp
            | ArtifactKind::DetailsTxt
            | ArtifactKind::LyricsTxt
            | ArtifactKind::Lrc
            | ArtifactKind::VideoMp4
    )
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

/// The manifest slot for a per-clip artifact kind, if that kind is stored on the
/// entry. Album/library classes have no per-clip slot yet, so they map to
/// `None`; the match stays generic so later phases can add slots without
/// touching callers.
fn manifest_artifact_by_kind(entry: &ManifestEntry, kind: ArtifactKind) -> Option<&ArtifactState> {
    match kind {
        ArtifactKind::CoverJpg => entry.cover_jpg.as_ref(),
        ArtifactKind::CoverWebp => entry.cover_webp.as_ref(),
        ArtifactKind::DetailsTxt => entry.details_txt.as_ref(),
        ArtifactKind::LyricsTxt => entry.lyrics_txt.as_ref(),
        ArtifactKind::Lrc => entry.lrc.as_ref(),
        ArtifactKind::VideoMp4 => entry.video_mp4.as_ref(),
        ArtifactKind::FolderJpg
        | ArtifactKind::FolderWebp
        | ArtifactKind::FolderMp4
        | ArtifactKind::Playlist => None,
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
        if !is_per_clip_kind(artifact.kind) {
            continue;
        }
        // A write is needed when the manifest lacks the sidecar, its bytes drift
        // (hash), the clip moved so the sidecar belongs at a new path, or the
        // tracked file is absent (or empty) on disk. A pure relocation (same
        // bytes, new path, old file present) is emitted as a MoveArtifact below,
        // which renames rather than re-fetching (#141).
        let state = entry.and_then(|e| manifest_artifact_by_kind(e, artifact.kind));
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
            .filter(|a| is_per_clip_kind(a.kind))
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
    if !can_delete {
        return None;
    }
    let entry = manifest.get(clip_id)?;
    if path.is_empty() || entry.preserve {
        return None;
    }
    Some(Action::DeleteStem {
        clip_id: clip_id.to_string(),
        key: key.to_string(),
        path: path.to_string(),
    })
}

/// Reconcile the keyed stems of a clip whose audio is kept this run.
///
/// Does nothing when `d.stems` is `None` (the listing was not authoritative:
/// feature off, `has_stem` false, or a disabled/failed/partial/`400` listing),
/// so existing local stems are always KEPT — a paging error is never read as
/// "no stems". When `d.stems` is `Some(set)`, the set is authoritative:
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
    let Some(desired_stems) = &d.stems else {
        return;
    };
    let clip_id = d.clip.id.as_str();
    let entry = manifest.get(clip_id);

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
        [] | [SourceMode::Mirror] | [SourceMode::Copy] | [SourceMode::Mirror, SourceMode::Copy]
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

/// Derive the desired folder art for every album in `desired`, grouped by the
/// stable root id (HARDENING H2).
///
/// This is pure: it groups the selected clips by their resolved `root_id`, then
/// per album chooses the folder-art sources deterministically:
///
/// - `folder.jpg` comes from the MOST-PLAYED art-bearing variant; ties break to
///   the EARLIEST `created_at`, then the lexicographically smallest id. Its hash
///   is the chosen art's content hash ([`art_hash`]), so a most-played flip to a
///   variant sharing the same art is a no-op downstream (H1).
/// - `cover.webp` (only when `animated_covers` is set) comes from the
///   EARLIEST-created variant with a non-empty `video_cover_url`; ties break to
///   the smallest id. Its hash folds in the `webp` encode settings, so changing
///   quality/lossless/effort re-transcodes it. `None` when no variant has an
///   animated source.
/// - `cover.mp4` (only when `raw_cover` is set) is that same variant's
///   `video_cover_url` kept verbatim (no transcode), so `both` yields the raw
///   source beside its WebP re-encode. `None` when no variant has an animated
///   source.
///
/// The album folder is the common parent of the album's clips' audio paths (they
/// share `{creator}/{album}/`); `folder.jpg` lands at `{album_dir}/folder.jpg`
/// and the animated covers at `{album_dir}/cover.webp` / `{album_dir}/cover.mp4`.
pub fn album_desired(
    desired: &[Desired],
    animated_covers: bool,
    raw_cover: bool,
    webp: WebpEncodeSettings,
) -> Vec<AlbumDesired> {
    let mut groups: BTreeMap<&str, Vec<&Desired>> = BTreeMap::new();
    for d in desired {
        groups
            .entry(d.lineage.root_id.as_str())
            .or_default()
            .push(d);
    }

    groups
        .into_iter()
        .map(|(root_id, members)| {
            let album_dir = album_dir_of(&members);
            let folder_jpg = folder_jpg_source(&members).map(|source| DesiredArtifact {
                kind: ArtifactKind::FolderJpg,
                path: album_child(&album_dir, "folder.jpg"),
                source_url: source.clip.selected_image_url().unwrap_or("").to_owned(),
                hash: art_hash(&source.clip),
                content: None,
            });
            let folder_webp = animated_covers
                .then(|| folder_webp_source(&members))
                .flatten()
                .map(|source| DesiredArtifact {
                    kind: ArtifactKind::FolderWebp,
                    path: album_child(&album_dir, "cover.webp"),
                    source_url: source.clip.video_cover_url.clone(),
                    hash: webp_art_hash(&source.clip.video_cover_url, &webp),
                    content: None,
                });
            let folder_mp4 = raw_cover
                .then(|| folder_webp_source(&members))
                .flatten()
                .map(|source| DesiredArtifact {
                    kind: ArtifactKind::FolderMp4,
                    path: album_child(&album_dir, "cover.mp4"),
                    source_url: source.clip.video_cover_url.clone(),
                    hash: art_url_hash(&source.clip.video_cover_url),
                    content: None,
                });
            AlbumDesired {
                root_id: root_id.to_owned(),
                folder_jpg,
                folder_webp,
                folder_mp4,
            }
        })
        .collect()
}

/// The album folder: the common parent of the members' audio paths.
///
/// The album's clips share `{creator}/{album}/`, so any member's parent is the
/// album dir; the smallest is taken so a stray differing path stays deterministic.
fn album_dir_of(members: &[&Desired]) -> String {
    members
        .iter()
        .map(|d| parent_dir(&d.path))
        .min()
        .unwrap_or("")
        .to_owned()
}

/// The most-played art-bearing variant: the `folder.jpg` source.
///
/// Filtered to variants that carry selectable art, then the winner MAXIMISES
/// `play_count`, breaking ties to the EARLIEST `created_at` and then the
/// lexicographically smallest id, so selection is fully deterministic.
fn folder_jpg_source<'a>(members: &[&'a Desired]) -> Option<&'a Desired> {
    members
        .iter()
        .copied()
        .filter(|d| {
            d.clip
                .selected_image_url()
                .is_some_and(|url| !url.is_empty())
        })
        .min_by(|a, b| {
            b.clip
                .play_count
                .cmp(&a.clip.play_count)
                .then_with(|| a.clip.created_at.cmp(&b.clip.created_at))
                .then_with(|| a.clip.id.cmp(&b.clip.id))
        })
}

/// The first-created animated variant: the `cover.webp` source.
///
/// Filtered to variants with a non-empty `video_cover_url`, then the winner is
/// the EARLIEST `created_at`, tie-broken by the smallest id for determinism.
fn folder_webp_source<'a>(members: &[&'a Desired]) -> Option<&'a Desired> {
    members
        .iter()
        .copied()
        .filter(|d| !d.clip.video_cover_url.is_empty())
        .min_by(|a, b| {
            a.clip
                .created_at
                .cmp(&b.clip.created_at)
                .then_with(|| a.clip.id.cmp(&b.clip.id))
        })
}

/// The parent directory of a forward-slash relative path, or `""` at the root.
fn parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// Join an album dir and a file name with a forward slash, tolerating an empty
/// dir (a path at the account root).
fn album_child(album_dir: &str, name: &str) -> String {
    if album_dir.is_empty() {
        name.to_owned()
    } else {
        format!("{album_dir}/{name}")
    }
}

/// Plan the folder-art writes and deletes for this run's albums.
///
/// Writes are keyed on the CHOSEN ART CONTENT HASH (and the target path), never
/// the source clip id: for each present desired kind, a [`Action::WriteArtifact`]
/// is emitted only when the album store lacks that kind, its stored hash differs,
/// its stored path differs, or the tracked file is absent (or empty) on disk.
/// When hash, path, and disk presence all match, nothing is written, so a
/// most-played flip that resolves to the same art content is a no-op
/// (HARDENING H1). Exactly one write can be emitted per album per kind.
///
/// `local` is a path-keyed probe map built by the caller. A stored path that
/// resolves to a missing or zero-size file forces `needs_write = true`.  A path
/// absent from `local` (probe unavailable) falls back to hash/path comparison.
///
/// Deletes cover any stored album/kind no longer desired — the album emptied (no
/// selected clips root there this run) or the kind's source disappeared (no
/// art-bearing or animated variant). Each is emitted only when `can_delete` (the
/// shared [`deletion_allowed`] verdict), so folder art is never removed on an
/// empty, failed, partial, or truncated listing. Folder art has no preserve
/// concept; the `can_delete` gate is the guard.
///
/// The output is deterministic: actions are sorted by `(root_id, kind)`, and a
/// given `(root_id, kind)` yields at most one action (a write or a delete).
pub fn plan_album_artifacts(
    desired: &[AlbumDesired],
    albums: &BTreeMap<String, AlbumArt>,
    can_delete: bool,
    local: &HashMap<String, LocalFile>,
) -> Vec<Action> {
    let mut actions: Vec<Action> = Vec::new();
    let by_root: BTreeMap<&str, &AlbumDesired> =
        desired.iter().map(|d| (d.root_id.as_str(), d)).collect();

    for d in desired {
        let stored = albums.get(&d.root_id);
        for artifact in [
            d.folder_jpg.as_ref(),
            d.folder_webp.as_ref(),
            d.folder_mp4.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let needs_write = needs_write_drift(
                stored
                    .and_then(|a| a.artifact(artifact.kind))
                    .map(|state| (state.hash.as_str(), state.path.as_str())),
                artifact.hash.as_str(),
                artifact.path.as_str(),
                local,
            );
            if needs_write {
                actions.push(Action::WriteArtifact {
                    kind: artifact.kind,
                    path: artifact.path.clone(),
                    source_url: artifact.source_url.clone(),
                    hash: artifact.hash.clone(),
                    owner_id: d.root_id.clone(),
                    content: None,
                });
            }
        }
    }

    // Deletes are fully gated: nothing is removed on an unreliable listing.
    if can_delete {
        for (root_id, art) in albums {
            for (kind, state) in album_artifacts(art) {
                let desired_here = by_root
                    .get(root_id.as_str())
                    .is_some_and(|d| album_desires_kind(d, kind));
                if !desired_here && !state.path.is_empty() {
                    actions.push(Action::DeleteArtifact {
                        kind,
                        path: state.path.clone(),
                        owner_id: root_id.clone(),
                    });
                }
            }
        }
    }

    actions.sort_by(|a, b| album_action_key(a).cmp(&album_action_key(b)));
    actions
}

/// The folder-art artifacts an album currently stores, paired with their kind,
/// in a stable order.
fn album_artifacts(art: &AlbumArt) -> Vec<(ArtifactKind, &ArtifactState)> {
    let mut out = Vec::new();
    if let Some(state) = &art.folder_jpg {
        out.push((ArtifactKind::FolderJpg, state));
    }
    if let Some(state) = &art.folder_webp {
        out.push((ArtifactKind::FolderWebp, state));
    }
    if let Some(state) = &art.folder_mp4 {
        out.push((ArtifactKind::FolderMp4, state));
    }
    out
}

/// Whether an [`AlbumDesired`] desires the given folder-art kind this run.
fn album_desires_kind(d: &AlbumDesired, kind: ArtifactKind) -> bool {
    match kind {
        ArtifactKind::FolderJpg => d.folder_jpg.is_some(),
        ArtifactKind::FolderWebp => d.folder_webp.is_some(),
        ArtifactKind::FolderMp4 => d.folder_mp4.is_some(),
        ArtifactKind::CoverJpg
        | ArtifactKind::CoverWebp
        | ArtifactKind::DetailsTxt
        | ArtifactKind::LyricsTxt
        | ArtifactKind::Lrc
        | ArtifactKind::VideoMp4
        | ArtifactKind::Playlist => false,
    }
}

/// The `(root_id, kind)` sort key for a folder-art action, for deterministic order.
fn album_action_key(action: &Action) -> (&str, ArtifactKind) {
    match action {
        Action::WriteArtifact { owner_id, kind, .. }
        | Action::DeleteArtifact { owner_id, kind, .. } => (owner_id.as_str(), *kind),
        _ => ("", ArtifactKind::CoverJpg),
    }
}

/// Plan the `.m3u8` writes and deletes for this run's playlists.
///
/// # Writes
///
/// For each desired playlist a single [`Action::WriteArtifact`] of kind
/// [`Playlist`](ArtifactKind::Playlist) is emitted (carrying the rendered body
/// inline in `content`) when the store lacks the playlist, its stored hash
/// differs, its stored path differs, or the tracked file is absent (or empty)
/// on disk. The hash is taken over the full rendered text, so a name, order,
/// path, title, or duration change all trigger a rewrite (HARDENING B1); an
/// unchanged, present playlist writes nothing (idempotent).
///
/// `local` is a path-keyed probe map built by the caller. A stored path that
/// resolves to a missing or zero-size file forces `needs_write = true`.  A path
/// absent from `local` (probe unavailable) falls back to hash/path comparison.
///
/// A **rename** (the same id whose sanitised name, and so path, changed) writes
/// the new file and, gated exactly like a stale delete (`can_delete &&
/// list_fully_enumerated`), also deletes the old stored path so the previous
/// `<oldname>.m3u8` does not linger.
///
/// # Deletes (HARDENING B2 — paramount)
///
/// A stored playlist absent from `desired` is stale (removed on Suno) and its
/// file is deleted **only** when `can_delete` AND `list_fully_enumerated`. The
/// second gate is the playlist-specific safety valve: `list_fully_enumerated`
/// is `true` only when the `/api/playlist/me` listing succeeded and was fully
/// paginated. If that listing **failed or was not fully enumerated**, the caller
/// passes `list_fully_enumerated = false` (and an empty `desired`), so this
/// function emits **zero deletes and zero writes** and every existing `.m3u8` is
/// left untouched. A failed *member* fetch for one playlist is handled upstream
/// by excluding that id from BOTH `desired` and `stored`, so it is never treated
/// as stale here.
///
/// The output is deterministic (sorted by `(owner_id, kind)`) and self-suppresses
/// path aliasing, so a rename to a name another playlist also renders this run
/// downgrades the colliding delete rather than removing a just-written file.
pub fn plan_playlist_artifacts(
    desired: &[PlaylistDesired],
    stored: &BTreeMap<String, PlaylistState>,
    can_delete: bool,
    list_fully_enumerated: bool,
    local: &HashMap<String, LocalFile>,
) -> Vec<Action> {
    let mut actions: Vec<Action> = Vec::new();
    let desired_ids: BTreeSet<&str> = desired.iter().map(|d| d.id.as_str()).collect();
    // Deletes (stale removals and rename cleanups) are gated on BOTH the shared
    // deletion verdict and a fully-enumerated playlist listing (B2).
    let deletes_allowed = can_delete && list_fully_enumerated;

    for d in desired {
        let stored_here = stored.get(&d.id);
        let needs_write = needs_write_drift(
            stored_here.map(|state| (state.hash.as_str(), state.path.as_str())),
            d.hash.as_str(),
            d.path.as_str(),
            local,
        );
        if needs_write {
            actions.push(Action::WriteArtifact {
                kind: ArtifactKind::Playlist,
                path: d.path.clone(),
                source_url: String::new(),
                hash: d.hash.clone(),
                owner_id: d.id.clone(),
                content: Some(d.content.clone()),
            });
        }
        // A rename changed the path: remove the old file, under the delete gate.
        if deletes_allowed
            && let Some(state) = stored_here
            && !state.path.is_empty()
            && state.path != d.path
        {
            actions.push(Action::DeleteArtifact {
                kind: ArtifactKind::Playlist,
                path: state.path.clone(),
                owner_id: d.id.clone(),
            });
        }
    }

    // Stale playlists (removed on Suno) are deleted only under the full gate, so
    // a failed or partial listing never removes an existing `.m3u8` (B2).
    if deletes_allowed {
        for (id, state) in stored {
            if !desired_ids.contains(id.as_str()) && !state.path.is_empty() {
                actions.push(Action::DeleteArtifact {
                    kind: ArtifactKind::Playlist,
                    path: state.path.clone(),
                    owner_id: id.clone(),
                });
            }
        }
    }

    actions.sort_by(|a, b| playlist_action_key(a).cmp(&playlist_action_key(b)));
    // A rename to a name another playlist also renders this run must not delete
    // the file that write just produced; downgrade any such colliding delete.
    suppress_path_aliasing(&mut actions);
    actions
}

/// The `(owner_id, is_delete)` sort key for a playlist action, so writes and
/// deletes for one id stay adjacent and order is deterministic.
fn playlist_action_key(action: &Action) -> (&str, u8) {
    match action {
        Action::WriteArtifact { owner_id, .. } => (owner_id.as_str(), 0),
        Action::DeleteArtifact { owner_id, .. } => (owner_id.as_str(), 1),
        Action::Skip { clip_id } => (clip_id.as_str(), 2),
        _ => ("", 3),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod proptests;
