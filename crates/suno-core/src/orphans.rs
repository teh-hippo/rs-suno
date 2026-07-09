//! Read-only detection of orphaned audio files on disk.
//!
//! An orphan is an audio file under the destination that no manifest entry
//! tracks: a clip the user moved or renamed by hand (its recorded path is
//! re-downloaded and the moved copy is left untracked), or a leftover from an
//! older layout. This module only LISTS such files for the dry-run and `check`
//! report; it never matches an orphan back to a clip, renames it, or deletes it,
//! so it can neither mislabel nor lose data.
//!
//! Auto-recovery is deliberately not attempted: the manifest stores no content
//! hash of the audio bytes and the clip id is not embedded in the tags, so there
//! is no provable way to re-identify a moved file, and any guess (by size, by a
//! re-derived metadata hash, or by filename) risks adopting the wrong file into
//! a clip's slot. The safe, honest behaviour is to report orphans and let the
//! user reconcile them by hand.

use std::collections::BTreeSet;

use crate::manifest::Manifest;
use crate::pathkey::canonical_path_key;

/// The audio paths in `on_disk` that no manifest entry tracks, sorted and
/// de-duplicated.
///
/// A path is tracked when it is a clip's audio file or one of its stems (both
/// can carry an audio extension). `on_disk` is the caller's walk of the
/// destination filtered to audio files, so sidecars, folder art, playlists, and
/// the hidden `.suno-*` files never appear and need no exclusion here. The
/// result is a pure function of its inputs: no filesystem, network, or clock.
///
/// Tracked and on-disk paths are compared by their filesystem-canonical key
/// (NFC + lowercase, see `canonical_path_key`), so a tracked file the walk
/// recorded under a different case or Unicode normalisation is not mis-reported
/// as an orphan on a case-insensitive or NFC-folding filesystem.
pub fn untracked_audio(manifest: &Manifest, on_disk: &[String]) -> Vec<String> {
    let tracked: BTreeSet<String> = manifest
        .iter()
        .flat_map(|(_, entry)| std::iter::once(entry.path.as_str()).chain(entry.artifact_paths()))
        .map(canonical_path_key)
        .collect();
    let mut orphans: Vec<String> = on_disk
        .iter()
        .filter(|path| !tracked.contains(&canonical_path_key(path)))
        .cloned()
        .collect();
    orphans.sort();
    orphans.dedup();
    orphans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioFormat;
    use crate::manifest::{ArtifactState, ManifestEntry};

    fn entry(path: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.to_owned(),
            format: AudioFormat::Flac,
            ..Default::default()
        }
    }

    #[test]
    fn tracked_file_with_case_or_nfc_mismatch_is_not_an_orphan() {
        // #269: on a case-insensitive or NFC-folding filesystem the walk can
        // record a tracked file under a different case or Unicode normalisation.
        // The canonical comparison keeps it from being mis-reported as an orphan.
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("Creator/Song.flac"));
        manifest.insert("b", entry("Creator/\u{00e9}toile.flac")); // é as NFC
        let on_disk = vec![
            "Creator/song.flac".to_owned(),           // same file, different case
            "Creator/e\u{0301}toile.flac".to_owned(), // same file, NFD encoding
        ];
        assert!(untracked_audio(&manifest, &on_disk).is_empty());
    }

    #[test]
    fn lists_audio_not_in_any_tracked_path() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("Artist/Album/a.flac"));
        // A file the user moved by hand: on disk but not the tracked path.
        let on_disk = vec![
            "Artist/Album/a.flac".to_owned(),
            "Moved/somewhere else.flac".to_owned(),
        ];
        assert_eq!(
            untracked_audio(&manifest, &on_disk),
            vec!["Moved/somewhere else.flac".to_owned()]
        );
    }

    #[test]
    fn excludes_tracked_audio_and_stems() {
        let mut manifest = Manifest::new();
        let mut e = entry("a.flac");
        e.stems.insert(
            "voc".to_owned(),
            ArtifactState {
                path: "a.stems/voc.wav".to_owned(),
                hash: "h".to_owned(),
            },
        );
        manifest.insert("a", e);
        // Both the audio and its stem are tracked, so neither is an orphan.
        let on_disk = vec!["a.flac".to_owned(), "a.stems/voc.wav".to_owned()];
        assert!(untracked_audio(&manifest, &on_disk).is_empty());
    }

    #[test]
    fn empty_when_every_file_is_tracked() {
        let mut manifest = Manifest::new();
        manifest.insert("a", entry("a.flac"));
        manifest.insert("b", entry("b.mp3"));
        let on_disk = vec!["a.flac".to_owned(), "b.mp3".to_owned()];
        assert!(untracked_audio(&manifest, &on_disk).is_empty());
    }

    #[test]
    fn output_is_sorted_and_deduplicated() {
        let manifest = Manifest::new();
        let on_disk = vec![
            "z.flac".to_owned(),
            "a.flac".to_owned(),
            "a.flac".to_owned(),
            "m.mp3".to_owned(),
        ];
        assert_eq!(
            untracked_audio(&manifest, &on_disk),
            vec!["a.flac".to_owned(), "m.mp3".to_owned(), "z.flac".to_owned()]
        );
    }

    #[test]
    fn empty_manifest_reports_all_on_disk_audio() {
        // A missing/empty manifest (nothing tracked) makes every on-disk file an
        // orphan; the report never crashes on empty input.
        let manifest = Manifest::new();
        assert!(untracked_audio(&manifest, &[]).is_empty());
        assert_eq!(
            untracked_audio(&manifest, &["only.flac".to_owned()]),
            vec!["only.flac".to_owned()]
        );
    }
}
