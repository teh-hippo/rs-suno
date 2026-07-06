//! Filesystem-canonical path keys for case- and normalisation-insensitive
//! comparison.
//!
//! Case-insensitive (Windows, macOS by default) and NFC-normalising filesystems
//! treat two paths that differ only by letter case or Unicode normalisation (NFC
//! vs NFD) as one file. The pure engine cannot probe the target filesystem, so
//! it compares paths through a single canonical key — NFC then lowercase — that
//! matches the collision model the namer already uses when it de-collides
//! rendered names. Sharing the key keeps the reconciler's deletion, rename, and
//! relocation decisions aligned with the names the namer produces, closing the
//! gap where a namer-collapsed pair still looked distinct to the reconciler.

use unicode_normalization::UnicodeNormalization as _;

/// The filesystem-canonical key for a path: NFC-normalise, then lowercase, so
/// paths differing only by case or by NFC/NFD encoding map to the same key.
pub(crate) fn canonical_path_key(path: &str) -> String {
    path.nfc().flat_map(char::to_lowercase).collect()
}

/// Whether two paths name the same file on a case-insensitive or
/// NFC-normalising filesystem, i.e. they are equal once canonicalised.
pub(crate) fn same_fs_path(a: &str, b: &str) -> bool {
    canonical_path_key(a) == canonical_path_key(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_only_difference_is_the_same_key() {
        assert_eq!(
            canonical_path_key("Creator/Song.flac"),
            canonical_path_key("creator/song.flac")
        );
        assert!(same_fs_path("Creator/Song.flac", "creator/song.flac"));
    }

    #[test]
    fn nfc_and_nfd_encodings_share_a_key() {
        // "é" as NFC (U+00E9) vs NFD (e + U+0301).
        assert!(same_fs_path("\u{00e9}toile.mp3", "e\u{0301}toile.mp3"));
    }

    #[test]
    fn distinct_paths_do_not_alias() {
        assert!(!same_fs_path("Creator/Alpha.flac", "Creator/Beta.flac"));
    }

    #[test]
    fn empty_paths_are_equal_and_do_not_panic() {
        assert!(same_fs_path("", ""));
        assert_eq!(canonical_path_key(""), "");
    }
}
