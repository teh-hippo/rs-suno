//! The on-disk manifest: the engine's record of prior download state.
//!
//! The manifest is the prior on the reconcile engine: it records, per clip id,
//! where the file lives, its format, the content hashes used to detect tag and
//! art drift, its size, and the state of each external sidecar artifact. The CLI
//! loads and saves it; this module only models it and provides pure helpers. It
//! is unversioned: serde round-trips it to a flat JSON object keyed by clip id
//! with no envelope.

use std::collections::BTreeMap;
use std::collections::btree_map::Iter;

use serde::{Deserialize, Serialize};

use crate::vocab::{ArtifactKind, AudioFormat};

/// The prior known state of one external sidecar artifact for a clip.
///
/// Records where the sidecar lives and a hash of the content or source it was
/// rendered from, so a later reconcile can detect drift and trigger a rewrite.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactState {
    /// Relative path of the sidecar file under the account root.
    pub path: String,
    /// Content/source change hash; a change triggers a rewrite.
    pub hash: String,
}

/// The record that a clip's synced lyrics were resolved (fetched) this run.
///
/// Suno's forced alignment for a clip is immutable in practice, so once a clip's
/// alignment has been fetched it need not be fetched again until the render
/// [`version`](Self::version) bumps. Instrumentals and untimed-fallback clips
/// are re-checked after [`checked_unix`](Self::checked_unix) ages past the
/// re-check window, to pick up alignment Suno may compute after generation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncedLyricsCheck {
    /// The render version this clip's synced lyrics were last resolved at. A
    /// bump forces a re-fetch and re-render (the `.lrc` format changed).
    pub version: u32,
    /// Unix seconds of the last alignment fetch, for the bounded re-check.
    pub checked_unix: u64,
    /// Whether the clip resolved to no lyrics (an instrumental): no `.lrc` was
    /// written.
    pub empty: bool,
    /// Whether the written `.lrc` carries timed (word/line) alignment, as
    /// opposed to an untimed plain-text fallback. Untimed clips are re-checked
    /// after the window, the same as instrumentals, so a later-available
    /// alignment upgrades the `.lrc` and `SYLT`. Defaults to `false` so
    /// pre-existing manifests written before this field existed are re-checked.
    pub timed: bool,
}

/// One manifest record: the prior known state of a single downloaded clip.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestEntry {
    /// Relative path of the audio file under the account root.
    pub path: String,
    /// Format the file was written in.
    pub format: AudioFormat,
    /// Hash of the clip's tag-bearing metadata, for detecting retag needs.
    pub meta_hash: String,
    /// Hash of the embedded cover art, for detecting art drift.
    pub art_hash: String,
    /// Fingerprint of the aligned lyrics currently embedded in the audio tag
    /// (the FLAC `LYRICS` / MP3 `USLT` / ALAC `©lyr` frame), or empty when none
    /// are embedded. Tracked separately from [`meta_hash`](Self::meta_hash)
    /// because the embedded text is Suno's fetched alignment, not `clip.lyrics`,
    /// so a drift here re-tags to back-fill the embed (#354). Its value is the
    /// content hash of the `.lrc` body the embed was rendered from, mirroring how
    /// [`lrc`](Self::lrc) tracks the sidecar. Additive: old manifests load with
    /// `""` and the common no-embed case is omitted from the serialised object.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub embedded_lyrics_hash: String,
    /// Size of the file in bytes when last written.
    pub size: u64,
    /// When set, this clip is held by a copy or archive source, or is private,
    /// so it must never be deleted as an orphan no matter the current selection.
    /// The caller writes this marker; the reconcile engine only reads it.
    pub preserve: bool,
    /// Prior state of the external `cover.jpg` sidecar, when one was written.
    #[serde(default)]
    pub cover_jpg: Option<ArtifactState>,
    /// Prior state of the external `cover.webp` sidecar, when one was written.
    #[serde(default)]
    pub cover_webp: Option<ArtifactState>,
    /// Prior state of the plain-text `.details.txt` sidecar, when one was written.
    #[serde(default)]
    pub details_txt: Option<ArtifactState>,
    /// Prior state of the plain-text `.lyrics.txt` sidecar, when one was written.
    #[serde(default)]
    pub lyrics_txt: Option<ArtifactState>,
    /// Prior state of the synced `.lrc` sidecar, when one was written. Its hash
    /// is the content hash of the rendered `.lrc` body, so an alignment or
    /// renderer change rewrites it.
    #[serde(default)]
    pub lrc: Option<ArtifactState>,
    /// The synced-lyrics resolution marker, gating whether the clip's alignment
    /// is re-fetched. Present once the clip has been resolved (written or empty).
    #[serde(default)]
    pub synced_lyrics: Option<SyncedLyricsCheck>,
    /// Prior state of the standalone `.mp4` music video, when one was written.
    #[serde(default)]
    pub video_mp4: Option<ArtifactState>,
    /// Prior state of each downloaded stem, keyed by a stable per-stem key
    /// (the server stem id, falling back to its label). Unlike the single-slot
    /// sidecars above, a clip owns a *set* of stems, so this is a keyed map:
    /// individual stems are added, rewritten, or removed without disturbing the
    /// others (no whole-folder deletes). Empty and omitted from older manifests,
    /// so the growth is purely additive.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stems: BTreeMap<String, ArtifactState>,
}

impl ManifestEntry {
    /// Every per-clip sidecar (and stem) path this entry currently records,
    /// enumerated in one place so its consumers — the executor's stale-copy
    /// cleanup, orphan detection, and the local-file stat passes — all agree on
    /// which paths a clip still owns.
    pub fn artifact_paths(&self) -> impl Iterator<Item = &str> {
        [
            self.cover_jpg.as_ref(),
            self.cover_webp.as_ref(),
            self.details_txt.as_ref(),
            self.lyrics_txt.as_ref(),
            self.lrc.as_ref(),
            self.video_mp4.as_ref(),
        ]
        .into_iter()
        .flatten()
        .chain(self.stems.values())
        .map(|state| state.path.as_str())
    }

    /// The stored state for one per-clip sidecar `kind`, if present. Album and
    /// library kinds have no per-clip slot and map to `None`. Mirrors
    /// [`AlbumArt::artifact`](crate::album_art::AlbumArt::artifact) so the
    /// kind-to-slot read lives in one place.
    pub(crate) fn artifact(&self, kind: ArtifactKind) -> Option<&ArtifactState> {
        match kind {
            ArtifactKind::CoverJpg => self.cover_jpg.as_ref(),
            ArtifactKind::CoverWebp => self.cover_webp.as_ref(),
            ArtifactKind::DetailsTxt => self.details_txt.as_ref(),
            ArtifactKind::LyricsTxt => self.lyrics_txt.as_ref(),
            ArtifactKind::Lrc => self.lrc.as_ref(),
            ArtifactKind::VideoMp4 => self.video_mp4.as_ref(),
            ArtifactKind::FolderJpg
            | ArtifactKind::FolderWebp
            | ArtifactKind::FolderMp4
            | ArtifactKind::Playlist => None,
        }
    }
}

/// The full prior download state, keyed by clip id.
///
/// Backed by a [`BTreeMap`] so iteration order is stable, which keeps any plan
/// derived from it deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Manifest {
    /// Records keyed by clip id.
    pub entries: BTreeMap<String, ManifestEntry>,
}

impl Manifest {
    /// Create an empty manifest.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the entry for `clip_id`, if present.
    pub fn get(&self, clip_id: &str) -> Option<&ManifestEntry> {
        self.entries.get(clip_id)
    }

    /// Insert or replace the entry for `clip_id`, returning any prior value.
    pub fn insert(
        &mut self,
        clip_id: impl Into<String>,
        entry: ManifestEntry,
    ) -> Option<ManifestEntry> {
        self.entries.insert(clip_id.into(), entry)
    }

    /// Remove and return the entry for `clip_id`, if present.
    pub fn remove(&mut self, clip_id: &str) -> Option<ManifestEntry> {
        self.entries.remove(clip_id)
    }

    /// Return true when an entry exists for `clip_id`.
    pub fn contains(&self, clip_id: &str) -> bool {
        self.entries.contains_key(clip_id)
    }

    /// Iterate entries in clip-id order.
    pub fn iter(&self) -> Iter<'_, String, ManifestEntry> {
        self.entries.iter()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, format: AudioFormat) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            format,
            meta_hash: "m".to_string(),
            art_hash: "a".to_string(),
            size: 42,
            preserve: false,
            ..Default::default()
        }
    }

    #[test]
    fn new_is_empty() {
        let m = Manifest::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn insert_get_contains() {
        let mut m = Manifest::new();
        assert!(m.insert("a", entry("a.flac", AudioFormat::Flac)).is_none());
        assert!(m.contains("a"));
        assert_eq!(m.get("a").unwrap().path, "a.flac");
        assert_eq!(m.len(), 1);
        assert!(!m.is_empty());
    }

    #[test]
    fn insert_replaces_and_returns_prior() {
        let mut m = Manifest::new();
        m.insert("a", entry("a.flac", AudioFormat::Flac));
        let prior = m.insert("a", entry("a.mp3", AudioFormat::Mp3));
        assert_eq!(prior.unwrap().path, "a.flac");
        assert_eq!(m.get("a").unwrap().format, AudioFormat::Mp3);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn remove_returns_prior_then_absent() {
        let mut m = Manifest::new();
        m.insert("a", entry("a.flac", AudioFormat::Flac));
        let removed = m.remove("a");
        assert_eq!(removed.unwrap().path, "a.flac");
        assert!(!m.contains("a"));
        assert!(m.remove("a").is_none());
    }

    #[test]
    fn get_absent_is_none() {
        let m = Manifest::new();
        assert!(m.get("missing").is_none());
    }

    #[test]
    fn iter_is_clip_id_sorted() {
        let mut m = Manifest::new();
        m.insert("c", entry("c.flac", AudioFormat::Flac));
        m.insert("a", entry("a.flac", AudioFormat::Flac));
        m.insert("b", entry("b.flac", AudioFormat::Flac));
        let ids: Vec<&str> = m.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn serde_roundtrip_preserves_entries() {
        let mut m = Manifest::new();
        m.insert("a", entry("a.flac", AudioFormat::Flac));
        m.insert("b", entry("b.mp3", AudioFormat::Mp3));
        // An entry carrying every sidecar artifact must round-trip intact.
        let mut c = entry("c.flac", AudioFormat::Flac);
        c.cover_jpg = Some(ArtifactState {
            path: "c/cover.jpg".to_string(),
            hash: "jpg-hash".to_string(),
        });
        c.cover_webp = Some(ArtifactState {
            path: "c/cover.webp".to_string(),
            hash: "webp-hash".to_string(),
        });
        c.details_txt = Some(ArtifactState {
            path: "c.details.txt".to_string(),
            hash: "details-hash".to_string(),
        });
        c.lyrics_txt = Some(ArtifactState {
            path: "c.lyrics.txt".to_string(),
            hash: "lyrics-hash".to_string(),
        });
        c.lrc = Some(ArtifactState {
            path: "c.lrc".to_string(),
            hash: "lrc-hash".to_string(),
        });
        m.insert("c", c);
        let json = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn serde_is_unversioned_flat_object() {
        let mut m = Manifest::new();
        m.insert("clip1", entry("song.flac", AudioFormat::Flac));
        let value: serde_json::Value = serde_json::to_value(&m).unwrap();
        // Top level is the clip-id map itself, with no envelope or version key.
        assert!(value.is_object());
        assert!(value.get("entries").is_none());
        assert!(value.get("version").is_none());
        let entry = value.get("clip1").unwrap();
        assert_eq!(entry.get("format").unwrap(), "flac");
        assert_eq!(entry.get("path").unwrap(), "song.flac");
    }

    #[test]
    fn empty_manifest_roundtrips() {
        let m = Manifest::new();
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, "{}");
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn unicode_and_reserved_ids_roundtrip() {
        let mut m = Manifest::new();
        m.insert("ünïcode-🎵", entry("音楽.flac", AudioFormat::Flac));
        m.insert("with\"quote", entry("a.flac", AudioFormat::Flac));
        let json = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        assert!(back.contains("ünïcode-🎵"));
    }

    #[test]
    fn default_format_deserialises_when_absent() {
        // A record missing the format key falls back to the compiled default.
        let json = r#"{"clip1":{"path":"a.flac","meta_hash":"","art_hash":"","size":0}}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.get("clip1").unwrap().format, AudioFormat::default());
    }

    #[test]
    fn preserve_defaults_to_false_when_absent() {
        // Older manifests written before the marker existed must load as not
        // preserved, so the field is purely additive.
        let json =
            r#"{"clip1":{"path":"a.flac","format":"flac","meta_hash":"","art_hash":"","size":1}}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert!(!m.get("clip1").unwrap().preserve);
    }

    #[test]
    fn preserve_roundtrips() {
        let mut m = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac);
        e.preserve = true;
        m.insert("a", e);
        let json = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert!(back.get("a").unwrap().preserve);
        assert_eq!(m, back);
    }

    #[test]
    fn cover_artifacts_default_to_none_when_absent() {
        // A pre-growth manifest, written before the sidecar fields existed, must
        // load with no artifacts and unpreserved, proving the growth is purely
        // additive and backwards compatible.
        let json = r#"{"clip1":{"path":"a.flac","format":"flac","meta_hash":"m","art_hash":"a","size":1}}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        let e = m.get("clip1").unwrap();
        assert_eq!(e.cover_jpg, None);
        assert_eq!(e.cover_webp, None);
        assert_eq!(e.details_txt, None);
        assert_eq!(e.lyrics_txt, None);
        assert_eq!(e.lrc, None);
        assert_eq!(e.synced_lyrics, None);
        assert!(e.stems.is_empty());
        assert!(!e.preserve);
    }

    #[test]
    fn synced_lyrics_check_roundtrips_and_defaults() {
        // A pre-feature manifest loads with no synced-lyrics marker; a populated
        // one round-trips intact, so the field is purely additive.
        let json =
            r#"{"c":{"path":"a.flac","format":"flac","meta_hash":"m","art_hash":"a","size":1}}"#;
        assert_eq!(
            serde_json::from_str::<Manifest>(json)
                .unwrap()
                .get("c")
                .unwrap()
                .synced_lyrics,
            None
        );

        let mut m = Manifest::new();
        let mut e = entry("a.flac", AudioFormat::Flac);
        e.synced_lyrics = Some(SyncedLyricsCheck {
            version: 1,
            checked_unix: 1_700_000_000,
            empty: true,
            timed: false,
        });
        m.insert("a", e);
        let back: Manifest = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn stems_default_to_empty_and_are_omitted_when_serialised_empty() {
        // A pre-stems manifest loads with an empty stem map (additive growth),
        // and an entry with no stems serialises without a `stems` key so the
        // on-disk manifest is byte-identical for anyone not using the feature.
        let json = r#"{"clip1":{"path":"a.flac","format":"flac","meta_hash":"m","art_hash":"a","size":1}}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert!(m.get("clip1").unwrap().stems.is_empty());
        let value: serde_json::Value = serde_json::to_value(&m).unwrap();
        assert!(value.get("clip1").unwrap().get("stems").is_none());
    }

    #[test]
    fn stems_map_roundtrips_and_reports_paths() {
        let mut e = entry("song.flac", AudioFormat::Flac);
        e.stems.insert(
            "stem-vocals".to_string(),
            ArtifactState {
                path: "song.stems/song - Vocals [stem-voc].mp3".to_string(),
                hash: "voc-hash".to_string(),
            },
        );
        e.stems.insert(
            "stem-drums".to_string(),
            ArtifactState {
                path: "song.stems/song - Drums [stem-drm].mp3".to_string(),
                hash: "drm-hash".to_string(),
            },
        );
        let mut m = Manifest::new();
        m.insert("clip1", e);
        let json = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        // Both stem paths are reported as owned artifact paths (so the executor
        // co-deletes them with the song and never orphans the `.stems` folder).
        let paths: Vec<&str> = back.get("clip1").unwrap().artifact_paths().collect();
        assert!(paths.contains(&"song.stems/song - Vocals [stem-voc].mp3"));
        assert!(paths.contains(&"song.stems/song - Drums [stem-drm].mp3"));
    }

    #[test]
    fn artifact_returns_the_slot_for_each_per_clip_kind() {
        let mut e = entry("song.flac", AudioFormat::Flac);
        let state = |name: &str| ArtifactState {
            path: name.to_string(),
            hash: format!("{name}-hash"),
        };
        e.cover_jpg = Some(state("cover.jpg"));
        e.cover_webp = Some(state("cover.webp"));
        e.details_txt = Some(state("details.txt"));
        e.lyrics_txt = Some(state("lyrics.txt"));
        e.lrc = Some(state("song.lrc"));
        e.video_mp4 = Some(state("song.mp4"));

        assert_eq!(e.artifact(ArtifactKind::CoverJpg), e.cover_jpg.as_ref());
        assert_eq!(e.artifact(ArtifactKind::CoverWebp), e.cover_webp.as_ref());
        assert_eq!(e.artifact(ArtifactKind::DetailsTxt), e.details_txt.as_ref());
        assert_eq!(e.artifact(ArtifactKind::LyricsTxt), e.lyrics_txt.as_ref());
        assert_eq!(e.artifact(ArtifactKind::Lrc), e.lrc.as_ref());
        assert_eq!(e.artifact(ArtifactKind::VideoMp4), e.video_mp4.as_ref());

        // Album/library kinds have no per-clip slot.
        assert_eq!(e.artifact(ArtifactKind::FolderJpg), None);
        assert_eq!(e.artifact(ArtifactKind::FolderWebp), None);
        assert_eq!(e.artifact(ArtifactKind::FolderMp4), None);
        assert_eq!(e.artifact(ArtifactKind::Playlist), None);
    }

    #[test]
    fn artifact_state_defaults_and_roundtrips() {
        let empty = ArtifactState::default();
        assert_eq!(empty.path, "");
        assert_eq!(empty.hash, "");
        let json = serde_json::to_string(&empty).unwrap();
        let back: ArtifactState = serde_json::from_str(&json).unwrap();
        assert_eq!(empty, back);

        let populated = ArtifactState {
            path: "x/cover.webp".to_string(),
            hash: "content-hash".to_string(),
        };
        let json = serde_json::to_string(&populated).unwrap();
        let back: ArtifactState = serde_json::from_str(&json).unwrap();
        assert_eq!(populated, back);
    }

    #[test]
    fn embedded_lyrics_hash_defaults_and_roundtrips() {
        // A pre-field manifest loads with an empty embed fingerprint (additive
        // growth), an empty value is omitted from the serialised object (so the
        // on-disk manifest is byte-identical for the no-embed majority), and a
        // populated value round-trips.
        let json = r#"{"clip1":{"path":"a.flac","format":"flac","meta_hash":"m","art_hash":"a","size":1}}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.get("clip1").unwrap().embedded_lyrics_hash, "");
        let value: serde_json::Value = serde_json::to_value(&m).unwrap();
        assert!(
            value
                .get("clip1")
                .unwrap()
                .get("embedded_lyrics_hash")
                .is_none(),
            "an empty embed hash is omitted from the manifest"
        );

        let mut e = entry("a.flac", AudioFormat::Flac);
        e.embedded_lyrics_hash = "lrc-content-hash".to_string();
        let mut m2 = Manifest::new();
        m2.insert("a", e);
        let serialised = serde_json::to_string(&m2).unwrap();
        assert!(serialised.contains("embedded_lyrics_hash"));
        let back: Manifest = serde_json::from_str(&serialised).unwrap();
        assert_eq!(m2, back);
        assert_eq!(
            back.get("a").unwrap().embedded_lyrics_hash,
            "lrc-content-hash"
        );
    }
}
