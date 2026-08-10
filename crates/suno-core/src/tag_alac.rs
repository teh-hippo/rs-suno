//! ALAC (Apple Lossless) tagging via `mp4ameta`, working entirely in memory.
//!
//! Mirrors [`tag_flac`](crate::tag_flac): the same [`TrackMetadata`] fields are
//! written, standard ones as iTunes atoms and the Suno-specific ones (plus the
//! precise `DATE`) as freeform `com.apple.iTunes` atoms, with the cover as the
//! `covr` artwork. The MP4 is read and rewritten over an in-memory `Cursor`, so
//! the engine stays free of direct IO.

use std::io::Cursor;

use mp4ameta::{Data, FreeformIdent, Img, Tag, Userdata};

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
    let mut tag = read_tag(&mut file)?;
    let existing_lyrics = tag.lyrics().map(str::to_owned);

    // Start from a clean slate: ffmpeg copies the source WAV's metadata into the
    // transcoded MP4 by default, so drop every existing atom before writing ours
    // (mirrors tag_flac replacing the Vorbis comments).
    tag.clear_meta_items();
    if let Some(existing_lyrics) = existing_lyrics {
        // Restore only the lyrics, which the shared policy below overwrites when
        // this run has its own.
        tag.set_lyrics(existing_lyrics);
    }

    apply_owned_atoms(&mut tag, meta, cover, true);

    write_tag(&tag, &mut file)?;
    Ok(file.into_inner())
}

/// Retag an existing ALAC/MP4 in place, touching only the atoms this crate owns.
///
/// Unlike [`tag_alac`], which clears the metadata ffmpeg copies into a freshly
/// transcoded file, this merges into what is already there: unknown atoms and
/// other tools' freeform atoms survive, and so does the artwork when no
/// replacement is supplied and `preserve_existing_cover` is set. A run that
/// leaves the metadata unchanged returns the original bytes rather than
/// rewriting them (#537).
pub fn retag_alac(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    preserve_existing_cover: bool,
) -> Result<Vec<u8>> {
    let mut file = Cursor::new(audio.to_vec());
    let mut tag = read_tag(&mut file)?;
    let original = tag.clone();

    apply_owned_atoms(&mut tag, meta, cover, preserve_existing_cover);

    if tag == original {
        return Ok(audio.to_vec());
    }
    write_tag(&tag, &mut file)?;
    Ok(file.into_inner())
}

/// Read the MP4 metadata, mapping a parse failure to a tag error that never
/// echoes the input bytes.
///
/// `mp4ameta` 0.13 divides by the `mvhd` timescale while reading, so an MP4
/// whose header carries a zero timescale panics inside the crate. Contain that
/// third-party panic here, as [`tag_flac`](crate::tag_flac) does for `metaflac`,
/// so a corrupt on-disk `.m4a` read during a retag returns an error rather than
/// crashing the run. This relies on the crate not being built with
/// `panic = "abort"` (no profile sets it), and `AssertUnwindSafe` is sound
/// because the cursor is dropped on the error path.
fn read_tag(file: &mut Cursor<Vec<u8>>) -> Result<Tag> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Tag::read_from(file))) {
        Ok(Ok(tag)) => Ok(tag),
        Ok(Err(err)) => Err(Error::Tag(format!("could not read MP4 metadata: {err}"))),
        Err(_) => Err(Error::Tag("could not read MP4 metadata".to_owned())),
    }
}

/// Write the MP4 metadata back over the same cursor.
fn write_tag(tag: &Tag, file: &mut Cursor<Vec<u8>>) -> Result<()> {
    tag.write_to(file)
        .map_err(|err| Error::Tag(format!("could not write MP4 metadata: {err}")))
}

/// Write this crate's atoms into `tag`, removing the owned ones it has no value
/// for and leaving every other atom alone.
///
/// Lyrics are the exception to "empty means remove": a run with no lyrics keeps
/// whatever is embedded, so it never strips a good file. Artwork is replaced
/// only when `cover` is supplied, and cleared when it is not and
/// `preserve_existing_cover` is unset.
fn apply_owned_atoms(
    tag: &mut Userdata,
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    preserve_existing_cover: bool,
) {
    if meta.title.is_empty() {
        tag.remove_title();
    } else {
        tag.set_title(meta.title.clone());
    }
    if meta.artist.is_empty() {
        tag.remove_artists();
    } else {
        tag.set_artist(meta.artist.clone());
    }
    if meta.album.is_empty() {
        tag.remove_album();
    } else {
        tag.set_album(meta.album.clone());
    }
    if meta.album_artist.is_empty() {
        tag.remove_album_artists();
    } else {
        tag.set_album_artist(meta.album_artist.clone());
    }
    if meta.year.is_empty() {
        tag.remove_year();
    } else {
        tag.set_year(meta.year.clone());
    }
    if meta.track > 0 {
        tag.set_track_number(track_atom_value(meta.track));
        if meta.track_total > 0 {
            tag.set_total_tracks(track_atom_value(meta.track_total));
        } else {
            tag.remove_total_tracks();
        }
    } else {
        tag.remove_track();
    }
    if meta.comment.is_empty() {
        tag.remove_comments();
    } else {
        tag.set_comment(meta.comment.clone());
    }
    if !meta.lyrics.is_empty() {
        tag.set_lyrics(meta.lyrics.clone());
    }

    set_freeform(tag, "DATE", &meta.date);
    for (name, value) in meta.suno_fields() {
        set_freeform(tag, name, value);
    }

    match cover {
        Some(cover) => tag.set_artwork(Img::jpeg(cover.bytes.to_vec())),
        None => {
            if !preserve_existing_cover {
                tag.remove_artworks();
            }
        }
    }
}

/// Clamp a `u32` lineage album index to the `u16` an MP4 track atom holds,
/// saturating at [`u16::MAX`] so an index above 65535 clamps rather than
/// wrapping (a plain `as u16` cast would silently truncate it).
fn track_atom_value(index: u32) -> u16 {
    u16::try_from(index).unwrap_or(u16::MAX)
}

/// Set a freeform `com.apple.iTunes` text atom, removing it when the value is
/// empty so a retag clears a field this crate no longer has a value for.
fn set_freeform(tag: &mut Userdata, name: &'static str, value: &str) {
    let ident = FreeformIdent::new_static(APPLE_ITUNES_MEAN, name);
    if value.is_empty() {
        tag.remove_data_of(&ident);
    } else {
        tag.set_data(ident, Data::Utf8(value.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap `body` in an MPEG-4 box of the given kind.
    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    /// The stand-in audio payload inside the fixture's `mdat` box.
    const MDAT_PAYLOAD: &[u8] = b"alac-sample-payload";

    /// Build a minimal but readable MPEG-4 container: an `ftyp`, a `moov`
    /// holding only an `mvhd`, and an `mdat`. Enough for `mp4ameta` to parse,
    /// tag, and round-trip in memory, without invoking ffmpeg.
    fn minimal_mp4() -> Vec<u8> {
        let mut ftyp = b"M4A ".to_vec();
        ftyp.extend_from_slice(&0u32.to_be_bytes());
        ftyp.extend_from_slice(b"M4A mp42isom");

        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes()); // timescale
        mvhd[16..20].copy_from_slice(&1000u32.to_be_bytes()); // duration

        let mut out = boxed(b"ftyp", &ftyp);
        out.extend_from_slice(&boxed(b"moov", &boxed(b"mvhd", &mvhd)));
        out.extend_from_slice(&boxed(b"mdat", MDAT_PAYLOAD));
        out
    }

    /// A `TrackMetadata` covering every field this crate writes.
    fn full_meta() -> TrackMetadata {
        TrackMetadata {
            title: "Neon Horizon".to_owned(),
            artist: "Alice".to_owned(),
            album: "Nights".to_owned(),
            album_artist: "Alice".to_owned(),
            date: "2026-07-05".to_owned(),
            year: "2026".to_owned(),
            lyrics: "la la la".to_owned(),
            comment: "a description".to_owned(),
            prompt: "a synthwave anthem".to_owned(),
            style: "synthwave".to_owned(),
            id: "clip-1234".to_owned(),
            ..Default::default()
        }
    }

    /// An MP4 carrying metadata this crate does not own: a genre, another
    /// tool's freeform atom, and artwork, as ffmpeg copies across a transcode.
    fn mp4_with_foreign_atoms() -> Vec<u8> {
        let mut file = Cursor::new(minimal_mp4());
        let mut tag = Tag::read_from(&mut file).unwrap();
        tag.set_genre("Ambient");
        tag.set_data(
            FreeformIdent::new_static("com.example.tagger", "MOOD"),
            Data::Utf8("calm".to_owned()),
        );
        tag.set_artwork(Img::jpeg(b"\xFF\xD8\xFFexisting-art".to_vec()));
        tag.write_to(&mut file).unwrap();
        file.into_inner()
    }

    /// Read the metadata back out of a tagged MP4.
    fn read_back(audio: &[u8]) -> Tag {
        Tag::read_from(&mut Cursor::new(audio.to_vec())).unwrap()
    }

    /// The value of a `com.apple.iTunes` freeform atom, if present.
    fn freeform(tag: &Tag, name: &'static str) -> Option<String> {
        tag.strings_of(&FreeformIdent::new_static(APPLE_ITUNES_MEAN, name))
            .next()
            .map(str::to_owned)
    }

    #[test]
    fn fresh_tagging_clears_copied_metadata_and_writes_ours() {
        // ffmpeg copies the source metadata into the transcoded MP4, so the
        // fresh path must start from a clean slate.
        let tagged = tag_alac(&mp4_with_foreign_atoms(), &full_meta(), None).unwrap();
        let tag = read_back(&tagged);

        assert_eq!(tag.genre(), None, "copied metadata is cleared");
        assert!(
            tag.strings_of(&FreeformIdent::new_static("com.example.tagger", "MOOD"))
                .next()
                .is_none(),
            "a copied freeform atom is cleared"
        );
        assert!(tag.artwork().is_none(), "copied artwork is cleared");
        assert_eq!(tag.title(), Some("Neon Horizon"));
        assert_eq!(
            freeform(&tag, "SUNO_PROMPT").as_deref(),
            Some("a synthwave anthem")
        );
        assert!(
            tagged
                .windows(MDAT_PAYLOAD.len())
                .any(|window| window == MDAT_PAYLOAD),
            "the audio payload survives"
        );
    }

    #[test]
    fn retag_merges_and_preserves_unknown_atoms_and_artwork() {
        let retagged = retag_alac(&mp4_with_foreign_atoms(), &full_meta(), None, true).unwrap();
        let tag = read_back(&retagged);

        assert_eq!(tag.genre(), Some("Ambient"), "an unowned atom survives");
        assert_eq!(
            tag.strings_of(&FreeformIdent::new_static("com.example.tagger", "MOOD"))
                .next(),
            Some("calm"),
            "another tool's freeform atom survives"
        );
        assert_eq!(
            tag.artwork().map(|img| img.data.to_vec()),
            Some(b"\xFF\xD8\xFFexisting-art".to_vec()),
            "artwork survives when no replacement is supplied"
        );
        assert_eq!(tag.title(), Some("Neon Horizon"), "our fields are written");
        assert_eq!(freeform(&tag, "DATE").as_deref(), Some("2026-07-05"));
    }

    #[test]
    fn retag_returns_original_bytes_when_nothing_changed() {
        let meta = full_meta();
        let first = tag_alac(&minimal_mp4(), &meta, None).unwrap();
        let retagged = retag_alac(&first, &meta, None, true).unwrap();
        assert_eq!(retagged, first, "a semantic no-op does not rewrite bytes");
    }

    #[test]
    fn retag_rewrites_and_replaces_a_changed_field() {
        let meta = full_meta();
        let first = tag_alac(&minimal_mp4(), &meta, None).unwrap();
        let mut changed = meta.clone();
        changed.album = "Days".to_owned();
        let retagged = retag_alac(&first, &changed, None, true).unwrap();

        assert_ne!(retagged, first);
        let tag = read_back(&retagged);
        assert_eq!(tag.album(), Some("Days"));
        assert_eq!(
            tag.strings_of(&mp4ameta::ident::ALBUM).count(),
            1,
            "replaced, not stacked"
        );
    }

    #[test]
    fn retag_clears_owned_fields_that_lost_their_value() {
        let meta = full_meta();
        let first = tag_alac(&minimal_mp4(), &meta, Some(Cover::jpeg(b"\xFF\xD8\xFFart"))).unwrap();
        let mut cleared = meta.clone();
        cleared.album = String::new();
        cleared.style = String::new();
        let retagged = retag_alac(&first, &cleared, None, true).unwrap();

        let tag = read_back(&retagged);
        assert_eq!(tag.album(), None, "an owned atom with no value is removed");
        assert_eq!(
            freeform(&tag, "SUNO_STYLE"),
            None,
            "an owned freeform atom with no value is removed"
        );
        assert_eq!(tag.title(), Some("Neon Horizon"), "the rest is intact");
        assert!(tag.artwork().is_some(), "artwork is still preserved");
    }

    #[test]
    fn retag_keeps_lyrics_when_the_run_has_none() {
        let meta = full_meta();
        let first = tag_alac(&minimal_mp4(), &meta, None).unwrap();
        let mut without = meta.clone();
        without.lyrics = String::new();
        let retagged = retag_alac(&first, &without, None, true).unwrap();
        assert_eq!(read_back(&retagged).lyrics(), Some("la la la"));
    }

    #[test]
    fn retag_replaces_or_clears_artwork_on_request() {
        let meta = full_meta();
        let first = tag_alac(&minimal_mp4(), &meta, Some(Cover::jpeg(b"\xFF\xD8\xFFold"))).unwrap();

        let replaced =
            retag_alac(&first, &meta, Some(Cover::jpeg(b"\xFF\xD8\xFFnew")), false).unwrap();
        assert_eq!(
            read_back(&replaced).artwork().map(|img| img.data.to_vec()),
            Some(b"\xFF\xD8\xFFnew".to_vec())
        );

        let cleared = retag_alac(&first, &meta, None, false).unwrap();
        assert!(
            read_back(&cleared).artwork().is_none(),
            "art is dropped when there is none to keep and none to write"
        );
    }

    #[test]
    fn errors_rather_than_panics_on_non_mp4_input() {
        // Untrusted bytes must yield an error, never a panic (mp4ameta cannot
        // parse a non-MP4 stream), and the message must not leak the input.
        let err = tag_alac(b"this is not an mp4 file", &TrackMetadata::default(), None)
            .expect_err("garbage input must not tag");
        assert!(matches!(err, Error::Tag(_)));

        // A structurally valid MP4 whose header carries a zero timescale makes
        // mp4ameta divide by zero; the guard must turn that into an error.
        let mut zero_timescale = minimal_mp4();
        let at = zero_timescale
            .windows(4)
            .position(|window| window == b"mvhd")
            .expect("the fixture has an mvhd box")
            + 4;
        zero_timescale[at + 12..at + 16].copy_from_slice(&0u32.to_be_bytes());
        for tagged in [
            tag_alac(&zero_timescale, &TrackMetadata::default(), None),
            retag_alac(&zero_timescale, &TrackMetadata::default(), None, true),
        ] {
            assert!(matches!(tagged, Err(Error::Tag(_))));
        }

        let err = retag_alac(b"still not an mp4", &TrackMetadata::default(), None, true)
            .expect_err("garbage input must not retag");
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

        let tag = Tag::read_from(&mut Cursor::new(tagged.as_slice())).unwrap();
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

        let mut retag_meta = meta;
        retag_meta.lyrics.clear();
        let retagged = tag_alac(&tagged, &retag_meta, None).unwrap();
        let retag = Tag::read_from(&mut Cursor::new(retagged)).unwrap();
        assert_eq!(
            retag.lyrics(),
            Some("la la la"),
            "an unrelated retag preserves existing lyrics"
        );

        // The surgical retag keeps the artwork it was given nothing to replace,
        // and a run that changes nothing leaves the file byte-identical.
        let kept = retag_alac(&tagged, &retag_meta, None, true).unwrap();
        let kept_tag = Tag::read_from(&mut Cursor::new(kept.clone())).unwrap();
        assert_eq!(kept_tag.artwork().map(|img| img.data.to_vec()), Some(cover));
        assert_eq!(kept_tag.lyrics(), Some("la la la"));
        assert_eq!(
            retag_alac(&kept, &retag_meta, None, true).unwrap(),
            kept,
            "a semantic no-op does not rewrite a real ALAC"
        );

        let _ = std::fs::remove_file(&src);
    }
}
