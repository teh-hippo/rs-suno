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

use crate::config::{AudioFormat, StemFormat};
use crate::graph::{AlbumArt, PlaylistState};
use crate::hash::{art_hash, art_url_hash};
use crate::lineage::LineageContext;
use crate::manifest::{ArtifactState, Manifest, ManifestEntry};
use crate::model::Clip;

/// The class of an external sidecar artifact a clip (or album/library) owns.
///
/// The reconcile engine keeps a single pair of artifact actions
/// ([`Action::WriteArtifact`] / [`Action::DeleteArtifact`]) rather than one
/// variant per class; the `kind` distinguishes them so the executor and the
/// manifest can route each to the right slot. Per-clip classes
/// ([`CoverJpg`](ArtifactKind::CoverJpg), [`CoverWebp`](ArtifactKind::CoverWebp),
/// [`DetailsTxt`](ArtifactKind::DetailsTxt), [`LyricsTxt`](ArtifactKind::LyricsTxt),
/// [`Lrc`](ArtifactKind::Lrc), and [`VideoMp4`](ArtifactKind::VideoMp4)) map to
/// a manifest entry field; the album/library classes are reconciled by later
/// phases and have no per-clip manifest slot yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtifactKind {
    /// The per-song external cover, sourced from `image_large_url`.
    CoverJpg,
    /// The per-song animated cover, derived from `video_cover_url`.
    CoverWebp,
    /// The per-song plain-text details dump (generated, inline content).
    DetailsTxt,
    /// The per-song plain-text lyrics file (generated, inline content).
    LyricsTxt,
    /// The per-song untimed `.lrc` lyrics file (generated, inline content).
    Lrc,
    /// The per-song standalone music video, fetched from `video_url` (off by
    /// default). A large binary, removed only alongside its own audio.
    VideoMp4,
    /// The album folder's static cover (album-scoped, later phase).
    FolderJpg,
    /// The album folder's animated cover (album-scoped, later phase).
    FolderWebp,
    /// The album folder's raw animated cover: the same `video_cover_url` as
    /// [`FolderWebp`](ArtifactKind::FolderWebp), kept verbatim with no transcode
    /// (album-scoped, later phase).
    FolderMp4,
    /// A library-root `.m3u8` playlist (library-scoped, later phase).
    Playlist,
}

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

/// Whether a playlist listing is authoritative for deletion, before the
/// empty-mirror guard.
///
/// A playlist area is authoritative only when its page set drained completely
/// (`complete`), no member was lost to the downloadable filter (`any_filtered`),
/// and no `--limit`/`--since` narrowing was applied (`narrowed`). A narrowed
/// playlist mirror disarms exactly as a narrowed library or liked feed does, so
/// `--limit`/`--since` never delete against the full playlist and deletion
/// always needs a full run, uniformly across every source (#148).
pub fn playlist_authoritative(complete: bool, any_filtered: bool, narrowed: bool) -> bool {
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
/// Cover art deliberately opts out: a clip's art or video-preview URL can be
/// transiently absent for a run (the feed omits it, or a fetch fails), and the
/// desired set then simply lacks that cover. Treating that absence as a removal
/// and deleting the on-disk sidecar would churn a perfectly good cover, so an
/// empty/transient URL must KEEP the existing file. A cover is therefore removed
/// only by [`co_delete_artifacts`], when the owning clip leaves every mirror
/// source and its audio is deleted (a fully gated path). The removed-kind
/// mechanism is kept intact for any future sidecar kind that genuinely wants it.
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
        | ArtifactKind::CoverWebp
        | ArtifactKind::LyricsTxt
        | ArtifactKind::Lrc
        | ArtifactKind::VideoMp4 => false,
        ArtifactKind::DetailsTxt
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
                || stored_path != want_path
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
                && state.path != artifact.path
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
            Some(state) => state.hash != stem.hash || state.path != stem.path,
        };
        if needs_write {
            // Downgrade a pure relocation to a rename: only the path drifted and
            // the bytes are unchanged, so move the raw stem rather than re-render
            // a WAV via convert_wav or re-fetch an MP3 (#141). The executor falls
            // back to a fetch-and-write if the old file has since vanished.
            if let Some(state) = state
                && state.hash == stem.hash
                && state.path != stem.path
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
fn suppress_path_aliasing(actions: &mut [Action]) {
    // Collect the delete indices whose path a write or move also targets this
    // run, borrowing the paths rather than cloning them. Only aliased deletes
    // are rewritten below, so the common (no-alias) case allocates nothing.
    let aliased: Vec<usize> = {
        let targets: BTreeSet<&str> = actions
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
            .collect();
        actions
            .iter()
            .enumerate()
            .filter_map(|(index, a)| match a {
                Action::Delete { path, .. }
                | Action::DeleteArtifact { path, .. }
                | Action::DeleteStem { path, .. } => {
                    targets.contains(path.as_str()).then_some(index)
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

// ── Folder art (album-scoped) ───────────────────────────────────────────────

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
///   the smallest id. `None` when no variant has an animated source.
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
                    hash: art_url_hash(&source.clip.video_cover_url),
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
mod tests {
    use super::*;
    use crate::hash::content_hash;

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
            ..Default::default()
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
            artifacts: Vec::new(),
            stems: None,
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
    fn bulk_album_rename_moves_and_retags_without_redownload() {
        // Renaming an album (a manual override) changes both the folder path and
        // the ALBUM tag/hash for every member clip. Reconcile must emit a Rename
        // (a filesystem move) plus an in-place Retag per clip, and NEVER a
        // Download: deletion safety holds (no Delete) and no audio is re-fetched.
        let mut manifest = Manifest::new();
        for id in ["a", "b", "c"] {
            manifest.insert(
                id,
                entry(
                    &format!("Creator/Old Album/{id}.flac"),
                    AudioFormat::Flac,
                    "old-meta",
                    "art",
                ),
            );
        }
        let d: Vec<Desired> = ["a", "b", "c"]
            .iter()
            .map(|id| {
                desired(
                    id,
                    &format!("Creator/New Album/{id}.flac"),
                    AudioFormat::Flac,
                    "new-meta",
                    "art",
                )
            })
            .collect();
        let local: HashMap<String, LocalFile> = ["a", "b", "c"]
            .iter()
            .map(|id| (id.to_string(), present(100)))
            .collect();

        let plan = reconcile(&manifest, &d, &local, &mirror_ok());

        assert_eq!(plan.renames(), 3, "every member folder move is a rename");
        assert_eq!(
            plan.retags(),
            3,
            "the album tag change retags each in place"
        );
        assert_eq!(
            plan.downloads(),
            0,
            "an album rename must never re-download"
        );
        assert_eq!(
            plan.deletes(),
            0,
            "deletion safety: a rename deletes nothing"
        );
        for id in ["a", "b", "c"] {
            assert!(plan.actions.contains(&Action::Rename {
                from: format!("Creator/Old Album/{id}.flac"),
                to: format!("Creator/New Album/{id}.flac"),
            }));
        }
    }

    #[test]
    fn mis_rooted_clip_moves_never_deletes_even_when_deletion_is_armed() {
        // Deletion safety: if a clip's resolved root changes between runs (its
        // album folder moves from {root A} to {root B}), reconcile must relocate
        // the file with a Rename, never Delete the old copy and re-download.
        // This holds with deletion fully armed (mirror_ok => can_delete), so a
        // future clip_roots-driven root shift can never arm an audio delete.
        let mut manifest = Manifest::new();
        manifest.insert(
            "child",
            entry("Creator/Root A/child.flac", AudioFormat::Flac, "m", "art"),
        );
        let d = vec![desired(
            "child",
            "Creator/Root B/child.flac",
            AudioFormat::Flac,
            "m",
            "art",
        )];
        let plan = reconcile(&manifest, &d, &local_present("child"), &mirror_ok());

        assert_eq!(
            plan.actions,
            vec![Action::Rename {
                from: "Creator/Root A/child.flac".to_string(),
                to: "Creator/Root B/child.flac".to_string(),
            }],
            "a mis-rooted clip is moved, not deleted or re-downloaded"
        );
        assert_eq!(
            plan.deletes(),
            0,
            "deletion safety: a re-root deletes nothing"
        );
        assert_eq!(plan.downloads(), 0, "a re-root never re-fetches audio");
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
    fn trashed_but_complete_clip_is_downloadable_yet_still_deletes() {
        // A trashed clip is complete and carries no excluded type or task, so it
        // passes `is_downloadable` (downloadability never screens on trashed).
        // A full run still schedules its deletion, proving the two concerns stay
        // decoupled: the download filter does not suppress the delete signal.
        let mut trashed = clip("a");
        trashed.status = "complete".to_string();
        trashed.is_trashed = true;
        assert!(crate::is_downloadable(&trashed));

        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
        d.clip = trashed;
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
    fn playlist_authoritative_requires_all_conditions() {
        // All three conditions satisfied: authoritative.
        assert!(playlist_authoritative(true, false, false));
        // Incomplete page drain: not authoritative.
        assert!(!playlist_authoritative(false, false, false));
        // A member lost to the downloadable filter: not authoritative.
        assert!(!playlist_authoritative(true, true, false));
        // Narrowed with --limit/--since: not authoritative.
        assert!(!playlist_authoritative(true, false, true));
        // Multiple conditions: any failure disarms.
        assert!(!playlist_authoritative(false, true, true));
    }

    #[test]
    fn area_fully_enumerated_applies_empty_mirror_guard() {
        // A non-empty Mirror that fully listed is authoritative.
        assert!(area_fully_enumerated(true, false, SourceMode::Mirror));
        // An empty Mirror is never authoritative (indistinguishable from a drop).
        assert!(!area_fully_enumerated(true, true, SourceMode::Mirror));
        // An empty Copy is still authoritative (it protects nothing).
        assert!(area_fully_enumerated(true, true, SourceMode::Copy));
        // A non-empty Copy is authoritative.
        assert!(area_fully_enumerated(true, false, SourceMode::Copy));
        // A non-authoritative (narrowed/incomplete) area is not enumerated regardless.
        assert!(!area_fully_enumerated(false, false, SourceMode::Mirror));
        assert!(!area_fully_enumerated(false, true, SourceMode::Copy));
    }

    #[test]
    fn narrows_downloads_only_when_no_deletion_and_no_full_library() {
        // Neither deleting nor a full library: narrowing is allowed.
        assert!(narrows_downloads(false, false));
        // Armed deletion: narrowing must not occur (D2).
        assert!(!narrows_downloads(true, false));
        // Full library listed: narrowing regresses the index.
        assert!(!narrows_downloads(false, true));
        // Both: definitely no narrowing.
        assert!(!narrows_downloads(true, true));
    }

    #[test]
    fn narrowing_never_coexists_with_deletion() {
        for can_delete in [false, true] {
            for lib_auth in [false, true] {
                assert!(
                    !(narrows_downloads(can_delete, lib_auth) && can_delete),
                    "truncate must imply !can_delete"
                );
            }
        }
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

    #[test]
    fn delete_artifact_suppressed_when_path_aliases_rename_target() {
        // A sidecar delete must never clobber a file a rename just produced this
        // run. A DeleteArtifact whose path equals a Rename's `to` is downgraded
        // to a Skip, exactly as an audio Delete is. Built directly so the
        // collision is explicit and independent of how reconcile derives it.
        let mut actions = vec![
            Action::Rename {
                from: "old/song.flac".to_string(),
                to: "new/cover.jpg".to_string(),
            },
            Action::DeleteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "new/cover.jpg".to_string(),
                owner_id: "a".to_string(),
            },
        ];
        suppress_path_aliasing(&mut actions);
        // The colliding delete is gone; only its Skip downgrade remains.
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::DeleteArtifact { .. })),
            "a sidecar delete must not alias a rename target"
        );
        assert!(actions.contains(&Action::Skip {
            clip_id: "a".to_string()
        }));
        // The rename target is untouched.
        assert!(actions.contains(&Action::Rename {
            from: "old/song.flac".to_string(),
            to: "new/cover.jpg".to_string(),
        }));
    }

    #[test]
    fn delete_artifact_suppressed_when_path_aliases_write_artifact_target() {
        // The same guard covers every write class: a DeleteArtifact colliding
        // with another artifact's WriteArtifact path is downgraded too.
        let mut actions = vec![
            Action::WriteArtifact {
                kind: ArtifactKind::FolderJpg,
                path: "creator/album/folder.jpg".to_string(),
                source_url: "https://art/large.jpg".to_string(),
                hash: "h".to_string(),
                owner_id: "root".to_string(),
                content: None,
            },
            Action::DeleteArtifact {
                kind: ArtifactKind::FolderJpg,
                path: "creator/album/folder.jpg".to_string(),
                owner_id: "root-old".to_string(),
            },
        ];
        suppress_path_aliasing(&mut actions);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::DeleteArtifact { .. }))
        );
        assert!(actions.contains(&Action::Skip {
            clip_id: "root-old".to_string()
        }));
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
                Action::WriteArtifact { owner_id, .. }
                | Action::DeleteArtifact { owner_id, .. }
                | Action::MoveArtifact { owner_id, .. } => owner_id.as_str(),
                Action::WriteStem { clip_id, .. }
                | Action::DeleteStem { clip_id, .. }
                | Action::MoveStem { clip_id, .. } => clip_id.as_str(),
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

    // ── Phase 6: artifact reconcile ─────────────────────────────────

    fn cover(path: &str, hash: &str) -> ArtifactState {
        ArtifactState {
            path: path.to_string(),
            hash: hash.to_string(),
        }
    }

    fn art(kind: ArtifactKind, path: &str, url: &str, hash: &str) -> DesiredArtifact {
        DesiredArtifact {
            kind,
            path: path.to_string(),
            source_url: url.to_string(),
            hash: hash.to_string(),
            content: None,
        }
    }

    /// A generated text sidecar desired artifact carrying its body inline.
    fn text_art(kind: ArtifactKind, path: &str, body: &str) -> DesiredArtifact {
        DesiredArtifact {
            kind,
            path: path.to_string(),
            source_url: String::new(),
            hash: content_hash(body),
            content: Some(body.to_string()),
        }
    }

    // An unchanged FLAC clip (Skip audio) that desires the given artifacts.
    fn desired_arts(id: &str, arts: Vec<DesiredArtifact>) -> Desired {
        Desired {
            artifacts: arts,
            ..desired(id, &format!("{id}.flac"), AudioFormat::Flac, "m", "art")
        }
    }

    // A manifest entry for an unchanged FLAC clip carrying a cover.jpg sidecar.
    fn entry_with_cover_jpg(id: &str, cover_path: &str, cover_hash: &str) -> ManifestEntry {
        ManifestEntry {
            cover_jpg: Some(cover(cover_path, cover_hash)),
            ..entry(&format!("{id}.flac"), AudioFormat::Flac, "m", "art")
        }
    }

    fn write_artifacts(plan: &Plan) -> Vec<&Action> {
        plan.actions
            .iter()
            .filter(|a| matches!(a, Action::WriteArtifact { .. }))
            .collect()
    }

    #[test]
    fn write_artifact_emitted_when_manifest_lacks_it() {
        // The clip's audio is unchanged (Skip), but the manifest has no cover.jpg
        // slot, so the desired sidecar is written.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "a/cover.jpg",
                "https://art/a",
                "h1",
            )],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 1);
        assert_eq!(plan.artifact_deletes(), 0);
        assert_eq!(plan.skips(), 1);
        assert_eq!(
            write_artifacts(&plan)[0],
            &Action::WriteArtifact {
                kind: ArtifactKind::CoverJpg,
                path: "a/cover.jpg".to_string(),
                source_url: "https://art/a".to_string(),
                hash: "h1".to_string(),
                owner_id: "a".to_string(),
                content: None,
            }
        );
    }

    #[test]
    fn write_artifact_emitted_when_hash_differs() {
        // The manifest already tracks a cover.jpg, but its stored hash differs
        // from the desired one, so it is rewritten (and never delete-reconciled).
        let mut manifest = Manifest::new();
        manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "old"));
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "a/cover.jpg",
                "https://art/a",
                "new",
            )],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 1);
        assert_eq!(plan.artifact_deletes(), 0);
        if let Action::WriteArtifact { hash, .. } = write_artifacts(&plan)[0] {
            assert_eq!(hash, "new");
        } else {
            panic!("expected a WriteArtifact");
        }
    }

    #[test]
    fn write_artifact_skipped_when_hash_matches() {
        // Present with a matching hash: no write, no delete.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "a/cover.jpg",
                "https://art/a",
                "h1",
            )],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 0);
        assert_eq!(plan.artifact_deletes(), 0);
        assert_eq!(
            plan.actions,
            vec![Action::Skip {
                clip_id: "a".to_string()
            }]
        );
    }

    #[test]
    fn removed_kind_cover_is_kept_not_deleted() {
        // The clip is kept but no longer desires a cover.jpg (an empty/transient
        // art URL this run). Covers opt out of removed-kind deletion, so the
        // existing sidecar is KEPT: no DeleteArtifact, no write, just a Skip.
        // This is the empty-art-URL keep the P6 review deferred to P7.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
        let d = vec![desired_arts("a", vec![])];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_deletes(), 0);
        assert_eq!(plan.artifact_writes(), 0);
        // The audio is untouched and the cover is preserved on disk.
        assert_eq!(plan.deletes(), 0);
        assert_eq!(
            plan.actions,
            vec![Action::Skip {
                clip_id: "a".to_string()
            }]
        );
        assert!(!plan.actions.iter().any(|a| matches!(
            a,
            Action::DeleteArtifact {
                kind: ArtifactKind::CoverJpg,
                ..
            }
        )));
    }

    #[test]
    fn delete_artifact_never_on_incomplete_listing() {
        // Kept clips no longer desiring their covers keep them: covers opt out of
        // removed-kind deletion. An incomplete mirror is a further backstop that
        // forbids every delete (the B2 gate on the co-delete path). Either way, a
        // large manifest of stale sidecars is safe.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
        manifest.insert("b", entry_with_cover_jpg("b", "b/cover.jpg", "h1"));
        let d = vec![desired_arts("a", vec![]), desired_arts("b", vec![])];
        let sources = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        }];
        let local: HashMap<String, LocalFile> = [
            ("a".to_string(), present(100)),
            ("b".to_string(), present(100)),
        ]
        .into_iter()
        .collect();
        let plan = reconcile(&manifest, &d, &local, &sources);
        assert_eq!(plan.artifact_deletes(), 0);
        assert_eq!(plan.deletes(), 0);
    }

    #[test]
    fn delete_artifact_never_when_entry_preserved() {
        // A kept clip that stops desiring its cover keeps it (covers opt out of
        // removed-kind deletion); the preserve marker is a further backstop.
        let mut manifest = Manifest::new();
        let preserved = ManifestEntry {
            preserve: true,
            ..entry_with_cover_jpg("a", "a/cover.jpg", "h1")
        };
        manifest.insert("a", preserved);
        let d = vec![desired_arts("a", vec![])];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_deletes(), 0);
    }

    #[test]
    fn co_delete_never_when_path_empty() {
        // The empty-path guard now matters on the co-delete path (covers opt out
        // of removed-kind deletion). An absent clip's audio is deleted, but its
        // sidecar with an empty path must never become a delete of the root.
        let mut manifest = Manifest::new();
        manifest.insert("gone", entry_with_cover_jpg("gone", "", "h1"));
        let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
        assert_eq!(plan.deletes(), 1);
        assert_eq!(plan.artifact_deletes(), 0);
    }

    #[test]
    fn co_delete_absent_clip_deletes_audio_and_cover() {
        // A clip absent from desired is deleted; its cover.jpg is co-deleted
        // under the same gate.
        let mut manifest = Manifest::new();
        manifest.insert("gone", entry_with_cover_jpg("gone", "gone/cover.jpg", "h1"));
        let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
        assert_eq!(plan.deletes(), 1);
        assert_eq!(plan.artifact_deletes(), 1);
        assert!(plan.actions.contains(&Action::Delete {
            path: "gone.flac".to_string(),
            clip_id: "gone".to_string(),
        }));
        assert!(plan.actions.contains(&Action::DeleteArtifact {
            kind: ArtifactKind::CoverJpg,
            path: "gone/cover.jpg".to_string(),
            owner_id: "gone".to_string(),
        }));
    }

    #[test]
    fn co_delete_absent_clip_suppressed_when_not_enumerated() {
        // Neither audio nor sidecar is removed on an incomplete listing.
        let mut manifest = Manifest::new();
        manifest.insert("gone", entry_with_cover_jpg("gone", "gone/cover.jpg", "h1"));
        let sources = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        }];
        let plan = reconcile(&manifest, &[], &HashMap::new(), &sources);
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.artifact_deletes(), 0);
    }

    #[test]
    fn co_delete_trashed_desired_clip_removes_audio_and_cover() {
        // A trashed clip present in desired: audio Delete plus cover co-delete.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
        let mut d = desired_arts("a", vec![]);
        d.trashed = true;
        let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
        assert_eq!(plan.deletes(), 1);
        assert_eq!(plan.artifact_deletes(), 1);
    }

    #[test]
    fn co_delete_trashed_suppressed_when_not_enumerated() {
        // The trashed co-delete obeys the same enumeration gate as the audio.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
        let mut d = desired_arts("a", vec![]);
        d.trashed = true;
        let sources = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        }];
        let plan = reconcile(&manifest, &[d], &local_present("a"), &sources);
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.artifact_deletes(), 0);
        assert_eq!(plan.skips(), 1);
    }

    #[test]
    fn co_delete_trashed_suppressed_when_preserved() {
        // A preserved, trashed clip keeps both audio and sidecar.
        let mut manifest = Manifest::new();
        let preserved = ManifestEntry {
            preserve: true,
            ..entry_with_cover_jpg("a", "a/cover.jpg", "h1")
        };
        manifest.insert("a", preserved);
        let mut d = desired_arts("a", vec![]);
        d.trashed = true;
        let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.artifact_deletes(), 0);
    }

    // ── Issue #15: per-song text sidecars ───────────────────────────

    #[test]
    fn details_sidecar_written_with_inline_content_when_slot_absent() {
        // The audio is unchanged (Skip) but no details slot exists, so the
        // generated sidecar is written and carries its body inline.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired_arts(
            "a",
            vec![text_art(
                ArtifactKind::DetailsTxt,
                "a.details.txt",
                "Title: A\n",
            )],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 1);
        assert_eq!(plan.artifact_deletes(), 0);
        assert_eq!(
            write_artifacts(&plan)[0],
            &Action::WriteArtifact {
                kind: ArtifactKind::DetailsTxt,
                path: "a.details.txt".to_string(),
                source_url: String::new(),
                hash: content_hash("Title: A\n"),
                owner_id: "a".to_string(),
                content: Some("Title: A\n".to_string()),
            }
        );
    }

    #[test]
    fn lrc_sidecar_written_with_inline_content_when_slot_absent() {
        // The audio is unchanged (Skip) but no lrc slot exists, so the generated
        // sidecar is written and carries its body inline. This is the guard that
        // the type system cannot provide: dropping Lrc from is_per_clip_kind
        // would silently never write the file, and only this test would catch it.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let body = "[re:rs-suno]\nla la\n";
        let d = vec![desired_arts(
            "a",
            vec![text_art(ArtifactKind::Lrc, "a.lrc", body)],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 1);
        assert_eq!(plan.artifact_deletes(), 0);
        assert_eq!(
            write_artifacts(&plan)[0],
            &Action::WriteArtifact {
                kind: ArtifactKind::Lrc,
                path: "a.lrc".to_string(),
                source_url: String::new(),
                hash: content_hash(body),
                owner_id: "a".to_string(),
                content: Some(body.to_string()),
            }
        );
    }

    #[test]
    fn text_sidecars_skipped_when_hash_and_path_match() {
        // Present with a matching content hash and path: no write, no delete.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.details_txt = Some(cover("a.details.txt", &content_hash("Title: A\n")));
        e.lyrics_txt = Some(cover("a.lyrics.txt", &content_hash("la la\n")));
        manifest.insert("a", e);
        let d = vec![desired_arts(
            "a",
            vec![
                text_art(ArtifactKind::DetailsTxt, "a.details.txt", "Title: A\n"),
                text_art(ArtifactKind::LyricsTxt, "a.lyrics.txt", "la la\n"),
            ],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 0);
        assert_eq!(plan.artifact_deletes(), 0);
    }

    #[test]
    fn details_rewritten_when_content_hash_differs() {
        // A title change alters the details body, so its content hash drifts and
        // the sidecar is rewritten even though the audio is otherwise unchanged.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.details_txt = Some(cover("a.details.txt", &content_hash("Title: Old\n")));
        manifest.insert("a", e);
        let d = vec![desired_arts(
            "a",
            vec![text_art(
                ArtifactKind::DetailsTxt,
                "a.details.txt",
                "Title: New\n",
            )],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 1);
        assert_eq!(plan.artifact_deletes(), 0);
    }

    #[test]
    fn lyrics_rewritten_when_content_hash_differs_though_meta_unchanged() {
        // The per-sidecar content hash keys on the rendered lyrics independently
        // of the audio's stored meta_hash, so editing the sidecar body rewrites
        // the file with no audio retag even when the meta_hash slot is unchanged.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.lyrics_txt = Some(cover("a.lyrics.txt", &content_hash("old words\n")));
        manifest.insert("a", e);
        let d = vec![desired_arts(
            "a",
            vec![text_art(
                ArtifactKind::LyricsTxt,
                "a.lyrics.txt",
                "new words\n",
            )],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        // The audio meta_hash matches ("m"), so only the sidecar rewrites.
        assert_eq!(plan.artifact_writes(), 1);
        assert_eq!(plan.retags(), 0);
    }

    #[test]
    fn text_sidecar_relocated_when_path_differs() {
        // The audio moved (rename), so the tracked details path drifts and the
        // sidecar is rewritten at the new path even though the content matches.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.details_txt = Some(cover("old/a.details.txt", &content_hash("Title: A\n")));
        manifest.insert("a", e);
        let d = vec![desired_arts(
            "a",
            vec![text_art(
                ArtifactKind::DetailsTxt,
                "new/a.details.txt",
                "Title: A\n",
            )],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 1);
        if let Action::WriteArtifact { path, .. } = write_artifacts(&plan)[0] {
            assert_eq!(path, "new/a.details.txt");
        } else {
            panic!("expected a WriteArtifact");
        }
    }

    #[test]
    fn fetched_sidecar_path_drift_emits_move() {
        // #141: a fetched cover whose bytes are unchanged but whose path drifted
        // (a retitle) is relocated with a rename rather than re-fetched.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.cover_jpg = Some(cover("old/cover.jpg", "arthash"));
        manifest.insert("a", e);
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "new/cover.jpg",
                "https://art/large.jpg",
                "arthash",
            )],
        )];
        let local: HashMap<String, LocalFile> = [
            ("a".to_string(), present(100)),
            ("old/cover.jpg".to_string(), present(50)),
        ]
        .into_iter()
        .collect();
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert_eq!(plan.artifact_moves(), 1);
        assert_eq!(plan.artifact_writes(), 0);
        assert!(plan.actions.contains(&Action::MoveArtifact {
            kind: ArtifactKind::CoverJpg,
            from: "old/cover.jpg".to_string(),
            to: "new/cover.jpg".to_string(),
            source_url: "https://art/large.jpg".to_string(),
            hash: "arthash".to_string(),
            owner_id: "a".to_string(),
        }));
    }

    #[test]
    fn sidecar_hash_drift_emits_write_not_move() {
        // Different bytes must re-fetch, even when the path also drifted.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.cover_jpg = Some(cover("old/cover.jpg", "oldhash"));
        manifest.insert("a", e);
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "new/cover.jpg",
                "https://art/large.jpg",
                "newhash",
            )],
        )];
        let local: HashMap<String, LocalFile> = [
            ("a".to_string(), present(100)),
            ("old/cover.jpg".to_string(), present(50)),
        ]
        .into_iter()
        .collect();
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert_eq!(plan.artifact_moves(), 0);
        assert_eq!(plan.artifact_writes(), 1);
    }

    #[test]
    fn inline_sidecar_path_drift_stays_a_write() {
        // Inline-content kinds (text) rewrite from the in-hand bytes, so a move
        // buys nothing: a path drift stays a WriteArtifact even at an equal hash.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.lyrics_txt = Some(cover("old/a.lyrics.txt", &content_hash("words\n")));
        manifest.insert("a", e);
        let d = vec![desired_arts(
            "a",
            vec![text_art(
                ArtifactKind::LyricsTxt,
                "new/a.lyrics.txt",
                "words\n",
            )],
        )];
        let local: HashMap<String, LocalFile> = [
            ("a".to_string(), present(100)),
            ("old/a.lyrics.txt".to_string(), present(50)),
        ]
        .into_iter()
        .collect();
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert_eq!(plan.artifact_moves(), 0);
        assert_eq!(plan.artifact_writes(), 1);
    }

    #[test]
    fn sidecar_move_downgrades_to_write_when_old_file_absent() {
        // Same bytes and a path drift, but the old file is gone: fetch fresh at
        // the new path (a self-heal), never emit a move that cannot rename.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.cover_jpg = Some(cover("old/cover.jpg", "arthash"));
        manifest.insert("a", e);
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "new/cover.jpg",
                "https://art/large.jpg",
                "arthash",
            )],
        )];
        let local: HashMap<String, LocalFile> = [
            ("a".to_string(), present(100)),
            (
                "old/cover.jpg".to_string(),
                LocalFile {
                    exists: false,
                    size: 0,
                },
            ),
        ]
        .into_iter()
        .collect();
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert_eq!(plan.artifact_moves(), 0);
        assert_eq!(plan.artifact_writes(), 1);
    }

    #[test]
    fn move_target_suppresses_a_colliding_delete() {
        // A MoveArtifact to a path another manifest entry is having deleted must
        // downgrade that delete, so relocation never clobbers the relocated file.
        let mut manifest = Manifest::new();
        let mut a = entry("a.flac", AudioFormat::Flac, "m", "art");
        a.cover_jpg = Some(cover("old/cover.jpg", "arthash"));
        manifest.insert("a", a);
        // b holds a cover at the path a is moving TO; b's cover is a removed kind
        // this run (feature toggled), so it would be delete-reconciled.
        let mut b = entry("b.flac", AudioFormat::Flac, "m", "art");
        b.details_txt = Some(cover("new/cover.jpg", "bh"));
        manifest.insert("b", b);
        let d = vec![
            desired_arts(
                "a",
                vec![art(
                    ArtifactKind::CoverJpg,
                    "new/cover.jpg",
                    "https://art/large.jpg",
                    "arthash",
                )],
            ),
            desired_arts("b", vec![]),
        ];
        let local: HashMap<String, LocalFile> = [
            ("a".to_string(), present(100)),
            ("b".to_string(), present(100)),
            ("old/cover.jpg".to_string(), present(50)),
            ("new/cover.jpg".to_string(), present(50)),
        ]
        .into_iter()
        .collect();
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert_eq!(plan.artifact_moves(), 1);
        // The colliding delete of new/cover.jpg is suppressed.
        assert!(!plan.actions.iter().any(|a| matches!(
            a,
            Action::DeleteArtifact { path, .. } if path == "new/cover.jpg"
        )));
    }

    #[test]
    fn stem_path_drift_emits_move() {
        // #141: a stem whose path drifts at an equal hash is relocated with a
        // rename rather than re-rendered or re-fetched.
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry_with_stems("a", &[("voc", "old.stems/voc.mp3", "h1")]),
        );
        let d = vec![stem_desired(
            "a",
            Some(vec![dstem("voc", "new.stems/voc.mp3", "h1")]),
        )];
        let local: HashMap<String, LocalFile> = [
            ("a".to_string(), present(100)),
            ("old.stems/voc.mp3".to_string(), present(50)),
        ]
        .into_iter()
        .collect();
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert_eq!(plan.stem_moves(), 1);
        assert_eq!(plan.stem_writes(), 0);
        assert!(plan.actions.contains(&Action::MoveStem {
            clip_id: "a".to_string(),
            key: "voc".to_string(),
            stem_id: "voc".to_string(),
            from: "old.stems/voc.mp3".to_string(),
            to: "new.stems/voc.mp3".to_string(),
            source_url: "https://cdn1.suno.ai/voc.mp3".to_string(),
            format: StemFormat::Mp3,
            hash: "h1".to_string(),
        }));
    }

    #[test]
    fn details_removed_kind_is_deleted_when_feature_off() {
        // DetailsTxt is total, so an absent desired can only mean the feature is
        // off: the stale sidecar is delete-reconciled through the shared gate.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.details_txt = Some(cover("a.details.txt", &content_hash("Title: A\n")));
        manifest.insert("a", e);
        let d = vec![desired_arts("a", vec![])];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_deletes(), 1);
        assert!(plan.actions.contains(&Action::DeleteArtifact {
            kind: ArtifactKind::DetailsTxt,
            path: "a.details.txt".to_string(),
            owner_id: "a".to_string(),
        }));
    }

    #[test]
    fn lyrics_removed_kind_is_kept_not_deleted() {
        // LyricsTxt is partial (absent could be feature-off OR a transient empty
        // lyrics read), so it opts out of removed-kind deletion cover-style: the
        // existing file is KEPT when no lyrics sidecar is desired this run.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.lyrics_txt = Some(cover("a.lyrics.txt", &content_hash("words\n")));
        manifest.insert("a", e);
        let d = vec![desired_arts("a", vec![])];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_deletes(), 0);
        assert_eq!(plan.deletes(), 0);
    }

    #[test]
    fn lrc_removed_kind_is_kept_not_deleted() {
        // Lrc is partial like LyricsTxt, so it opts out of removed-kind deletion:
        // an existing `.lrc` is KEPT when no lrc sidecar is desired this run.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.lrc = Some(cover("a.lrc", &content_hash("[re:rs-suno]\nwords\n")));
        manifest.insert("a", e);
        let d = vec![desired_arts("a", vec![])];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_deletes(), 0);
        assert_eq!(plan.deletes(), 0);
    }

    #[test]
    fn video_mp4_removed_kind_is_kept_not_deleted() {
        // VideoMp4 opts out of removed-kind deletion like a cover: a large binary
        // is never deleted merely because the video feature is off this run (or
        // the URL was transiently absent). Only a co-delete removes it.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.video_mp4 = Some(cover("a.mp4", "vid-hash"));
        manifest.insert("a", e);
        let d = vec![desired_arts("a", vec![])];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_deletes(), 0);
        assert_eq!(plan.deletes(), 0);
    }

    #[test]
    fn video_mp4_written_when_manifest_lacks_it() {
        // A desired VideoMp4 with no manifest slot is written as a fetched binary
        // (no inline content), proving the new kind flows through per-clip planning.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::VideoMp4,
                "a/song.mp4",
                "https://cdn/a/video.mp4",
                "vid-hash",
            )],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 1);
        assert_eq!(
            write_artifacts(&plan)[0],
            &Action::WriteArtifact {
                kind: ArtifactKind::VideoMp4,
                path: "a/song.mp4".to_string(),
                source_url: "https://cdn/a/video.mp4".to_string(),
                hash: "vid-hash".to_string(),
                owner_id: "a".to_string(),
                content: None,
            }
        );
    }

    #[test]
    fn details_removed_kind_not_deleted_on_incomplete_listing() {
        // The removed-kind delete still obeys the enumeration gate: an incomplete
        // mirror forbids removing the stale details sidecar.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.details_txt = Some(cover("a.details.txt", &content_hash("Title: A\n")));
        manifest.insert("a", e);
        let d = vec![desired_arts("a", vec![])];
        let sources = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        }];
        let plan = reconcile(&manifest, &d, &local_present("a"), &sources);
        assert_eq!(plan.artifact_deletes(), 0);
    }

    #[test]
    fn details_removed_kind_not_deleted_when_preserved() {
        // A preserved (private/copy-held) clip keeps its stale details sidecar
        // even when the feature is off this run.
        let mut manifest = Manifest::new();
        let mut e = ManifestEntry {
            preserve: true,
            ..entry("a.flac", AudioFormat::Flac, "m", "art")
        };
        e.details_txt = Some(cover("a.details.txt", &content_hash("Title: A\n")));
        manifest.insert("a", e);
        let d = vec![desired_arts("a", vec![])];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_deletes(), 0);
    }

    #[test]
    fn co_delete_orphan_removes_every_text_sidecar() {
        // An orphaned clip's audio is deleted; ALL its per-clip sidecars must be
        // co-deleted. This fails if `manifest_artifacts` misses a kind, which
        // would strand the file. Guards the single most important #15 wiring.
        let mut manifest = Manifest::new();
        let mut e = entry("gone.flac", AudioFormat::Flac, "m", "art");
        e.cover_jpg = Some(cover("gone/cover.jpg", "h1"));
        e.details_txt = Some(cover("gone.details.txt", &content_hash("Title: G\n")));
        e.lyrics_txt = Some(cover("gone.lyrics.txt", &content_hash("words\n")));
        e.lrc = Some(cover("gone.lrc", &content_hash("[re:rs-suno]\nwords\n")));
        e.video_mp4 = Some(cover("gone/song.mp4", "vid-hash"));
        manifest.insert("gone", e);
        let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
        assert_eq!(plan.deletes(), 1);
        assert_eq!(plan.artifact_deletes(), 5);
        for (kind, path) in [
            (ArtifactKind::CoverJpg, "gone/cover.jpg"),
            (ArtifactKind::DetailsTxt, "gone.details.txt"),
            (ArtifactKind::LyricsTxt, "gone.lyrics.txt"),
            (ArtifactKind::Lrc, "gone.lrc"),
            (ArtifactKind::VideoMp4, "gone/song.mp4"),
        ] {
            assert!(
                plan.actions.contains(&Action::DeleteArtifact {
                    kind,
                    path: path.to_string(),
                    owner_id: "gone".to_string(),
                }),
                "missing co-delete for {kind:?}"
            );
        }
    }

    #[test]
    fn co_delete_trashed_removes_every_text_sidecar() {
        // The same co-delete completeness holds on the trashed path.
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
        e.details_txt = Some(cover("a.details.txt", &content_hash("Title: A\n")));
        e.lyrics_txt = Some(cover("a.lyrics.txt", &content_hash("words\n")));
        manifest.insert("a", e);
        let mut d = desired_arts("a", vec![]);
        d.trashed = true;
        let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
        assert_eq!(plan.deletes(), 1);
        assert_eq!(plan.artifact_deletes(), 2);
    }

    #[test]
    fn suppress_downgrades_delete_artifact_colliding_with_write_artifact() {
        // Clip "a" writes a cover to the very path clip "b"'s stale cover holds;
        // deleting it would clobber the freshly written file, so it is dropped.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        manifest.insert("b", entry_with_cover_jpg("b", "shared/cover.jpg", "h1"));
        // "a" writes a new CoverJpg to the shared path; "b" is absent (its cover
        // would be co-deleted from the same path).
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "shared/cover.jpg",
                "https://art/a",
                "h2",
            )],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 1);
        // The colliding DeleteArtifact is suppressed.
        assert!(!plan.actions.iter().any(
            |a| matches!(a, Action::DeleteArtifact { path, .. } if path == "shared/cover.jpg")
        ));
        // The audio for "b" is still deleted (different path), just not its cover.
        assert!(plan.actions.contains(&Action::Delete {
            path: "b.flac".to_string(),
            clip_id: "b".to_string(),
        }));
    }

    #[test]
    fn suppress_downgrades_delete_artifact_colliding_with_download() {
        // A fresh clip downloads audio to the path an absent clip's cover holds.
        let mut manifest = Manifest::new();
        manifest.insert("b", entry_with_cover_jpg("b", "shared/x", "h1"));
        let d = vec![desired("a", "shared/x", AudioFormat::Flac, "m", "art")];
        let plan = reconcile(&manifest, &d, &HashMap::new(), &mirror_ok());
        assert_eq!(plan.downloads(), 1);
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::DeleteArtifact { path, .. } if path == "shared/x"))
        );
    }

    #[test]
    fn adding_artifacts_leaves_the_audio_plan_unchanged() {
        // SYNC-8/9/10/12 matrix invariance: the audio actions and plan.deletes()
        // are identical with and without artifacts attached. One absent clip is
        // deleted, one desired clip is kept (Skip), one trashed clip is deleted.
        let build = |with_art: bool| {
            let mut manifest = Manifest::new();
            manifest.insert("keep", entry_with_cover_jpg("keep", "keep/cover.jpg", "h1"));
            manifest.insert("gone", entry_with_cover_jpg("gone", "gone/cover.jpg", "h1"));
            manifest.insert(
                "trash",
                entry_with_cover_jpg("trash", "trash/cover.jpg", "h1"),
            );
            let keep = if with_art {
                desired_arts(
                    "keep",
                    vec![art(
                        ArtifactKind::CoverJpg,
                        "keep/cover.jpg",
                        "https://art/keep",
                        "h1",
                    )],
                )
            } else {
                desired_arts("keep", vec![])
            };
            let mut trash = desired_arts("trash", vec![]);
            trash.trashed = true;
            let local: HashMap<String, LocalFile> = ["keep", "gone", "trash"]
                .iter()
                .map(|id| (id.to_string(), present(100)))
                .collect();
            reconcile(&manifest, &[keep, trash], &local, &mirror_ok())
        };

        let with = build(true);
        let without = build(false);

        // The audio decisions are identical regardless of artifacts.
        let audio = |plan: &Plan| -> Vec<Action> {
            plan.actions
                .iter()
                .filter(|a| {
                    !matches!(
                        a,
                        Action::WriteArtifact { .. } | Action::DeleteArtifact { .. }
                    )
                })
                .cloned()
                .collect()
        };
        assert_eq!(audio(&with), audio(&without));
        assert_eq!(with.deletes(), without.deletes());
        // gone + trash audio deletes, unaffected by the artifacts.
        assert_eq!(with.deletes(), 2);
        // The `with` run additionally reconciles sidecars: gone + trash covers
        // co-deleted, and keep's cover matches so it is neither written nor
        // deleted.
        assert_eq!(with.artifact_deletes(), 2);
        assert_eq!(with.artifact_writes(), 0);
    }

    // ── Phase 6 review fixes: protection, path-drift, kind guard ─────

    #[test]
    fn removed_kind_sidecar_kept_when_clip_is_protected_this_run() {
        // Covers opt out of removed-kind deletion, so a kept clip keeps its cover
        // regardless of protection. This case additionally proves protection is
        // honoured: a private clip and a copy-held clip each keep a removed-kind
        // cover even though the persisted entry is NOT preserve-marked and the
        // mirror is fully enumerated.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
        assert!(!manifest.get("a").unwrap().preserve);

        // Private this run.
        let private = Desired {
            private: true,
            ..desired_arts("a", vec![])
        };
        let plan = reconcile(&manifest, &[private], &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_deletes(), 0);

        // Copy-held this run (modes contains Copy).
        let copy_held = Desired {
            modes: vec![SourceMode::Copy],
            ..desired_arts("a", vec![])
        };
        let plan = reconcile(&manifest, &[copy_held], &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_deletes(), 0);
    }

    #[test]
    fn write_artifact_emitted_when_path_differs_even_if_hash_matches() {
        // The audio moved (new album/name) so the sidecar belongs at a new path;
        // the bytes are unchanged (same hash) but a rewrite at the new path is
        // still required. Reconcile emits no DeleteArtifact for the old path: the
        // executor's WriteArtifact relocates the sidecar (writes new, removes the
        // old copy), so the plan stays a single write.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry_with_cover_jpg("a", "old/cover.jpg", "h1"));
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "new/cover.jpg",
                "https://art/a",
                "h1",
            )],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 1);
        assert_eq!(plan.artifact_deletes(), 0);
        if let Action::WriteArtifact { path, .. } = write_artifacts(&plan)[0] {
            assert_eq!(path, "new/cover.jpg");
        } else {
            panic!("expected a WriteArtifact");
        }
    }

    #[test]
    fn needs_write_drift_applies_hash_path_and_probe_rules() {
        let local: HashMap<String, LocalFile> = [
            ("ok".to_string(), present(10)),
            ("missing".to_string(), LocalFile::default()),
            ("empty".to_string(), present(0)),
        ]
        .into_iter()
        .collect();

        assert!(needs_write_drift(None, "h1", "ok", &local));
        assert!(!needs_write_drift(Some(("h1", "ok")), "h1", "ok", &local));
        assert!(needs_write_drift(Some(("h0", "ok")), "h1", "ok", &local));
        assert!(needs_write_drift(
            Some(("h1", "missing")),
            "h1",
            "missing",
            &local
        ));
        assert!(needs_write_drift(
            Some(("h1", "empty")),
            "h1",
            "empty",
            &local
        ));
        assert!(!needs_write_drift(
            Some(("h1", "unprobed")),
            "h1",
            "unprobed",
            &local
        ));
    }

    #[test]
    fn per_clip_reconcile_ignores_album_and_library_kinds() {
        // Album/library kinds must never be written per clip (they have no
        // per-song manifest slot, so they would be rewritten every run). A
        // CoverJpg alongside them is still handled.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired_arts(
            "a",
            vec![
                art(
                    ArtifactKind::FolderJpg,
                    "a/folder.jpg",
                    "https://art/folder",
                    "hf",
                ),
                art(
                    ArtifactKind::Playlist,
                    "a/list.m3u",
                    "https://art/list",
                    "hp",
                ),
                art(ArtifactKind::CoverJpg, "a/cover.jpg", "https://art/a", "h1"),
            ],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 1);
        let paths: Vec<&str> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::WriteArtifact { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(paths, vec!["a/cover.jpg"]);
    }

    #[test]
    fn per_clip_reconcile_emits_nothing_for_album_only_artifacts() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::FolderWebp,
                "a/folder.webp",
                "https://art/folder",
                "hf",
            )],
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.artifact_writes(), 0);
        assert_eq!(plan.artifact_deletes(), 0);
    }

    // ── Self-heal: missing-on-disk sidecar / folder-art / playlist ──

    /// A local probe map that marks `path` as missing (exists=false).
    fn local_with_missing(audio_id: &str, missing_path: &str) -> HashMap<String, LocalFile> {
        let mut m = local_present(audio_id);
        m.insert(missing_path.to_owned(), LocalFile::default());
        m
    }

    /// A local probe map that marks `path` as present (exists=true, size>0).
    fn local_with_present_artifact(
        audio_id: &str,
        artifact_path: &str,
    ) -> HashMap<String, LocalFile> {
        let mut m = local_present(audio_id);
        m.insert(artifact_path.to_owned(), present(50));
        m
    }

    #[test]
    fn sidecar_missing_on_disk_forces_rewrite() {
        // Manifest and desired agree on hash+path, but the file is absent on
        // disk: the probe forces needs_write = true and a WriteArtifact is
        // emitted to self-heal it.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "a/cover.jpg",
                "https://art/a",
                "h1",
            )],
        )];
        let local = local_with_missing("a", "a/cover.jpg");
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert_eq!(
            plan.artifact_writes(),
            1,
            "missing sidecar must be rewritten"
        );
        assert_eq!(plan.artifact_deletes(), 0);
    }

    #[test]
    fn sidecar_present_on_disk_with_matching_hash_no_churn() {
        // Same manifest / desired / hash — but the file IS present. No write.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "a/cover.jpg",
                "https://art/a",
                "h1",
            )],
        )];
        let local = local_with_present_artifact("a", "a/cover.jpg");
        let plan = reconcile(&manifest, &d, &local, &mirror_ok());
        assert_eq!(plan.artifact_writes(), 0, "present sidecar must not churn");
        assert_eq!(plan.artifact_deletes(), 0);
    }

    #[test]
    fn sidecar_probe_absent_falls_back_to_hash_comparison_no_write() {
        // When the artifact path is not in the local map (probe unavailable),
        // the engine falls back to hash/path comparison only. A matching entry
        // must NOT trigger a write, and must NOT trigger a delete.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
        let d = vec![desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "a/cover.jpg",
                "https://art/a",
                "h1",
            )],
        )];
        // local only has the audio entry; cover path is unprobeable.
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(
            plan.artifact_writes(),
            0,
            "no write when probe unavailable and hash matches"
        );
        assert_eq!(
            plan.artifact_deletes(),
            0,
            "missing probe must never trigger a delete"
        );
    }

    #[test]
    fn folder_art_missing_on_disk_forces_rewrite() {
        // The album store records a matching folder.jpg, but the file is absent:
        // the probe must force a WriteArtifact.
        let members = vec![album_member(
            album_clip("a", 1, "t0", "art-a", ""),
            "root",
            "c/al/a.flac",
        )];
        let desired = album_desired(&members, false, false);
        let mut albums = BTreeMap::new();
        albums.insert(
            "root".to_string(),
            AlbumArt {
                folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
                folder_webp: None,
                folder_mp4: None,
            },
        );
        let mut local: HashMap<String, LocalFile> = HashMap::new();
        local.insert("c/al/folder.jpg".to_owned(), LocalFile::default());
        let actions = plan_album_artifacts(&desired, &albums, true, &local);
        assert_eq!(actions.len(), 1, "missing folder art must be rewritten");
        assert!(matches!(
            &actions[0],
            Action::WriteArtifact {
                kind: ArtifactKind::FolderJpg,
                ..
            }
        ));
    }

    #[test]
    fn folder_art_present_on_disk_no_churn() {
        // Matching hash+path and the file is present: no write.
        let members = vec![album_member(
            album_clip("a", 1, "t0", "art-a", ""),
            "root",
            "c/al/a.flac",
        )];
        let desired = album_desired(&members, false, false);
        let mut albums = BTreeMap::new();
        albums.insert(
            "root".to_string(),
            AlbumArt {
                folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
                folder_webp: None,
                folder_mp4: None,
            },
        );
        let mut local: HashMap<String, LocalFile> = HashMap::new();
        local.insert("c/al/folder.jpg".to_owned(), present(5000));
        let actions = plan_album_artifacts(&desired, &albums, true, &local);
        assert!(
            actions.is_empty(),
            "present folder art with matching hash must not churn"
        );
    }

    #[test]
    fn playlist_missing_on_disk_forces_rewrite() {
        // The playlist store records a matching entry, but the file is absent:
        // the probe must force a WriteArtifact.
        let desired = vec![pl_desired("pl1", "Mix", "Mix.m3u8", "h1")];
        let stored = pl_store(&[("pl1", pl_state("Mix", "Mix.m3u8", "h1"))]);
        let mut local: HashMap<String, LocalFile> = HashMap::new();
        local.insert("Mix.m3u8".to_owned(), LocalFile::default());
        let actions = plan_playlist_artifacts(&desired, &stored, true, true, &local);
        assert_eq!(actions.len(), 1, "missing playlist file must be rewritten");
        assert!(matches!(
            &actions[0],
            Action::WriteArtifact {
                kind: ArtifactKind::Playlist,
                ..
            }
        ));
    }

    #[test]
    fn playlist_present_on_disk_no_churn() {
        // Matching hash+path and the file is present: no write.
        let desired = vec![pl_desired("pl1", "Mix", "Mix.m3u8", "h1")];
        let stored = pl_store(&[("pl1", pl_state("Mix", "Mix.m3u8", "h1"))]);
        let mut local: HashMap<String, LocalFile> = HashMap::new();
        local.insert("Mix.m3u8".to_owned(), present(200));
        let actions = plan_playlist_artifacts(&desired, &stored, true, true, &local);
        assert!(
            actions.is_empty(),
            "present playlist with matching hash must not churn"
        );
    }

    // ── Phase 8: folder art (album-scoped) ──────────────────────────

    fn album_clip(id: &str, play_count: u64, created_at: &str, image: &str, video: &str) -> Clip {
        Clip {
            id: id.to_string(),
            title: "Song".to_string(),
            image_large_url: image.to_string(),
            video_cover_url: video.to_string(),
            play_count,
            created_at: created_at.to_string(),
            ..Default::default()
        }
    }

    fn album_member(clip: Clip, root_id: &str, path: &str) -> Desired {
        let mut lineage = LineageContext::own_root(&clip);
        lineage.root_id = root_id.to_string();
        Desired {
            clip,
            lineage,
            path: path.to_string(),
            format: AudioFormat::Flac,
            meta_hash: "m".to_string(),
            art_hash: "a".to_string(),
            modes: vec![SourceMode::Mirror],
            trashed: false,
            private: false,
            artifacts: Vec::new(),
            stems: None,
        }
    }

    fn stored(path: &str, hash: &str) -> ArtifactState {
        ArtifactState {
            path: path.to_string(),
            hash: hash.to_string(),
        }
    }

    #[test]
    fn folder_jpg_source_is_most_played() {
        let members = vec![
            album_member(album_clip("a", 5, "t0", "art-a", ""), "root", "c/al/a.flac"),
            album_member(album_clip("b", 9, "t1", "art-b", ""), "root", "c/al/b.flac"),
            album_member(album_clip("c", 2, "t2", "art-c", ""), "root", "c/al/c.flac"),
        ];
        let albums = album_desired(&members, false, false);
        assert_eq!(albums.len(), 1);
        let jpg = albums[0].folder_jpg.as_ref().unwrap();
        // "b" has the highest play_count, so its art content hash wins.
        assert_eq!(jpg.hash, art_url_hash("art-b"));
        assert_eq!(jpg.source_url, "art-b");
        assert_eq!(jpg.path, "c/al/folder.jpg");
        assert_eq!(jpg.kind, ArtifactKind::FolderJpg);
    }

    #[test]
    fn folder_jpg_tie_breaks_earliest_then_lex_id() {
        // Equal play_count: earliest created_at wins.
        let by_time = vec![
            album_member(album_clip("z", 4, "t2", "art-z", ""), "root", "c/al/z.flac"),
            album_member(album_clip("y", 4, "t0", "art-y", ""), "root", "c/al/y.flac"),
            album_member(album_clip("x", 4, "t1", "art-x", ""), "root", "c/al/x.flac"),
        ];
        let jpg = album_desired(&by_time, false, false)[0]
            .folder_jpg
            .clone()
            .unwrap();
        assert_eq!(jpg.source_url, "art-y");

        // Equal play_count and created_at: lexicographically smallest id wins.
        let by_id = vec![
            album_member(album_clip("m", 4, "t0", "art-m", ""), "root", "c/al/m.flac"),
            album_member(album_clip("g", 4, "t0", "art-g", ""), "root", "c/al/g.flac"),
        ];
        let jpg = album_desired(&by_id, false, false)[0]
            .folder_jpg
            .clone()
            .unwrap();
        assert_eq!(jpg.source_url, "art-g");
    }

    #[test]
    fn folder_webp_source_is_first_created_animated() {
        let members = vec![
            album_member(
                album_clip("a", 9, "t2", "art-a", "vid-a"),
                "root",
                "c/al/a.flac",
            ),
            album_member(
                album_clip("b", 1, "t0", "art-b", "vid-b"),
                "root",
                "c/al/b.flac",
            ),
            album_member(album_clip("c", 5, "t1", "art-c", ""), "root", "c/al/c.flac"),
        ];
        let webp = album_desired(&members, true, false)[0]
            .folder_webp
            .clone()
            .unwrap();
        // "b" is earliest-created with an animated source, regardless of plays.
        assert_eq!(webp.source_url, "vid-b");
        assert_eq!(webp.hash, art_url_hash("vid-b"));
        assert_eq!(webp.path, "c/al/cover.webp");
        assert_eq!(webp.kind, ArtifactKind::FolderWebp);
    }

    #[test]
    fn animated_covers_off_yields_no_folder_webp() {
        let members = vec![album_member(
            album_clip("a", 1, "t0", "art-a", "vid-a"),
            "root",
            "c/al/a.flac",
        )];
        let off = album_desired(&members, false, false);
        assert!(off[0].folder_webp.is_none());
        let on = album_desired(&members, true, false);
        assert!(on[0].folder_webp.is_some());
    }

    #[test]
    fn raw_cover_yields_folder_mp4_from_the_webp_source_verbatim() {
        let members = vec![
            album_member(
                album_clip("a", 9, "t2", "art-a", "vid-a"),
                "root",
                "c/al/a.flac",
            ),
            album_member(
                album_clip("b", 1, "t0", "art-b", "vid-b"),
                "root",
                "c/al/b.flac",
            ),
        ];
        // `both`: cover.webp (transcoded) and cover.mp4 (raw) come from the SAME
        // earliest-created animated variant, so they describe one animation. The
        // raw cover keeps the `video_cover_url` unchanged and hashes on the URL.
        let album = album_desired(&members, true, true).remove(0);
        let webp = album.folder_webp.unwrap();
        let mp4 = album.folder_mp4.unwrap();
        assert_eq!(mp4.kind, ArtifactKind::FolderMp4);
        assert_eq!(mp4.path, "c/al/cover.mp4");
        assert_eq!(mp4.source_url, "vid-b");
        assert_eq!(mp4.hash, art_url_hash("vid-b"));
        assert_eq!(mp4.source_url, webp.source_url, "same variant feeds both");
    }

    #[test]
    fn raw_cover_and_webp_are_independent_toggles() {
        let members = vec![album_member(
            album_clip("a", 1, "t0", "art-a", "vid-a"),
            "root",
            "c/al/a.flac",
        )];
        // webp-only keeps the transcode but no raw mp4.
        let webp_only = album_desired(&members, true, false).remove(0);
        assert!(webp_only.folder_webp.is_some());
        assert!(webp_only.folder_mp4.is_none());
        // mp4-only keeps the raw source but no transcode.
        let mp4_only = album_desired(&members, false, true).remove(0);
        assert!(mp4_only.folder_webp.is_none());
        assert!(mp4_only.folder_mp4.is_some());
    }

    #[test]
    fn raw_cover_needs_an_animated_source() {
        // No variant carries a video_cover_url, so there is nothing to keep.
        let members = vec![album_member(
            album_clip("a", 3, "t0", "art-a", ""),
            "root",
            "c/al/a.flac",
        )];
        let album = album_desired(&members, true, true).remove(0);
        assert!(album.folder_mp4.is_none());
        assert!(album.folder_webp.is_none());
    }

    #[test]
    fn album_with_no_art_yields_no_folder_jpg() {
        let members = vec![album_member(
            album_clip("a", 3, "t0", "", ""),
            "root",
            "c/al/a.flac",
        )];
        let albums = album_desired(&members, true, false);
        assert!(albums[0].folder_jpg.is_none());
        assert!(albums[0].folder_webp.is_none());
    }

    #[test]
    fn album_desired_groups_by_root_id() {
        let members = vec![
            album_member(album_clip("a", 1, "t0", "art-a", ""), "r1", "c/al1/a.flac"),
            album_member(album_clip("b", 1, "t0", "art-b", ""), "r2", "c/al2/b.flac"),
            album_member(album_clip("c", 9, "t0", "art-c", ""), "r1", "c/al1/c.flac"),
        ];
        let albums = album_desired(&members, false, false);
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].root_id, "r1");
        assert_eq!(albums[0].folder_jpg.as_ref().unwrap().source_url, "art-c");
        assert_eq!(
            albums[0].folder_jpg.as_ref().unwrap().path,
            "c/al1/folder.jpg"
        );
        assert_eq!(albums[1].root_id, "r2");
        assert_eq!(albums[1].folder_jpg.as_ref().unwrap().source_url, "art-b");
        assert_eq!(
            albums[1].folder_jpg.as_ref().unwrap().path,
            "c/al2/folder.jpg"
        );
    }

    #[test]
    fn plan_writes_folder_art_when_store_empty() {
        let members = vec![album_member(
            album_clip("a", 1, "t0", "art-a", "vid-a"),
            "root",
            "c/al/a.flac",
        )];
        let desired = album_desired(&members, true, false);
        let actions = plan_album_artifacts(&desired, &BTreeMap::new(), true, &HashMap::new());
        assert_eq!(
            actions,
            vec![
                Action::WriteArtifact {
                    kind: ArtifactKind::FolderJpg,
                    path: "c/al/folder.jpg".to_string(),
                    source_url: "art-a".to_string(),
                    hash: art_url_hash("art-a"),
                    owner_id: "root".to_string(),
                    content: None,
                },
                Action::WriteArtifact {
                    kind: ArtifactKind::FolderWebp,
                    path: "c/al/cover.webp".to_string(),
                    source_url: "vid-a".to_string(),
                    hash: art_url_hash("vid-a"),
                    owner_id: "root".to_string(),
                    content: None,
                },
            ]
        );
    }

    #[test]
    fn plan_skips_when_hash_and_path_match() {
        let members = vec![album_member(
            album_clip("a", 1, "t0", "art-a", ""),
            "root",
            "c/al/a.flac",
        )];
        let desired = album_desired(&members, false, false);
        let mut albums = BTreeMap::new();
        albums.insert(
            "root".to_string(),
            AlbumArt {
                folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
                folder_webp: None,
                folder_mp4: None,
            },
        );
        assert!(plan_album_artifacts(&desired, &albums, true, &HashMap::new()).is_empty());
    }

    #[test]
    fn plan_rewrites_when_path_drifts_even_if_hash_matches() {
        let members = vec![album_member(
            album_clip("a", 1, "t0", "art-a", ""),
            "root",
            "c/al/a.flac",
        )];
        let desired = album_desired(&members, false, false);
        let mut albums = BTreeMap::new();
        albums.insert(
            "root".to_string(),
            AlbumArt {
                folder_jpg: Some(stored("old/folder.jpg", &art_url_hash("art-a"))),
                folder_webp: None,
                folder_mp4: None,
            },
        );
        let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            Action::WriteArtifact { path, .. } if path == "c/al/folder.jpg"
        ));
    }

    #[test]
    fn h1_most_played_flip_to_same_art_writes_nothing() {
        // Two variants sharing identical art. Run 1: "a" is most-played.
        let run1 = vec![
            album_member(
                album_clip("a", 9, "t0", "same-art", ""),
                "root",
                "c/al/a.flac",
            ),
            album_member(
                album_clip("b", 1, "t1", "same-art", ""),
                "root",
                "c/al/b.flac",
            ),
        ];
        let desired1 = album_desired(&run1, false, false);
        let write1 = plan_album_artifacts(&desired1, &BTreeMap::new(), true, &HashMap::new());
        assert_eq!(write1.len(), 1);

        // Persist the winner's state as the executor would.
        let mut albums = BTreeMap::new();
        if let Action::WriteArtifact {
            path,
            hash,
            owner_id,
            ..
        } = &write1[0]
        {
            albums.insert(
                owner_id.clone(),
                AlbumArt {
                    folder_jpg: Some(stored(path, hash)),
                    folder_webp: None,
                    folder_mp4: None,
                },
            );
        }

        // Run 2: "b" overtakes "a" on plays, but the art content is identical.
        let run2 = vec![
            album_member(
                album_clip("a", 1, "t0", "same-art", ""),
                "root",
                "c/al/a.flac",
            ),
            album_member(
                album_clip("b", 9, "t1", "same-art", ""),
                "root",
                "c/al/b.flac",
            ),
        ];
        let desired2 = album_desired(&run2, false, false);
        // The winner flipped, but the chosen art content hash did not: no churn.
        assert!(plan_album_artifacts(&desired2, &albums, true, &HashMap::new()).is_empty());
    }

    #[test]
    fn h1_flip_to_different_art_writes_exactly_one() {
        let mut albums = BTreeMap::new();
        albums.insert(
            "root".to_string(),
            AlbumArt {
                folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("old-art"))),
                folder_webp: None,
                folder_mp4: None,
            },
        );
        // The new most-played variant carries genuinely different art.
        let members = vec![
            album_member(
                album_clip("a", 1, "t0", "old-art", ""),
                "root",
                "c/al/a.flac",
            ),
            album_member(
                album_clip("b", 9, "t1", "new-art", ""),
                "root",
                "c/al/b.flac",
            ),
        ];
        let desired = album_desired(&members, false, false);
        let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            Action::WriteArtifact { hash, .. } if *hash == art_url_hash("new-art")
        ));
    }

    #[test]
    fn one_write_per_album_regardless_of_clip_count() {
        let members: Vec<Desired> = (0..200)
            .map(|i| {
                album_member(
                    album_clip(
                        &format!("clip-{i:03}"),
                        i as u64,
                        &format!("t{i:03}"),
                        &format!("art-{i:03}"),
                        &format!("vid-{i:03}"),
                    ),
                    "root",
                    &format!("c/al/clip-{i:03}.flac"),
                )
            })
            .collect();
        let desired = album_desired(&members, true, false);
        assert_eq!(desired.len(), 1);
        let actions = plan_album_artifacts(&desired, &BTreeMap::new(), true, &HashMap::new());
        // Exactly one folder.jpg and one cover.webp for the whole 200-clip album.
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, Action::WriteArtifact { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn emptied_album_deletes_only_when_can_delete() {
        let mut albums = BTreeMap::new();
        albums.insert(
            "root".to_string(),
            AlbumArt {
                folder_jpg: Some(stored("c/al/folder.jpg", "h")),
                folder_webp: Some(stored("c/al/cover.webp", "hw")),
                folder_mp4: Some(stored("c/al/cover.mp4", "hm")),
            },
        );
        // No album desires this root any more (it emptied out this run).
        let desired: Vec<AlbumDesired> = Vec::new();

        // Gated off: an incomplete/unsafe listing removes nothing.
        assert!(plan_album_artifacts(&desired, &albums, false, &HashMap::new()).is_empty());

        // Gated on: every stored kind is removed, sorted by kind.
        let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
        assert_eq!(
            actions,
            vec![
                Action::DeleteArtifact {
                    kind: ArtifactKind::FolderJpg,
                    path: "c/al/folder.jpg".to_string(),
                    owner_id: "root".to_string(),
                },
                Action::DeleteArtifact {
                    kind: ArtifactKind::FolderWebp,
                    path: "c/al/cover.webp".to_string(),
                    owner_id: "root".to_string(),
                },
                Action::DeleteArtifact {
                    kind: ArtifactKind::FolderMp4,
                    path: "c/al/cover.mp4".to_string(),
                    owner_id: "root".to_string(),
                },
            ]
        );
    }

    #[test]
    fn disappeared_webp_source_deletes_only_that_kind_when_gated() {
        let mut albums = BTreeMap::new();
        albums.insert(
            "root".to_string(),
            AlbumArt {
                folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
                folder_webp: Some(stored("c/al/cover.webp", &art_url_hash("vid-a"))),
                folder_mp4: None,
            },
        );
        // The album is still present with the same folder.jpg, but animated
        // covers are now off, so the webp source has disappeared.
        let members = vec![album_member(
            album_clip("a", 1, "t0", "art-a", "vid-a"),
            "root",
            "c/al/a.flac",
        )];
        let desired = album_desired(&members, false, false);

        assert!(plan_album_artifacts(&desired, &albums, false, &HashMap::new()).is_empty());

        let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
        assert_eq!(
            actions,
            vec![Action::DeleteArtifact {
                kind: ArtifactKind::FolderWebp,
                path: "c/al/cover.webp".to_string(),
                owner_id: "root".to_string(),
            }]
        );
    }

    #[test]
    fn disappeared_raw_cover_deletes_only_that_kind_when_gated() {
        let mut albums = BTreeMap::new();
        albums.insert(
            "root".to_string(),
            AlbumArt {
                folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
                folder_webp: Some(stored("c/al/cover.webp", &art_url_hash("vid-a"))),
                folder_mp4: Some(stored("c/al/cover.mp4", &art_url_hash("vid-a"))),
            },
        );
        // The album stays and animated covers stay on, but raw cover retention
        // is now off, so only the raw `cover.mp4` is no longer desired.
        let members = vec![album_member(
            album_clip("a", 1, "t0", "art-a", "vid-a"),
            "root",
            "c/al/a.flac",
        )];
        let desired = album_desired(&members, true, false);

        // Gated off: nothing removed on an unsafe listing.
        assert!(plan_album_artifacts(&desired, &albums, false, &HashMap::new()).is_empty());

        // Gated on: only the raw cover goes; folder.jpg and cover.webp stay.
        let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
        assert_eq!(
            actions,
            vec![Action::DeleteArtifact {
                kind: ArtifactKind::FolderMp4,
                path: "c/al/cover.mp4".to_string(),
                owner_id: "root".to_string(),
            }]
        );
    }

    #[test]
    fn plan_album_artifacts_is_deterministically_ordered() {
        let members = vec![
            album_member(
                album_clip("a", 1, "t0", "art-a", "vid-a"),
                "r2",
                "c/al2/a.flac",
            ),
            album_member(
                album_clip("b", 1, "t0", "art-b", "vid-b"),
                "r1",
                "c/al1/b.flac",
            ),
        ];
        let desired = album_desired(&members, true, true);
        let actions = plan_album_artifacts(&desired, &BTreeMap::new(), true, &HashMap::new());
        let keys: Vec<(&str, ArtifactKind)> = actions
            .iter()
            .map(|a| match a {
                Action::WriteArtifact { owner_id, kind, .. } => (owner_id.as_str(), *kind),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                ("r1", ArtifactKind::FolderJpg),
                ("r1", ArtifactKind::FolderWebp),
                ("r1", ArtifactKind::FolderMp4),
                ("r2", ArtifactKind::FolderJpg),
                ("r2", ArtifactKind::FolderWebp),
                ("r2", ArtifactKind::FolderMp4),
            ]
        );
    }

    // ── Phase 9: playlist artifacts ─────────────────────────────────

    fn pl_desired(id: &str, name: &str, path: &str, hash: &str) -> PlaylistDesired {
        PlaylistDesired {
            id: id.to_owned(),
            name: name.to_owned(),
            path: path.to_owned(),
            content: format!("#EXTM3U\n#PLAYLIST:{name}\n<{hash}>\n"),
            hash: hash.to_owned(),
        }
    }

    fn pl_state(name: &str, path: &str, hash: &str) -> PlaylistState {
        PlaylistState {
            name: name.to_owned(),
            path: path.to_owned(),
            hash: hash.to_owned(),
        }
    }

    fn pl_store(entries: &[(&str, PlaylistState)]) -> BTreeMap<String, PlaylistState> {
        entries
            .iter()
            .map(|(id, state)| ((*id).to_owned(), state.clone()))
            .collect()
    }

    #[test]
    fn playlist_write_emitted_for_a_new_playlist() {
        let desired = vec![pl_desired("pl1", "Road Trip", "Road Trip.m3u8", "h1")];
        let actions =
            plan_playlist_artifacts(&desired, &BTreeMap::new(), true, true, &HashMap::new());
        assert_eq!(
            actions,
            vec![Action::WriteArtifact {
                kind: ArtifactKind::Playlist,
                path: "Road Trip.m3u8".to_owned(),
                source_url: String::new(),
                hash: "h1".to_owned(),
                owner_id: "pl1".to_owned(),
                content: Some("#EXTM3U\n#PLAYLIST:Road Trip\n<h1>\n".to_owned()),
            }]
        );
    }

    #[test]
    fn playlist_write_emitted_when_hash_changes() {
        // Same id and path, different content hash (a member's title, an order
        // flip, a new path) — the m3u8 is rewritten (B1).
        let desired = vec![pl_desired("pl1", "Mix", "Mix.m3u8", "h2")];
        let stored = pl_store(&[("pl1", pl_state("Mix", "Mix.m3u8", "h1"))]);
        let actions = plan_playlist_artifacts(&desired, &stored, true, true, &HashMap::new());
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            Action::WriteArtifact { hash, owner_id, .. } if hash == "h2" && owner_id == "pl1"
        ));
    }

    #[test]
    fn playlist_unchanged_is_idempotent() {
        let desired = vec![pl_desired("pl1", "Mix", "Mix.m3u8", "h1")];
        let stored = pl_store(&[("pl1", pl_state("Mix", "Mix.m3u8", "h1"))]);
        let actions = plan_playlist_artifacts(&desired, &stored, true, true, &HashMap::new());
        assert!(actions.is_empty(), "an unchanged playlist plans nothing");
    }

    #[test]
    fn playlist_rename_writes_new_and_deletes_old_path() {
        // The playlist was renamed on Suno, so its sanitised path changed: write
        // the new file and delete the old one, both under the full delete gate.
        let desired = vec![pl_desired("pl1", "Summer", "Summer.m3u8", "h2")];
        let stored = pl_store(&[("pl1", pl_state("Spring", "Spring.m3u8", "h1"))]);
        let actions = plan_playlist_artifacts(&desired, &stored, true, true, &HashMap::new());
        assert_eq!(
            actions,
            vec![
                Action::WriteArtifact {
                    kind: ArtifactKind::Playlist,
                    path: "Summer.m3u8".to_owned(),
                    source_url: String::new(),
                    hash: "h2".to_owned(),
                    owner_id: "pl1".to_owned(),
                    content: Some("#EXTM3U\n#PLAYLIST:Summer\n<h2>\n".to_owned()),
                },
                Action::DeleteArtifact {
                    kind: ArtifactKind::Playlist,
                    path: "Spring.m3u8".to_owned(),
                    owner_id: "pl1".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn playlist_rename_keeps_old_file_when_deletes_disallowed() {
        // A rename still writes the new file, but the OLD-path cleanup is a
        // delete and is gated: no can_delete means no removal (B2).
        let desired = vec![pl_desired("pl1", "Summer", "Summer.m3u8", "h2")];
        let stored = pl_store(&[("pl1", pl_state("Spring", "Spring.m3u8", "h1"))]);
        let actions = plan_playlist_artifacts(&desired, &stored, false, true, &HashMap::new());
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            Action::WriteArtifact { path, .. } if path == "Summer.m3u8"
        ));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::DeleteArtifact { .. })),
            "old path must not be deleted when deletes are disallowed"
        );
    }

    #[test]
    fn playlist_stale_removed_only_under_full_gate() {
        // A stored playlist absent from desired is stale. It is deleted only when
        // BOTH can_delete and list_fully_enumerated hold.
        let stored = pl_store(&[("gone", pl_state("Gone", "Gone.m3u8", "h1"))]);

        let deleted = plan_playlist_artifacts(&[], &stored, true, true, &HashMap::new());
        assert_eq!(
            deleted,
            vec![Action::DeleteArtifact {
                kind: ArtifactKind::Playlist,
                path: "Gone.m3u8".to_owned(),
                owner_id: "gone".to_owned(),
            }]
        );

        // Any gate off → no delete.
        assert!(plan_playlist_artifacts(&[], &stored, false, true, &HashMap::new()).is_empty());
        assert!(plan_playlist_artifacts(&[], &stored, true, false, &HashMap::new()).is_empty());
        assert!(plan_playlist_artifacts(&[], &stored, false, false, &HashMap::new()).is_empty());
    }

    #[test]
    fn b2_failed_list_emits_zero_writes_and_zero_deletes() {
        // B2 BLOCKER: when the /api/playlist/me listing fails, the caller passes
        // an empty desired and list_fully_enumerated=false. Even with a
        // non-empty store and can_delete, NOTHING is planned — every existing
        // .m3u8 is left untouched.
        let stored = pl_store(&[
            ("pl1", pl_state("Mix", "Mix.m3u8", "h1")),
            ("pl2", pl_state("Chill", "Chill.m3u8", "h2")),
        ]);
        let actions = plan_playlist_artifacts(&[], &stored, true, false, &HashMap::new());
        assert!(
            actions.is_empty(),
            "a failed playlist listing must plan zero actions, got {actions:?}"
        );
    }

    #[test]
    fn b2_empty_list_deletes_only_when_fully_enumerated() {
        // An empty desired that contradicts a non-empty store is a genuine
        // wipe ONLY when the listing was fully enumerated (and can_delete). That
        // path IS a mass delete — the CLI cap/confirmation then guards it — but
        // an unreliable listing (not fully enumerated) plans nothing here (B2).
        let stored = pl_store(&[
            ("pl1", pl_state("Mix", "Mix.m3u8", "h1")),
            ("pl2", pl_state("Chill", "Chill.m3u8", "h2")),
        ]);

        // Not fully enumerated: zero deletes (the safety valve).
        assert!(plan_playlist_artifacts(&[], &stored, true, false, &HashMap::new()).is_empty());

        // Fully enumerated and allowed: both are deleted (the caller's cap
        // catches this mass removal).
        let wiped = plan_playlist_artifacts(&[], &stored, true, true, &HashMap::new());
        assert_eq!(
            wiped
                .iter()
                .filter(|a| matches!(a, Action::DeleteArtifact { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn b2_failed_member_playlist_is_untouched_while_others_reconcile() {
        // A playlist whose member fetch failed is excluded upstream from BOTH
        // desired and the stored map handed here, so it is neither rewritten nor
        // treated as stale: its .m3u8 survives while a sibling reconciles.
        // `pl_ok` reconciles; `pl_fail` is simply absent from both maps.
        let desired = vec![pl_desired("pl_ok", "Ok", "Ok.m3u8", "h2")];
        let stored = pl_store(&[("pl_ok", pl_state("Ok", "Ok.m3u8", "h1"))]);
        let actions = plan_playlist_artifacts(&desired, &stored, true, true, &HashMap::new());
        // Only the healthy playlist is rewritten; nothing references pl_fail.
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            Action::WriteArtifact { owner_id, .. } if owner_id == "pl_ok"
        ));
        assert!(
            !actions.iter().any(|a| match a {
                Action::WriteArtifact { owner_id, .. }
                | Action::DeleteArtifact { owner_id, .. } => owner_id == "pl_fail",
                _ => false,
            }),
            "a protected (failed-member) playlist must have no action"
        );
    }

    #[test]
    fn playlist_rename_collision_downgrades_the_delete() {
        // pl1 renames Old -> Shared.m3u8; pl2 already renders Shared.m3u8 this
        // run. The delete of pl1's old path is fine, but a delete must never
        // alias a write target, so if the OLD path equals another write target
        // it is downgraded. Here we force the collision: pl1's old path is the
        // very path pl2 writes.
        let desired = vec![
            pl_desired("pl1", "Shared", "Shared.m3u8", "h2"),
            pl_desired("pl2", "Shared", "Shared.m3u8", "h3"),
        ];
        let stored = pl_store(&[("pl1", pl_state("Old", "Shared.m3u8", "h1"))]);
        let actions = plan_playlist_artifacts(&desired, &stored, true, true, &HashMap::new());
        // No DeleteArtifact survives against a path some write produces.
        let write_paths: BTreeSet<&str> = actions
            .iter()
            .filter_map(|a| match a {
                Action::WriteArtifact { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        for a in &actions {
            if let Action::DeleteArtifact { path, .. } = a {
                assert!(
                    !write_paths.contains(path.as_str()),
                    "a playlist delete aliases a write target: {path}"
                );
            }
        }
    }

    // ── Keyed stem reconcile ────────────────────────────────────────

    fn dstem(key: &str, path: &str, hash: &str) -> DesiredStem {
        DesiredStem {
            key: key.to_string(),
            stem_id: key.to_string(),
            path: path.to_string(),
            source_url: format!("https://cdn1.suno.ai/{key}.mp3"),
            format: StemFormat::Mp3,
            hash: hash.to_string(),
        }
    }

    /// A kept FLAC clip that desires the given (possibly `None`) stem set.
    fn stem_desired(id: &str, stems: Option<Vec<DesiredStem>>) -> Desired {
        Desired {
            stems,
            ..desired(id, &format!("{id}.flac"), AudioFormat::Flac, "m", "art")
        }
    }

    /// A manifest entry for a kept clip carrying the given tracked stems.
    fn entry_with_stems(id: &str, stems: &[(&str, &str, &str)]) -> ManifestEntry {
        let mut e = entry(&format!("{id}.flac"), AudioFormat::Flac, "m", "art");
        for (key, path, hash) in stems {
            e.stems.insert(
                key.to_string(),
                ArtifactState {
                    path: path.to_string(),
                    hash: hash.to_string(),
                },
            );
        }
        e
    }

    fn stem_writes(plan: &Plan) -> Vec<(&str, &str)> {
        plan.actions
            .iter()
            .filter_map(|a| match a {
                Action::WriteStem { key, path, .. } => Some((key.as_str(), path.as_str())),
                _ => None,
            })
            .collect()
    }

    fn stem_deletes(plan: &Plan) -> Vec<(&str, &str)> {
        plan.actions
            .iter()
            .filter_map(|a| match a {
                Action::DeleteStem { key, path, .. } => Some((key.as_str(), path.as_str())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn stems_none_keeps_every_existing_stem() {
        // An indeterminate listing (feature off, has_stem false, or a
        // paged-error) surfaces as `None`: no stem is written or deleted.
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry_with_stems(
                "a",
                &[
                    ("voc", "a.stems/voc.mp3", "h1"),
                    ("drm", "a.stems/drm.mp3", "h2"),
                ],
            ),
        );
        let d = vec![stem_desired("a", None)];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.stem_writes(), 0);
        assert_eq!(plan.stem_deletes(), 0);
    }

    #[test]
    fn stems_authoritative_writes_missing_stems() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
        let d = vec![stem_desired(
            "a",
            Some(vec![
                dstem("voc", "a.stems/voc.mp3", "h1"),
                dstem("drm", "a.stems/drm.mp3", "h2"),
            ]),
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(
            stem_writes(&plan),
            vec![("voc", "a.stems/voc.mp3"), ("drm", "a.stems/drm.mp3")]
        );
        assert_eq!(plan.stem_deletes(), 0);
    }

    #[test]
    fn stems_authoritative_rewrites_only_on_hash_or_path_drift() {
        let mut manifest = Manifest::new();
        // voc unchanged, drm hash drift, bas path drift (song moved).
        manifest.insert(
            "a",
            entry_with_stems(
                "a",
                &[
                    ("voc", "a.stems/voc.mp3", "h1"),
                    ("drm", "a.stems/drm.mp3", "h2"),
                    ("bas", "old.stems/bas.mp3", "h3"),
                ],
            ),
        );
        let d = vec![stem_desired(
            "a",
            Some(vec![
                dstem("voc", "a.stems/voc.mp3", "h1"),     // unchanged
                dstem("drm", "a.stems/drm.mp3", "h2-new"), // hash drift
                dstem("bas", "a.stems/bas.mp3", "h3"),     // path drift
            ]),
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(
            stem_writes(&plan),
            vec![("drm", "a.stems/drm.mp3"), ("bas", "a.stems/bas.mp3")]
        );
        assert_eq!(plan.stem_deletes(), 0);
    }

    #[test]
    fn stems_authoritative_removes_a_stem_absent_from_the_set() {
        // drm is gone from the authoritative listing, so it is delete-reconciled
        // through the shared gate; voc (still present) is untouched.
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry_with_stems(
                "a",
                &[
                    ("voc", "a.stems/voc.mp3", "h1"),
                    ("drm", "a.stems/drm.mp3", "h2"),
                ],
            ),
        );
        let d = vec![stem_desired(
            "a",
            Some(vec![dstem("voc", "a.stems/voc.mp3", "h1")]),
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        assert_eq!(plan.stem_writes(), 0);
        assert_eq!(stem_deletes(&plan), vec![("drm", "a.stems/drm.mp3")]);
    }

    #[test]
    fn stems_removal_needs_deletion_allowed() {
        // The same authoritative-omission case, but deletion is not allowed this
        // run (no fully-enumerated mirror). The stem is KEPT, never deleted.
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry_with_stems(
                "a",
                &[
                    ("voc", "a.stems/voc.mp3", "h1"),
                    ("drm", "a.stems/drm.mp3", "h2"),
                ],
            ),
        );
        let d = vec![stem_desired(
            "a",
            Some(vec![dstem("voc", "a.stems/voc.mp3", "h1")]),
        )];

        let incomplete = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        }];
        assert_eq!(
            reconcile(&manifest, &d, &local_present("a"), &incomplete).stem_deletes(),
            0
        );

        let copy_only = vec![SourceStatus {
            mode: SourceMode::Copy,
            fully_enumerated: true,
        }];
        assert_eq!(
            reconcile(&manifest, &d, &local_present("a"), &copy_only).stem_deletes(),
            0
        );
    }

    #[test]
    fn stems_removal_skipped_for_preserved_or_protected_clip() {
        let mut manifest = Manifest::new();
        let mut e = entry_with_stems(
            "a",
            &[
                ("voc", "a.stems/voc.mp3", "h1"),
                ("drm", "a.stems/drm.mp3", "h2"),
            ],
        );
        e.preserve = true;
        manifest.insert("a", e);
        let authoritative = Some(vec![dstem("voc", "a.stems/voc.mp3", "h1")]);

        // preserve marker wins: no stem delete.
        let d = vec![stem_desired("a", authoritative.clone())];
        assert_eq!(
            reconcile(&manifest, &d, &local_present("a"), &mirror_ok()).stem_deletes(),
            0
        );

        // A copy-held clip this run also keeps all stems (protected_now).
        let mut manifest2 = Manifest::new();
        manifest2.insert(
            "a",
            entry_with_stems(
                "a",
                &[
                    ("voc", "a.stems/voc.mp3", "h1"),
                    ("drm", "a.stems/drm.mp3", "h2"),
                ],
            ),
        );
        let held = Desired {
            modes: vec![SourceMode::Mirror, SourceMode::Copy],
            stems: authoritative,
            ..desired("a", "a.flac", AudioFormat::Flac, "m", "art")
        };
        assert_eq!(
            reconcile(&manifest2, &[held], &local_present("a"), &mirror_ok()).stem_deletes(),
            0
        );
    }

    #[test]
    fn stems_are_co_deleted_when_the_song_is_trashed() {
        // A trashed clip's audio is deleted; its stems must be co-deleted so the
        // `.stems` folder is not orphaned (no stranding).
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry_with_stems(
                "a",
                &[
                    ("voc", "a.stems/voc.mp3", "h1"),
                    ("drm", "a.stems/drm.mp3", "h2"),
                ],
            ),
        );
        let trashed = Desired {
            trashed: true,
            ..desired("a", "a.flac", AudioFormat::Flac, "m", "art")
        };
        let plan = reconcile(&manifest, &[trashed], &local_present("a"), &mirror_ok());
        assert_eq!(plan.deletes(), 1, "the trashed audio is deleted");
        let mut deleted: Vec<&str> = stem_deletes(&plan).into_iter().map(|(k, _)| k).collect();
        deleted.sort_unstable();
        assert_eq!(deleted, vec!["drm", "voc"], "both stems co-deleted");
    }

    #[test]
    fn stems_are_co_deleted_for_an_absent_clip() {
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry_with_stems("a", &[("voc", "a.stems/voc.mp3", "h1")]),
        );
        // Desired is empty: clip "a" left every source and is deleted.
        let plan = reconcile(&manifest, &[], &local_present("a"), &mirror_ok());
        assert_eq!(plan.deletes(), 1);
        assert_eq!(stem_deletes(&plan), vec![("voc", "a.stems/voc.mp3")]);
    }

    #[test]
    fn stems_are_kept_when_absent_clip_listing_is_incomplete() {
        // SYNC-9: an unreliable listing deletes nothing, stems included.
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry_with_stems("a", &[("voc", "a.stems/voc.mp3", "h1")]),
        );
        let incomplete = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        }];
        let plan = reconcile(&manifest, &[], &HashMap::new(), &incomplete);
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.stem_deletes(), 0);
    }

    #[test]
    fn stem_delete_is_suppressed_when_it_aliases_a_stem_write() {
        // A prior stem at a path is being removed, while a different stem is
        // written to that same path this run (a re-key at a stable path). The
        // delete must be downgraded so it can never clobber the fresh write.
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry_with_stems("a", &[("old", "a.stems/mix.mp3", "h1")]),
        );
        let d = vec![stem_desired(
            "a",
            Some(vec![dstem("new", "a.stems/mix.mp3", "h2")]),
        )];
        let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
        // The new stem is written to the shared path; the old key's delete of the
        // same path is suppressed (no DeleteStem survives for that path).
        assert_eq!(stem_writes(&plan), vec![("new", "a.stems/mix.mp3")]);
        assert!(
            !plan.actions.iter().any(|a| matches!(
                a,
                Action::DeleteStem { path, .. } if path == "a.stems/mix.mp3"
            )),
            "a stem delete must never alias a stem write target"
        );
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
                    ..Default::default()
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
            artifacts: Vec::new(),
            stems: None,
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
