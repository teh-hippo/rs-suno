//! The reconcile plan vocabulary: the desired-state inputs (`Desired` and
//! friends), the on-disk `LocalFile` probe, the per-source `SourceStatus`,
//! and the `Action` / `Plan` output types. Pure data: every deletion
//! *decision* lives in the parent `reconcile` module.

use super::*;

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
    /// Fingerprint of the aligned lyrics this run intends to have embedded in the
    /// audio tag, or empty when none. Compared against
    /// [`ManifestEntry::embedded_lyrics_hash`](crate::manifest::ManifestEntry::embedded_lyrics_hash)
    /// to drive a back-fill retag when Suno's fetched alignment is missing or
    /// stale in the tag (#354). Populated at the synced-lyrics resolve seam: the
    /// content hash of the fetched `.lrc` body on a fetch, otherwise the
    /// persisted value carried forward (so a retag only ever fires when alignment
    /// was actually fetched this run).
    pub embedded_lyrics_hash: String,
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
    /// Only ever emitted through `delete_artifact_action`, which shares the
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
    /// Only ever emitted through `delete_stem_action`, which shares the audio
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
