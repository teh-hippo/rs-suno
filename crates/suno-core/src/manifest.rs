//! The on-disk manifest: the engine's record of prior download state.
//!
//! The manifest is the prior on the reconcile engine: it records, per clip id,
//! where the file lives, its format, the content hashes used to detect tag and
//! art drift, and its size. The CLI loads and saves it; this module only models
//! it and provides pure helpers. It is unversioned: serde round-trips it to a
//! flat JSON object keyed by clip id with no envelope.

use std::collections::BTreeMap;
use std::collections::btree_map::Iter;

use serde::{Deserialize, Serialize};

use crate::config::AudioFormat;

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
    /// Size of the file in bytes when last written.
    pub size: u64,
    /// When set, this clip is held by a copy or archive source, or is private,
    /// so it must never be deleted as an orphan no matter the current selection.
    /// The caller writes this marker; the reconcile engine only reads it.
    pub preserve: bool,
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
}
