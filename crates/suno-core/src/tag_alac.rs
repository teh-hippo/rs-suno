//! ALAC (Apple Lossless) tagging via `mp4ameta`, working entirely in memory.
//!
//! Mirrors [`tag_flac`](crate::tag_flac): the same [`TrackMetadata`] fields are
//! written, standard ones as iTunes atoms and the Suno-specific ones (plus the
//! precise `DATE`) as freeform `com.apple.iTunes` atoms, with the cover as the
//! `covr` artwork. The MP4 is read and rewritten over an in-memory `Cursor`, so
//! the engine stays free of direct IO.

use std::io::Cursor;

use mp4ameta::{Data, FreeformIdent, Img, Tag};

use crate::error::{Error, Result};
use crate::tag::{Cover, TrackMetadata};

/// The iTunes reverse-DNS mean for the freeform (`----`) atoms.
const APPLE_ITUNES_MEAN: &str = "com.apple.iTunes";

/// Tag `audio` (an ALAC/MP4 byte stream) with `meta`, returning the tagged bytes.
///
/// Sets the standard iTunes atoms (title, artist, album, album artist, year via
/// `©day`, comment via `©cmt`, and lyrics via `©lyr`), the precise `DATE` and the
/// eight Suno fields as freeform atoms, and embeds `cover` as the `covr` artwork.
/// The MP4 structure is read from and rewritten into an in-memory cursor.
///
/// `mp4ameta` models `covr` artwork as JPEG/PNG/BMP only, so the ALAC path never
/// embeds an animated WebP; the executor always hands this a static JPEG.
pub fn tag_alac(audio: &[u8], meta: &TrackMetadata, cover: Option<Cover<'_>>) -> Result<Vec<u8>> {
    let mut file = Cursor::new(audio.to_vec());
    let mut tag = Tag::read_from(&mut file)
        .map_err(|err| Error::Tag(format!("could not read MP4 metadata: {err}")))?;

    // Start from a clean slate: ffmpeg copies the source WAV's metadata into the
    // transcoded MP4 by default, so drop every existing atom before writing ours
    // (mirrors tag_flac replacing the Vorbis comments).
    tag.clear_meta_items();

    if !meta.title.is_empty() {
        tag.set_title(meta.title.clone());
    }
    if !meta.artist.is_empty() {
        tag.set_artist(meta.artist.clone());
    }
    if !meta.album.is_empty() {
        tag.set_album(meta.album.clone());
    }
    if !meta.album_artist.is_empty() {
        tag.set_album_artist(meta.album_artist.clone());
    }
    if !meta.year.is_empty() {
        tag.set_year(meta.year.clone());
    }
    if meta.track > 0 {
        tag.set_track_number(track_atom_value(meta.track));
        if meta.track_total > 0 {
            tag.set_total_tracks(track_atom_value(meta.track_total));
        }
    }
    if !meta.comment.is_empty() {
        tag.set_comment(meta.comment.clone());
    }
    if !meta.lyrics.is_empty() {
        tag.set_lyrics(meta.lyrics.clone());
    }

    set_freeform(&mut tag, "DATE", &meta.date);
    for (name, value) in meta.suno_fields() {
        set_freeform(&mut tag, name, value);
    }

    if let Some(cover) = cover {
        tag.set_artwork(Img::jpeg(cover.bytes.to_vec()));
    }

    tag.write_to(&mut file)
        .map_err(|err| Error::Tag(format!("could not write MP4 metadata: {err}")))?;
    Ok(file.into_inner())
}

/// Clamp a `u32` lineage album index to the `u16` an MP4 track atom holds,
/// saturating at [`u16::MAX`] so an index above 65535 clamps rather than
/// wrapping (a plain `as u16` cast would silently truncate it).
fn track_atom_value(index: u32) -> u16 {
    u16::try_from(index).unwrap_or(u16::MAX)
}

/// Set a freeform `com.apple.iTunes` text atom, skipping an empty value.
fn set_freeform(tag: &mut Tag, name: &'static str, value: &str) {
    if !value.is_empty() {
        tag.set_data(
            FreeformIdent::new_static(APPLE_ITUNES_MEAN, name),
            Data::Utf8(value.to_owned()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_rather_than_panics_on_non_mp4_input() {
        // Untrusted bytes must yield an error, never a panic (mp4ameta cannot
        // parse a non-MP4 stream), and the message must not leak the input.
        let err = tag_alac(b"this is not an mp4 file", &TrackMetadata::default(), None)
            .expect_err("garbage input must not tag");
        assert!(matches!(err, Error::Tag(_)));
    }

    #[test]
    fn track_atom_value_clamps_above_u16_max() {
        // A lineage album index beyond u16 must clamp to u16::MAX, never wrap:
        // the old `meta.track as u16` cast truncated 70000 to 4464, corrupting
        // the track number written into the MP4 atom.
        assert_eq!(track_atom_value(70_000), u16::MAX);
        assert_eq!(track_atom_value(u32::from(u16::MAX) + 1), u16::MAX);
        assert_eq!(track_atom_value(u32::MAX), u16::MAX);
        // In-range indices pass through unchanged.
        assert_eq!(track_atom_value(0), 0);
        assert_eq!(track_atom_value(7), 7);
        assert_eq!(track_atom_value(u32::from(u16::MAX)), u16::MAX);
    }

    /// Proves the real pipeline: an ffmpeg-produced ALAC `.m4a` round-trips its
    /// standard atoms, a freeform Suno field, and the cover through `tag_alac`.
    /// Ignored because CI has no ffmpeg; run locally with
    /// `cargo test -p suno-core -- --ignored`.
    #[test]
    #[ignore = "requires ffmpeg"]
    fn round_trips_tags_and_cover() {
        use std::process::Command;

        let dir = std::path::Path::new("target").join("tag-alac-smoke");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.m4a");
        let made = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:a",
                "alac",
                "-f",
                "ipod",
            ])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let audio = std::fs::read(&src).unwrap();

        let meta = TrackMetadata {
            title: "Neon Horizon".to_owned(),
            artist: "Alice".to_owned(),
            album: "Nights".to_owned(),
            album_artist: "Alice".to_owned(),
            date: "2026-07-05".to_owned(),
            year: "2026".to_owned(),
            lyrics: "la la la".to_owned(),
            comment: "a description".to_owned(),
            prompt: "a synthwave anthem".to_owned(),
            ..Default::default()
        };
        let cover = b"\xff\xd8\xff\xe0jpeg-bytes".to_vec();
        let tagged = tag_alac(&audio, &meta, Some(Cover::jpeg(&cover))).unwrap();

        let tag = Tag::read_from(&mut Cursor::new(tagged)).unwrap();
        assert_eq!(tag.title(), Some("Neon Horizon"));
        assert_eq!(tag.artist(), Some("Alice"));
        assert_eq!(tag.album(), Some("Nights"));
        assert_eq!(tag.album_artist(), Some("Alice"));
        assert_eq!(tag.year(), Some("2026"));
        assert_eq!(tag.lyrics(), Some("la la la"));
        let prompt = FreeformIdent::new_static(APPLE_ITUNES_MEAN, "SUNO_PROMPT");
        assert_eq!(tag.strings_of(&prompt).next(), Some("a synthwave anthem"));
        let date = FreeformIdent::new_static(APPLE_ITUNES_MEAN, "DATE");
        assert_eq!(tag.strings_of(&date).next(), Some("2026-07-05"));
        assert!(tag.artwork().is_some());

        let _ = std::fs::remove_file(&src);
    }
}
