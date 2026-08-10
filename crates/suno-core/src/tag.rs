//! Track metadata and pure, byte-to-byte audio tagging.
//!
//! [`TrackMetadata`] is the tag set derived from a [`Clip`], mirroring the
//! `ha-suno` reference. [`tag_mp3`], [`tag_flac`], and [`tag_wav`] take audio
//! bytes plus metadata and return tagged bytes, working entirely in memory so
//! the engine stays free of direct IO and the logic is unit-testable without a
//! network.

use std::io::Cursor;

use id3::TagLike;
use id3::frame::{
    Comment, Content, ExtendedText, Frame, Lyrics, Picture, PictureType, SynchronisedLyrics,
    SynchronisedLyricsType, TimestampFormat,
};

use crate::consts::SUNO_SONG_BASE_URL;
use crate::error::{Error, Result};
use crate::lineage::{EdgeType, LineageContext};
use crate::lyrics::AlignedLyrics;
use crate::model::Clip;
use crate::vocab::LyricsTiming;

mod flac;

pub use flac::retag_flac;

const LANG: &str = "eng";

/// The standard ID3 text frames this crate writes and therefore owns on a
/// retag. Any other text frame (genre, composer, encoder, and so on) belongs to
/// the user or another tool and is carried through untouched.
const OWNED_TEXT_FRAMES: [&str; 7] = ["TIT2", "TPE1", "TALB", "TPE2", "TDRC", "TDRL", "TRCK"];

/// An embedded cover image: its raw bytes paired with its MIME type. The MIME
/// travels with the bytes so the executor can embed an animated `image/webp`
/// where the container allows it (FLAC within its size cap, MP3, WAV) or a
/// static `image/jpeg` otherwise.
#[derive(Debug, Clone, Copy)]
pub struct Cover<'a> {
    pub bytes: &'a [u8],
    pub mime: &'a str,
}

impl<'a> Cover<'a> {
    /// A static JPEG cover.
    pub fn jpeg(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            mime: "image/jpeg",
        }
    }

    /// An animated WebP cover.
    pub fn webp(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            mime: "image/webp",
        }
    }
}

/// A FLAC metadata block length is a 24-bit field, so a single block body cannot
/// exceed this. `metaflac` silently truncates the length on overflow (writing a
/// corrupt file whose length prefix disagrees with the body, which then panics
/// its own reader), so [`tag_flac`] refuses any picture that would exceed it.
const FLAC_METADATA_BLOCK_MAX: usize = (1 << 24) - 1;

/// Fixed overhead of a FLAC PICTURE block body, excluding the MIME and
/// description strings and the image data: eight 4-byte fields (picture type,
/// MIME length, description length, width, height, colour depth, colour count,
/// and data length).
const FLAC_PICTURE_FIXED_OVERHEAD: usize = 32;

/// The largest image-data payload that fits a FLAC PICTURE block for a cover of
/// the given `mime` and an empty description. The executor uses this to decide
/// whether an encoded animated WebP fits FLAC, and [`tag_flac`] enforces it.
pub fn flac_picture_data_budget(mime: &str) -> usize {
    FLAC_METADATA_BLOCK_MAX - FLAC_PICTURE_FIXED_OVERHEAD - mime.len()
}

/// The metadata tags written into a downloaded audio file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub date: String,
    /// The album's release year (`YYYY`): the lineage root's creation year, or
    /// the clip's own year when the root's is unavailable.
    pub year: String,
    pub lyrics: String,
    pub prompt: String,
    pub comment: String,
    pub style: String,
    pub style_summary: String,
    pub model: String,
    pub handle: String,
    pub parent: String,
    pub root: String,
    pub lineage: String,
    /// The clip's own Suno id, embedded as `SUNO_ID`.
    pub id: String,
    /// The canonical `https://suno.com/song/<id>` page URL, embedded as
    /// `SUNO_URL`. Empty when the clip has no id.
    pub url: String,
    /// This track's 1-based position within its lineage album, or `0` when
    /// unnumbered. Written as `TRACKNUMBER`/`TRCK`/`trkn`.
    pub track: u32,
    /// The album's track count paired with [`track`](Self::track), or `0` when
    /// unnumbered.
    pub track_total: u32,
}

impl TrackMetadata {
    /// Map a [`Clip`] plus its resolved [`LineageContext`] to its tag set,
    /// mirroring `ha-suno`'s `to_track_metadata`.
    ///
    /// `artist` and `album_artist` fall back to `"Suno"`, and `date` is the
    /// `YYYY-MM-DD` prefix of `created_at`. `year` is the lineage root's
    /// creation year (the clip's own year when the root's is unavailable), so an
    /// album whose tracks cross a calendar boundary groups under one year. The
    /// `album`, `parent`, `root`, and `lineage` tags come from the resolved
    /// context, not the clip's own `edited_clip_id` pointer, and the generation
    /// `prompt` is preserved in its own `SUNO_PROMPT` tag.
    pub fn from_clip(clip: &Clip, lineage: &LineageContext) -> TrackMetadata {
        let artist = non_empty(&clip.display_name).unwrap_or("Suno").to_owned();
        let album = lineage.album(&clip.title);
        TrackMetadata {
            title: clip.title.clone(),
            artist: artist.clone(),
            album,
            album_artist: artist,
            date: first_chars(&clip.created_at, 10),
            year: lineage.year(&clip.created_at),
            lyrics: clip.lyrics.clone(),
            prompt: clip.prompt.clone(),
            comment: clip.gpt_description_prompt.clone(),
            style: clip.tags.clone(),
            style_summary: clip.gpt_description_prompt.clone(),
            model: model_label(clip),
            handle: clip.handle.clone(),
            parent: lineage.parent_id.clone(),
            root: lineage.root_id.clone(),
            lineage: lineage_summary(clip, lineage),
            id: clip.id.clone(),
            url: song_url(&clip.id),
            track: lineage.track,
            track_total: lineage.track_total,
        }
    }

    /// Map a clip to its tag set, filling otherwise-missing plain lyrics from
    /// Suno's aligned-lyrics response.
    ///
    /// Inline clip lyrics remain authoritative when present. Alignment is only
    /// a fallback for real-feed clips whose top-level `lyrics` field is empty;
    /// its timing is handled separately by the ID3 `SYLT` path.
    pub fn from_clip_with_alignment(
        clip: &Clip,
        lineage: &LineageContext,
        aligned: Option<&AlignedLyrics>,
    ) -> TrackMetadata {
        let mut meta = Self::from_clip(clip, lineage);
        if meta.lyrics.trim().is_empty()
            && let Some(aligned) = aligned
        {
            let text = aligned.plain_text();
            if !text.trim().is_empty() {
                meta.lyrics = text;
            }
        }
        meta
    }

    /// The standard metadata fields, paired with their Vorbis comment key.
    ///
    /// The FLAC path writes these (chained with [`suno_fields`](Self::suno_fields))
    /// as Vorbis comments; the ID3 and MP4 paths map the same values through
    /// their own typed setters.
    pub(crate) fn standard_fields(&self) -> [(&'static str, &str); 8] {
        [
            ("TITLE", &self.title),
            ("ARTIST", &self.artist),
            ("ALBUM", &self.album),
            ("ALBUMARTIST", &self.album_artist),
            ("DATE", &self.date),
            ("YEAR", &self.year),
            ("LYRICS", &self.lyrics),
            ("DESCRIPTION", &self.comment),
        ]
    }

    /// The Suno-specific fields, paired with their tag description/key.
    pub(crate) fn suno_fields(&self) -> [(&'static str, &str); 10] {
        [
            ("SUNO_PROMPT", &self.prompt),
            ("SUNO_STYLE", &self.style),
            ("SUNO_STYLE_SUMMARY", &self.style_summary),
            ("SUNO_MODEL", &self.model),
            ("SUNO_HANDLE", &self.handle),
            ("SUNO_PARENT", &self.parent),
            ("SUNO_ROOT", &self.root),
            ("SUNO_LINEAGE", &self.lineage),
            ("SUNO_ID", &self.id),
            ("SUNO_URL", &self.url),
        ]
    }
}

/// Tag `audio` (an MP3 byte stream) with `meta`, returning the tagged bytes.
///
/// Writes ID3v2.4 frames, replacing any existing ID3 tag, and embeds `cover`
/// as a front-cover `APIC` frame when provided. When `synced` carries aligned
/// lyrics, a line-level `SYLT` (synchronised lyrics) frame is added alongside
/// the plain `USLT` lyrics; an empty (instrumental) `synced` adds no `SYLT`.
///
/// Because the whole tag is rebuilt, any existing `SYLT`/`USLT` lyrics would be
/// lost on a plain retag that carries no new lyrics. To avoid downgrading a good
/// timed file, existing `SYLT` frames are preserved when `synced` is `None`, and
/// existing `USLT` frames are preserved when `meta` carries no lyrics text.
pub fn tag_mp3(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    synced: Option<&AlignedLyrics>,
) -> Result<Vec<u8>> {
    tag_mp3_with_timing(audio, meta, cover, synced, LyricsTiming::Line)
}

/// Tag an MP3, selecting line- or word-level `SYLT` timing.
pub fn tag_mp3_with_timing(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    synced: Option<&AlignedLyrics>,
    timing: LyricsTiming,
) -> Result<Vec<u8>> {
    tag_id3(audio, meta, cover, synced, timing, false, "ID3 tag")
}

/// Retag an existing MP3 in place, selecting line- or word-level `SYLT` timing.
///
/// Unlike [`tag_mp3_with_timing`], which builds a fresh tag for a newly
/// downloaded file, this keeps every frame this crate does not own (see
/// [`retag_id3`]).
pub(crate) fn retag_mp3_with_timing(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    synced: Option<&AlignedLyrics>,
    timing: LyricsTiming,
    preserve_existing_cover: bool,
) -> Result<Vec<u8>> {
    retag_id3(
        audio,
        meta,
        cover,
        synced,
        timing,
        preserve_existing_cover,
        "ID3 tag",
    )
}

/// Build a line- or word-level `SYLT` frame from `aligned`, or `None` when there
/// is nothing to time (an instrumental with empty arrays).
///
/// Timestamps are absolute milliseconds ([`TimestampFormat::Ms`]); the content
/// is selected from the aligned line grouping, with a leading newline on each
/// line after the first so a player renders line breaks.
fn build_sylt(aligned: &AlignedLyrics, timing: LyricsTiming) -> Option<SynchronisedLyrics> {
    let content = aligned.sylt_entries_with_timing(timing);
    if content.is_empty() {
        return None;
    }
    Some(SynchronisedLyrics {
        lang: LANG.to_owned(),
        timestamp_format: TimestampFormat::Ms,
        content_type: SynchronisedLyricsType::Lyrics,
        description: String::new(),
        content,
    })
}

/// Tag `audio` (a FLAC byte stream) with `meta`, returning the tagged bytes.
///
/// Replaces the Vorbis comments, embeds `cover` as the sole front-cover
/// `PICTURE` block, and preserves the original `STREAMINFO` and audio frames.
/// Works in memory: the existing metadata blocks are rewritten and the audio
/// frames are appended unchanged.
///
/// A FLAC PICTURE block is bounded by a 24-bit length field, and `metaflac`
/// silently truncates that length on overflow (corrupting the file), so a cover
/// whose bytes exceed [`flac_picture_data_budget`] is rejected with an error
/// rather than embedded. The executor sizes the animated WebP to fit and falls
/// back to a static JPEG before reaching this guard, which is a backstop.
///
/// When `meta` carries no lyrics text (a plain retag), any existing `LYRICS`
/// comment is preserved rather than dropped, so a retag never loses embedded
/// lyrics.
pub fn tag_flac(audio: &[u8], meta: &TrackMetadata, cover: Option<Cover<'_>>) -> Result<Vec<u8>> {
    // metaflac 0.2.8 slices unchecked while parsing metadata blocks, so a
    // truncated or malformed FLAC panics inside the crate and unwinds straight
    // past the `.map_err` calls in `tag_flac_inner`. Contain that third-party
    // panic here so a corrupt on-disk `.flac` read during a retag returns an
    // error instead of crashing the whole run. This relies on the crate not
    // being built with panic = "abort" (no profile sets it). AssertUnwindSafe is
    // sound: the closure borrows only a `&[u8]` and a `&TrackMetadata`, neither
    // of which an unwind can leave observably broken.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tag_flac_inner(audio, meta, cover)
    }))
    .unwrap_or_else(|_| Err(Error::Tag("could not read FLAC metadata".to_owned())))
}

fn tag_flac_inner(audio: &[u8], meta: &TrackMetadata, cover: Option<Cover<'_>>) -> Result<Vec<u8>> {
    let mut tag = metaflac::Tag::read_from(&mut Cursor::new(audio))
        .map_err(|err| Error::Tag(format!("could not read FLAC metadata: {err}")))?;

    let existing_lyrics: Vec<String> = tag
        .get_vorbis("LYRICS")
        .map(|values| values.map(str::to_owned).collect())
        .unwrap_or_default();

    tag.remove_blocks(metaflac::BlockType::VorbisComment);
    for (key, value) in meta.standard_fields().into_iter().chain(meta.suno_fields()) {
        if !value.is_empty() {
            tag.set_vorbis(key, vec![value.to_owned()]);
        }
    }
    if meta.lyrics.is_empty() && !existing_lyrics.is_empty() {
        tag.set_vorbis("LYRICS", existing_lyrics);
    }
    if meta.track > 0 {
        tag.set_vorbis("TRACKNUMBER", vec![meta.track.to_string()]);
        if meta.track_total > 0 {
            tag.set_vorbis("TRACKTOTAL", vec![meta.track_total.to_string()]);
        }
    }
    if let Some(cover) = cover {
        let budget = flac_picture_data_budget(cover.mime);
        if cover.bytes.len() > budget {
            return Err(Error::Tag(format!(
                "cover image is {} bytes, over the {}-byte FLAC picture limit",
                cover.bytes.len(),
                budget
            )));
        }
        // Exactly one front cover: drop any existing picture before adding ours
        // (metaflac's add_picture already replaces the same type; this is
        // explicit so a stale picture of any type cannot linger).
        tag.remove_blocks(metaflac::BlockType::Picture);
        tag.add_picture(
            cover.mime,
            metaflac::block::PictureType::CoverFront,
            cover.bytes.to_vec(),
        );
    }

    let audio_frames = metaflac::Tag::skip_metadata(&mut Cursor::new(audio));
    let mut out = Vec::with_capacity(audio.len());
    tag.write_to(&mut out)
        .map_err(|err| Error::Tag(format!("could not write FLAC metadata: {err}")))?;
    out.extend_from_slice(&audio_frames);
    Ok(out)
}

/// Tag `audio` (a WAV byte stream) with `meta`, returning the tagged bytes.
///
/// Writes an ID3v2.4 tag into the RIFF `id3 ` chunk, replacing any existing
/// ID3 tag, and embeds `cover` as a front-cover `APIC` frame when provided.
/// When `synced` carries aligned lyrics, a line-level `SYLT` frame is added
/// alongside the plain `USLT` frame.
///
/// Existing `SYLT` frames are preserved when `synced` is `None`, and existing
/// `USLT` frames are preserved when `meta` carries no lyrics text, so a retag
/// never loses previously embedded lyrics.
pub fn tag_wav(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    synced: Option<&AlignedLyrics>,
) -> Result<Vec<u8>> {
    tag_wav_with_timing(audio, meta, cover, synced, LyricsTiming::Line)
}

/// Tag a WAV, selecting line- or word-level `SYLT` timing.
pub fn tag_wav_with_timing(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    synced: Option<&AlignedLyrics>,
    timing: LyricsTiming,
) -> Result<Vec<u8>> {
    tag_id3(audio, meta, cover, synced, timing, false, "WAV ID3 tag")
}

/// Retag an existing WAV in place, selecting line- or word-level `SYLT` timing.
///
/// Unlike [`tag_wav_with_timing`], which builds a fresh tag for a newly
/// downloaded file, this keeps every frame this crate does not own (see
/// [`retag_id3`]).
pub(crate) fn retag_wav_with_timing(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    synced: Option<&AlignedLyrics>,
    timing: LyricsTiming,
    preserve_existing_cover: bool,
) -> Result<Vec<u8>> {
    retag_id3(
        audio,
        meta,
        cover,
        synced,
        timing,
        preserve_existing_cover,
        "WAV ID3 tag",
    )
}

/// The shared ID3v2.4 tagging skeleton behind [`tag_mp3`] and [`tag_wav`], which
/// differ only in the `err_context` used to label a write failure.
///
/// Builds the frame set from `meta` (title, artist, album, recording/release
/// dates, comment, lyrics, and the Suno `TXXX` fields), embeds `cover` as a
/// front-cover `APIC`, and writes the selected `SYLT` from `synced`. Because the
/// tag is rebuilt from scratch, existing `USLT` lyrics are carried over when
/// `meta` carries no lyrics text and existing `SYLT` frames are carried over
/// when `synced` is `None`, so a fresh tag never downgrades a timed file.
fn tag_id3(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    synced: Option<&AlignedLyrics>,
    timing: LyricsTiming,
    preserve_existing_cover: bool,
    err_context: &str,
) -> Result<Vec<u8>> {
    let existing = id3::Tag::read_from2(Cursor::new(audio)).ok();
    let sylt = synced.and_then(|aligned| build_sylt(aligned, timing));
    let owned = OwnedId3::new(
        meta,
        cover.is_some(),
        sylt.is_some(),
        preserve_existing_cover,
    );

    let mut tag = id3::Tag::new();
    if let Some(existing) = &existing {
        // A fresh tag keeps only what this run cannot supply itself: embedded
        // lyrics, timed lyrics, and cover art.
        carry_frames(&mut tag, existing, &owned, Carry::LyricsAndCover);
    }
    apply_owned_id3(&mut tag, meta, cover, sylt);
    write_id3(&tag, audio, err_context)
}

/// The surgical retag behind [`retag_mp3_with_timing`] and
/// [`retag_wav_with_timing`].
///
/// Starts from the tag already in the file and rewrites only the frames this
/// crate owns: the standard text frames, the Suno `TXXX` descriptions, the
/// English unnamed comment and lyrics, the English `SYLT`, and the front-cover
/// `APIC`. Everything else (`POPM` ratings, ReplayGain `TXXX`, comments and
/// lyrics in other languages or with their own descriptions, and frames this
/// crate does not model at all) is carried through untouched.
///
/// A file with no readable tag falls back to fresh tagging, and a run that
/// leaves the tag semantically unchanged returns the original bytes rather than
/// rewriting them (#537).
fn retag_id3(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    synced: Option<&AlignedLyrics>,
    timing: LyricsTiming,
    preserve_existing_cover: bool,
    err_context: &str,
) -> Result<Vec<u8>> {
    let Ok(existing) = id3::Tag::read_from2(Cursor::new(audio)) else {
        return tag_id3(
            audio,
            meta,
            cover,
            synced,
            timing,
            preserve_existing_cover,
            err_context,
        );
    };
    let sylt = synced.and_then(|aligned| build_sylt(aligned, timing));
    let owned = OwnedId3::new(
        meta,
        cover.is_some(),
        sylt.is_some(),
        preserve_existing_cover,
    );

    let mut tag = id3::Tag::new();
    carry_frames(&mut tag, &existing, &owned, Carry::Unowned);
    apply_owned_id3(&mut tag, meta, cover, sylt);
    if tag == existing {
        return Ok(audio.to_vec());
    }
    write_id3(&tag, audio, err_context)
}

/// Which existing frames a tagging pass carries into the new tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Carry {
    /// Fresh tagging: only lyrics, timed lyrics, and cover art, and only where
    /// this run has nothing to replace them with.
    LyricsAndCover,
    /// Retagging: every frame this crate does not own.
    Unowned,
}

/// The frames this run rewrites or clears, so they must not be carried over.
struct OwnedId3<'m> {
    meta: &'m TrackMetadata,
    /// A new `USLT` replaces the owned one; otherwise it is kept.
    lyrics: bool,
    /// A new `SYLT` replaces the owned one; otherwise it is kept.
    sylt: bool,
    /// The owned front cover is being replaced or deliberately cleared.
    cover: bool,
}

impl<'m> OwnedId3<'m> {
    fn new(
        meta: &'m TrackMetadata,
        has_cover: bool,
        has_sylt: bool,
        preserve_existing_cover: bool,
    ) -> Self {
        Self {
            meta,
            lyrics: !meta.lyrics.is_empty(),
            sylt: has_sylt,
            cover: has_cover || !preserve_existing_cover,
        }
    }

    /// Does this run own (and therefore rewrite or drop) `frame`?
    fn claims(&self, frame: &Frame) -> bool {
        match frame.content() {
            Content::Text(_) => OWNED_TEXT_FRAMES.contains(&frame.id()),
            Content::ExtendedText(txxx) => self
                .meta
                .suno_fields()
                .iter()
                .any(|(desc, _)| *desc == txxx.description),
            Content::Comment(comment) => unnamed_english(&comment.lang, &comment.description),
            Content::Lyrics(lyrics) => {
                self.lyrics && unnamed_english(&lyrics.lang, &lyrics.description)
            }
            Content::SynchronisedLyrics(sylt) => {
                self.sylt
                    && sylt.lang == LANG
                    && sylt.content_type == SynchronisedLyricsType::Lyrics
            }
            Content::Picture(picture) => {
                self.cover && picture.picture_type == PictureType::CoverFront
            }
            _ => false,
        }
    }

    /// Does a fresh tag carry `frame` over at all? It keeps the whole set of a
    /// kind when this run has nothing to replace it with, and none of it
    /// otherwise, which is how rebuilding the tag from scratch has always
    /// behaved.
    fn fresh_carries(&self, frame: &Frame) -> bool {
        match frame.content() {
            Content::Lyrics(_) => !self.lyrics,
            Content::SynchronisedLyrics(_) => !self.sylt,
            Content::Picture(_) => !self.cover,
            _ => false,
        }
    }
}

/// A comment or lyrics frame with no description in this crate's language: the
/// shape it writes, and so the only one it may overwrite.
fn unnamed_english(lang: &str, description: &str) -> bool {
    lang == LANG && description.is_empty()
}

/// Copy the frames `existing` keeps into `tag`, honouring the carry policy.
fn carry_frames(tag: &mut id3::Tag, existing: &id3::Tag, owned: &OwnedId3<'_>, carry: Carry) {
    for frame in existing.frames() {
        let carried = match carry {
            Carry::LyricsAndCover => owned.fresh_carries(frame),
            Carry::Unowned => !owned.claims(frame),
        };
        if carried {
            tag.add_frame(frame.clone());
        }
    }
}

/// Write this crate's frames into `tag`, replacing any it already holds.
///
/// An owned field with no value is simply not written; the caller has already
/// dropped the stale frame, which is how a cleared field is removed on a retag.
fn apply_owned_id3(
    tag: &mut id3::Tag,
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    sylt: Option<SynchronisedLyrics>,
) {
    tag.set_title(meta.title.clone());
    tag.set_artist(meta.artist.clone());
    if !meta.album.is_empty() {
        tag.set_album(meta.album.clone());
    }
    if !meta.album_artist.is_empty() {
        tag.set_album_artist(meta.album_artist.clone());
    }
    if !meta.date.is_empty() {
        tag.set_text("TDRC", meta.date.as_str());
    }
    if !meta.year.is_empty() {
        tag.set_text("TDRL", meta.year.as_str());
    }
    if meta.track > 0 {
        tag.set_track(meta.track);
        if meta.track_total > 0 {
            tag.set_total_tracks(meta.track_total);
        }
    }
    if !meta.comment.is_empty() {
        tag.add_frame(Comment {
            lang: LANG.to_owned(),
            description: String::new(),
            text: meta.comment.clone(),
        });
    }
    if !meta.lyrics.is_empty() {
        tag.add_frame(Lyrics {
            lang: LANG.to_owned(),
            description: String::new(),
            text: meta.lyrics.clone(),
        });
    }
    for (desc, value) in meta.suno_fields() {
        if !value.is_empty() {
            tag.add_frame(ExtendedText {
                description: desc.to_owned(),
                value: value.to_owned(),
            });
        }
    }
    if let Some(cover) = cover {
        tag.add_frame(Picture {
            mime_type: cover.mime.to_owned(),
            picture_type: PictureType::CoverFront,
            description: String::new(),
            data: cover.bytes.to_vec(),
        });
    }
    if let Some(sylt) = sylt {
        tag.add_frame(sylt);
    }
}

/// Write `tag` into `audio`, replacing any tag already there.
fn write_id3(tag: &id3::Tag, audio: &[u8], err_context: &str) -> Result<Vec<u8>> {
    let mut cursor = Cursor::new(audio.to_vec());
    tag.write_to_file(&mut cursor, id3::Version::Id3v24)
        .map_err(|err| Error::Tag(format!("could not write {err_context}: {err}")))?;
    Ok(cursor.into_inner())
}

/// Combined `"name (version)"` model label, or just the name when no version.
fn model_label(clip: &Clip) -> String {
    match (
        non_empty(&clip.model_name),
        non_empty(&clip.major_model_version),
    ) {
        (Some(name), Some(version)) => format!("{name} ({version})"),
        _ => clip.model_name.clone(),
    }
}

/// The compact, typed lineage summary embedded as `SUNO_LINEAGE`.
///
/// Derived purely from the resolved [`LineageContext`]. Emits up to two lines:
///
/// - when the clip has a parent: `"<edge label> <parent8>"` (the edge's
///   [`EdgeType::label`], or `"Derived from"` when the edge is unknown);
/// - when the clip is not its own root: `"Root <root8> (<root title>)"`.
///
/// A pure root (no parent, its own root) yields an empty string.
fn lineage_summary(clip: &Clip, lineage: &LineageContext) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !lineage.parent_id.is_empty() {
        let label = lineage
            .edge_type
            .map(EdgeType::label)
            .unwrap_or("Derived from");
        parts.push(format!("{label} {}", first_chars(&lineage.parent_id, 8)));
    }
    if !lineage.root_id.is_empty() && lineage.root_id != clip.id {
        parts.push(format!(
            "Root {} ({})",
            first_chars(&lineage.root_id, 8),
            lineage.root_title
        ));
    }
    parts.join("\n")
}

/// The canonical Suno page URL for a clip id, or empty when the id is empty.
fn song_url(id: &str) -> String {
    if id.is_empty() {
        String::new()
    } else {
        format!("{SUNO_SONG_BASE_URL}/{id}")
    }
}

/// `Some(s)` when `s` is non-empty, else `None`.
///
/// Shared with the library index so its `artist`/`handle` fields apply the same
/// emptiness test (and `"Suno"` fallback) as the embedded tags.
pub(crate) fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// The first `n` characters of `s` (whole string when shorter).
fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests;
