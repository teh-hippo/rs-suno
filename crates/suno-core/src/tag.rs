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
    Comment, ExtendedText, Lyrics, Picture, PictureType, SynchronisedLyrics,
    SynchronisedLyricsType, TimestampFormat,
};

use crate::error::{Error, Result};
use crate::lineage::{EdgeType, LineageContext};
use crate::lyrics::AlignedLyrics;
use crate::model::Clip;

const COVER_MIME: &str = "image/jpeg";
const LANG: &str = "eng";

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
    /// context, not from the clip's own `edited_clip_id` pointer or the removed
    /// `album_title`/`root_ancestor_id` feed fields. The `lyrics` tag carries the
    /// clip's real
    /// lyrics, and the generation `prompt` is preserved in its own `SUNO_PROMPT`
    /// tag.
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
        }
    }

    /// The Suno-specific fields, paired with their tag description/key.
    pub(crate) fn suno_fields(&self) -> [(&'static str, &str); 8] {
        [
            ("SUNO_PROMPT", &self.prompt),
            ("SUNO_STYLE", &self.style),
            ("SUNO_STYLE_SUMMARY", &self.style_summary),
            ("SUNO_MODEL", &self.model),
            ("SUNO_HANDLE", &self.handle),
            ("SUNO_PARENT", &self.parent),
            ("SUNO_ROOT", &self.root),
            ("SUNO_LINEAGE", &self.lineage),
        ]
    }
}

/// Tag `audio` (an MP3 byte stream) with `meta`, returning the tagged bytes.
///
/// Writes ID3v2.4 frames, replacing any existing ID3 tag, and embeds `cover`
/// as a front-cover `APIC` frame when provided. When `synced` carries aligned
/// lyrics, a word-level `SYLT` (synchronised lyrics) frame is added alongside
/// the plain `USLT` lyrics, so a player can render karaoke-style timed lyrics;
/// an empty (instrumental) `synced` adds no `SYLT`.
///
/// Because the whole tag is rebuilt, any existing `SYLT`/`USLT` lyrics would be
/// lost on a plain retag that carries no new lyrics. To avoid downgrading a good
/// timed file, existing `SYLT` frames are preserved when `synced` is `None`, and
/// existing `USLT` frames are preserved when `meta` carries no lyrics text.
pub fn tag_mp3(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<&[u8]>,
    synced: Option<&AlignedLyrics>,
) -> Result<Vec<u8>> {
    let existing = id3::Tag::read_from2(Cursor::new(audio)).ok();
    let mut tag = id3::Tag::new();
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
    } else if let Some(existing) = &existing {
        // No new lyrics this run (a plain retag): keep any embedded USLT.
        for lyrics in existing.lyrics() {
            tag.add_frame(lyrics.clone());
        }
    }
    for (desc, value) in meta.suno_fields() {
        if !value.is_empty() {
            tag.add_frame(ExtendedText {
                description: desc.to_owned(),
                value: value.to_owned(),
            });
        }
    }
    if let Some(bytes) = cover {
        tag.add_frame(Picture {
            mime_type: COVER_MIME.to_owned(),
            picture_type: PictureType::CoverFront,
            description: String::new(),
            data: bytes.to_vec(),
        });
    }
    match synced.and_then(build_sylt) {
        Some(sylt) => {
            tag.add_frame(sylt);
        }
        // No new alignment this run: keep any embedded SYLT so a retag never
        // downgrades a timed file.
        None => {
            if let Some(existing) = &existing {
                for sylt in existing.synchronised_lyrics() {
                    tag.add_frame(sylt.clone());
                }
            }
        }
    }

    let mut cursor = Cursor::new(audio.to_vec());
    tag.write_to_file(&mut cursor, id3::Version::Id3v24)
        .map_err(|err| Error::Tag(format!("could not write ID3 tag: {err}")))?;
    Ok(cursor.into_inner())
}

/// Build a word-level `SYLT` frame from `aligned`, or `None` when there is
/// nothing to time (an instrumental with empty arrays).
///
/// Timestamps are absolute milliseconds ([`TimestampFormat::Ms`]); the content
/// is the word-level segments from [`AlignedLyrics::sylt_entries`], with a
/// leading newline on each line's first word so a player renders line breaks.
fn build_sylt(aligned: &AlignedLyrics) -> Option<SynchronisedLyrics> {
    let content = aligned.sylt_entries();
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
/// Replaces the Vorbis comments, embeds `cover` as a front-cover `PICTURE`
/// block, and preserves the original `STREAMINFO` and audio frames. Works in
/// memory: the existing metadata blocks are rewritten and the audio frames are
/// appended unchanged.
///
/// When `meta` carries no lyrics text (a plain retag), any existing `LYRICS`
/// comment is preserved rather than dropped, so a retag never loses embedded
/// lyrics.
pub fn tag_flac(audio: &[u8], meta: &TrackMetadata, cover: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut tag = metaflac::Tag::read_from(&mut Cursor::new(audio))
        .map_err(|err| Error::Tag(format!("could not read FLAC metadata: {err}")))?;

    let existing_lyrics: Vec<String> = tag
        .get_vorbis("LYRICS")
        .map(|values| values.map(str::to_owned).collect())
        .unwrap_or_default();

    tag.remove_blocks(metaflac::BlockType::VorbisComment);
    for (key, value) in flac_fields(meta) {
        if !value.is_empty() {
            tag.set_vorbis(key, vec![value.to_owned()]);
        }
    }
    if meta.lyrics.is_empty() && !existing_lyrics.is_empty() {
        tag.set_vorbis("LYRICS", existing_lyrics);
    }
    if let Some(bytes) = cover {
        tag.add_picture(
            COVER_MIME,
            metaflac::block::PictureType::CoverFront,
            bytes.to_vec(),
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
/// When `synced` carries aligned lyrics, a word-level `SYLT` frame is added
/// alongside the plain `USLT` frame.
///
/// Existing `SYLT` frames are preserved when `synced` is `None`, and existing
/// `USLT` frames are preserved when `meta` carries no lyrics text, so a retag
/// never loses previously embedded lyrics.
pub fn tag_wav(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<&[u8]>,
    synced: Option<&AlignedLyrics>,
) -> Result<Vec<u8>> {
    let existing = id3::Tag::read_from2(Cursor::new(audio)).ok();
    let mut tag = id3::Tag::new();
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
    } else if let Some(existing) = &existing {
        for lyrics in existing.lyrics() {
            tag.add_frame(lyrics.clone());
        }
    }
    for (desc, value) in meta.suno_fields() {
        if !value.is_empty() {
            tag.add_frame(ExtendedText {
                description: desc.to_owned(),
                value: value.to_owned(),
            });
        }
    }
    if let Some(bytes) = cover {
        tag.add_frame(Picture {
            mime_type: COVER_MIME.to_owned(),
            picture_type: PictureType::CoverFront,
            description: String::new(),
            data: bytes.to_vec(),
        });
    }
    match synced.and_then(build_sylt) {
        Some(sylt) => {
            tag.add_frame(sylt);
        }
        None => {
            if let Some(existing) = &existing {
                for sylt in existing.synchronised_lyrics() {
                    tag.add_frame(sylt.clone());
                }
            }
        }
    }

    let mut cursor = Cursor::new(audio.to_vec());
    tag.write_to_file(&mut cursor, id3::Version::Id3v24)
        .map_err(|err| Error::Tag(format!("could not write WAV ID3 tag: {err}")))?;
    Ok(cursor.into_inner())
}

/// The Vorbis comment fields, in `(KEY, value)` order.
fn flac_fields(meta: &TrackMetadata) -> [(&'static str, &str); 16] {
    [
        ("TITLE", &meta.title),
        ("ARTIST", &meta.artist),
        ("ALBUM", &meta.album),
        ("ALBUMARTIST", &meta.album_artist),
        ("DATE", &meta.date),
        ("YEAR", &meta.year),
        ("LYRICS", &meta.lyrics),
        ("DESCRIPTION", &meta.comment),
        ("SUNO_PROMPT", &meta.prompt),
        ("SUNO_STYLE", &meta.style),
        ("SUNO_STYLE_SUMMARY", &meta.style_summary),
        ("SUNO_MODEL", &meta.model),
        ("SUNO_HANDLE", &meta.handle),
        ("SUNO_PARENT", &meta.parent),
        ("SUNO_ROOT", &meta.root),
        ("SUNO_LINEAGE", &meta.lineage),
    ]
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
/// Derived purely from the resolved [`LineageContext`], never the defunct feed
/// fields. Emits up to two lines:
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
mod tests {
    use super::*;
    use crate::lineage::ResolveStatus;

    fn full_clip() -> Clip {
        Clip {
            id: "clip-1234abcd".to_owned(),
            title: "Electric Storm".to_owned(),
            tags: "ambient, cinematic".to_owned(),
            created_at: "2024-03-10T14:22:01Z".to_owned(),
            display_name: "alice".to_owned(),
            handle: "alice".to_owned(),
            prompt: "an orchestral storm".to_owned(),
            gpt_description_prompt: "a moody cinematic build".to_owned(),
            lyrics: "thunder rolls\nover the plains".to_owned(),
            model_name: "chirp-v4".to_owned(),
            major_model_version: "v4".to_owned(),
            edited_clip_id: "parentid1234".to_owned(),
            ..Clip::default()
        }
    }

    /// A resolved context for [`full_clip`]: an extension whose root carries the
    /// "Weather Series" album title and a root date one year before the clip's
    /// own, so the Year tag can be seen to follow the root, not the clip.
    fn full_lineage() -> LineageContext {
        LineageContext {
            root_id: "rootid567890".to_owned(),
            root_title: "Weather Series".to_owned(),
            root_date: "2023-11-02T09:00:00Z".to_owned(),
            parent_id: "parentid1234".to_owned(),
            edge_type: Some(EdgeType::Extend),
            status: ResolveStatus::Resolved,
        }
    }

    #[test]
    fn maps_full_clip() {
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        assert_eq!(meta.title, "Electric Storm");
        assert_eq!(meta.artist, "alice");
        assert_eq!(meta.album, "Weather Series");
        assert_eq!(meta.album_artist, "alice");
        assert_eq!(meta.date, "2024-03-10");
        // The Year follows the lineage root (2023), not the clip's own 2024.
        assert_eq!(meta.year, "2023");
        assert_eq!(meta.lyrics, "thunder rolls\nover the plains");
        assert_eq!(meta.prompt, "an orchestral storm");
        assert_eq!(meta.comment, "a moody cinematic build");
        assert_eq!(meta.style, "ambient, cinematic");
        assert_eq!(meta.style_summary, "a moody cinematic build");
        assert_eq!(meta.model, "chirp-v4 (v4)");
        assert_eq!(meta.handle, "alice");
        assert_eq!(meta.parent, "parentid1234");
        assert_eq!(meta.root, "rootid567890");
    }

    #[test]
    fn falls_back_when_fields_are_empty() {
        let clip = Clip {
            title: "Just A Title".to_owned(),
            ..Clip::default()
        };
        let meta = TrackMetadata::from_clip(&clip, &LineageContext::own_root(&clip));
        assert_eq!(meta.artist, "Suno");
        assert_eq!(meta.album_artist, "Suno");
        assert_eq!(meta.album, "Just A Title");
        assert_eq!(meta.date, "");
        assert_eq!(meta.year, "");
        assert_eq!(meta.model, "");
        assert_eq!(meta.lineage, "");
    }

    #[test]
    fn album_uses_root_title() {
        let clip = Clip {
            id: "child-01".to_owned(),
            title: "Track".to_owned(),
            ..Clip::default()
        };
        let lineage = LineageContext {
            root_id: "root-01".to_owned(),
            root_title: "The Album".to_owned(),
            root_date: String::new(),
            parent_id: "root-01".to_owned(),
            edge_type: Some(EdgeType::Cover),
            status: ResolveStatus::Resolved,
        };
        let meta = TrackMetadata::from_clip(&clip, &lineage);
        assert_eq!(meta.album, "The Album");
    }

    #[test]
    fn model_label_uses_name_only_without_version() {
        let clip = Clip {
            model_name: "chirp-v3".to_owned(),
            ..Clip::default()
        };
        let meta = TrackMetadata::from_clip(&clip, &LineageContext::own_root(&clip));
        assert_eq!(meta.model, "chirp-v3");
    }

    #[test]
    fn model_label_is_empty_without_name() {
        let clip = Clip {
            major_model_version: "v4".to_owned(),
            ..Clip::default()
        };
        let meta = TrackMetadata::from_clip(&clip, &LineageContext::own_root(&clip));
        assert_eq!(meta.model, "");
    }

    #[test]
    fn date_is_truncated_to_ten_characters() {
        let clip = Clip {
            created_at: "2024-12-31T23:59:59Z".to_owned(),
            ..Clip::default()
        };
        let meta = TrackMetadata::from_clip(&clip, &LineageContext::own_root(&clip));
        assert_eq!(meta.date, "2024-12-31");
    }

    #[test]
    fn lineage_reports_derivation_and_root() {
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        assert_eq!(
            meta.lineage,
            "Extended from parentid\nRoot rootid56 (Weather Series)"
        );
    }

    #[test]
    fn lineage_defaults_to_derived_from_when_edge_unknown() {
        let clip = Clip {
            id: "self-0001".to_owned(),
            ..Clip::default()
        };
        let lineage = LineageContext {
            root_id: "root-7777".to_owned(),
            root_title: "Origin".to_owned(),
            root_date: String::new(),
            parent_id: "parent-9999".to_owned(),
            edge_type: None,
            status: ResolveStatus::Resolved,
        };
        let meta = TrackMetadata::from_clip(&clip, &lineage);
        assert_eq!(
            meta.lineage,
            "Derived from parent-9\nRoot root-777 (Origin)"
        );
    }

    #[test]
    fn lineage_is_empty_for_a_pure_root() {
        let clip = Clip {
            id: "same-id-01".to_owned(),
            ..Clip::default()
        };
        let meta = TrackMetadata::from_clip(&clip, &LineageContext::own_root(&clip));
        assert_eq!(meta.lineage, "");
        assert_eq!(meta.parent, "");
    }

    #[test]
    fn mp3_round_trips_core_tags() {
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        let cover = b"\xFF\xD8\xFFcover-bytes".to_vec();
        let tagged = tag_mp3(b"", &meta, Some(&cover), None).unwrap();

        let tag = id3::Tag::read_from2(Cursor::new(tagged)).unwrap();
        assert_eq!(tag.title(), Some("Electric Storm"));
        assert_eq!(tag.artist(), Some("alice"));
        assert_eq!(tag.album(), Some("Weather Series"));
        assert_eq!(tag.album_artist(), Some("alice"));

        // TDRC keeps the accurate per-track recording date; TDRL surfaces the
        // lineage root's year so a player can show a distinct Year.
        let text = |id: &str| tag.get(id).and_then(|frame| frame.content().text());
        assert_eq!(text("TDRC"), Some("2024-03-10"));
        assert_eq!(text("TDRL"), Some("2023"));

        let extended = |desc: &str| {
            tag.extended_texts()
                .find(|frame| frame.description == desc)
                .map(|frame| frame.value.clone())
        };
        assert_eq!(
            extended("SUNO_STYLE").as_deref(),
            Some("ambient, cinematic")
        );
        assert_eq!(extended("SUNO_MODEL").as_deref(), Some("chirp-v4 (v4)"));
        assert_eq!(
            extended("SUNO_PROMPT").as_deref(),
            Some("an orchestral storm")
        );
        assert_eq!(extended("SUNO_PARENT").as_deref(), Some("parentid1234"));
        assert_eq!(extended("SUNO_ROOT").as_deref(), Some("rootid567890"));
        assert_eq!(
            extended("SUNO_LINEAGE").as_deref(),
            Some("Extended from parentid\nRoot rootid56 (Weather Series)")
        );

        let lyrics = tag.lyrics().next().map(|frame| frame.text.as_str());
        assert_eq!(lyrics, Some("thunder rolls\nover the plains"));

        let picture = tag.pictures().next().unwrap();
        assert_eq!(picture.picture_type, PictureType::CoverFront);
        assert_eq!(picture.mime_type, COVER_MIME);
        assert_eq!(picture.data, cover);
    }

    #[test]
    fn lyrics_and_prompt_are_distinct_and_not_swapped() {
        let clip = Clip {
            prompt: "the generation prompt".to_owned(),
            lyrics: "the sung words".to_owned(),
            ..Clip::default()
        };
        let meta = TrackMetadata::from_clip(&clip, &LineageContext::own_root(&clip));
        assert_eq!(meta.lyrics, "the sung words");
        assert_eq!(meta.prompt, "the generation prompt");

        let tagged = tag_mp3(b"", &meta, None, None).unwrap();
        let tag = id3::Tag::read_from2(Cursor::new(tagged)).unwrap();
        let uslt = tag.lyrics().next().map(|frame| frame.text.clone());
        assert_eq!(uslt.as_deref(), Some("the sung words"));
        let prompt = tag
            .extended_texts()
            .find(|frame| frame.description == "SUNO_PROMPT")
            .map(|frame| frame.value.clone());
        assert_eq!(prompt.as_deref(), Some("the generation prompt"));
    }

    fn sample_aligned() -> AlignedLyrics {
        AlignedLyrics::from_json(&serde_json::json!({
            "aligned_words": [],
            "aligned_lyrics": [
                {"text": "Hello world", "start_s": 0.5, "end_s": 1.4, "section": "Verse 1",
                 "words": [
                     {"text": "Hello", "start_s": 0.5, "end_s": 0.9},
                     {"text": "world", "start_s": 1.0, "end_s": 1.4}
                 ]},
                {"text": "again", "start_s": 61.2, "end_s": 61.8, "section": "Chorus",
                 "words": [{"text": "again", "start_s": 61.2, "end_s": 61.8}]}
            ]
        }))
    }

    #[test]
    fn build_sylt_produces_ms_word_entries() {
        let sylt = build_sylt(&sample_aligned()).unwrap();
        assert_eq!(sylt.timestamp_format, TimestampFormat::Ms);
        assert_eq!(sylt.content_type, SynchronisedLyricsType::Lyrics);
        assert_eq!(sylt.lang, "eng");
        assert_eq!(
            sylt.content,
            vec![
                (500, "Hello".to_owned()),
                (1000, " world".to_owned()),
                (61200, "\nagain".to_owned()),
            ]
        );
    }

    #[test]
    fn build_sylt_is_none_for_empty_alignment() {
        assert!(build_sylt(&AlignedLyrics::default()).is_none());
    }

    #[test]
    fn mp3_embeds_sylt_when_synced_present() {
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        let aligned = sample_aligned();
        let tagged = tag_mp3(b"frames", &meta, None, Some(&aligned)).unwrap();
        let tag = id3::Tag::read_from2(Cursor::new(&tagged)).unwrap();
        let sylt = tag
            .synchronised_lyrics()
            .next()
            .expect("a SYLT frame is present");
        assert_eq!(sylt.timestamp_format, TimestampFormat::Ms);
        assert_eq!(sylt.content.first(), Some(&(500, "Hello".to_owned())));
        assert!(tagged.ends_with(b"frames"));
    }

    #[test]
    fn mp3_omits_sylt_for_instrumental() {
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        let tagged = tag_mp3(b"frames", &meta, None, Some(&AlignedLyrics::default())).unwrap();
        let tag = id3::Tag::read_from2(Cursor::new(&tagged)).unwrap();
        assert_eq!(tag.synchronised_lyrics().count(), 0);
    }

    #[test]
    fn mp3_retag_preserves_existing_sylt_and_uslt_without_new_lyrics() {
        // First write embeds SYLT + USLT from alignment.
        let aligned = sample_aligned();
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        let mut with_lyrics = meta.clone();
        with_lyrics.lyrics = aligned.plain_text();
        let first = tag_mp3(b"frames", &with_lyrics, None, Some(&aligned)).unwrap();

        // A later retag carries NO new lyrics (empty lyrics, no synced): the
        // existing SYLT and USLT must be preserved, not dropped.
        let mut retag_meta = meta.clone();
        retag_meta.lyrics = String::new();
        let retagged = tag_mp3(&first, &retag_meta, None, None).unwrap();
        let tag = id3::Tag::read_from2(Cursor::new(&retagged)).unwrap();
        assert_eq!(tag.synchronised_lyrics().count(), 1, "SYLT preserved");
        assert_eq!(
            tag.lyrics().next().map(|frame| frame.text.clone()),
            Some(aligned.plain_text()),
            "USLT preserved"
        );
    }

    #[test]
    fn mp3_retag_replaces_sylt_when_new_alignment_given() {
        let aligned = sample_aligned();
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        let first = tag_mp3(b"frames", &meta, None, Some(&aligned)).unwrap();
        // A fresh alignment on retag replaces (not stacks) the SYLT frame.
        let again = tag_mp3(&first, &meta, None, Some(&aligned)).unwrap();
        let tag = id3::Tag::read_from2(Cursor::new(&again)).unwrap();
        assert_eq!(tag.synchronised_lyrics().count(), 1);
    }

    #[test]
    fn flac_retag_preserves_existing_lyrics_comment() {
        let audio = minimal_flac();
        let mut meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        meta.lyrics = "line one\nline two".to_owned();
        let first = tag_flac(&audio, &meta, None).unwrap();

        // A retag with no lyrics text keeps the existing LYRICS comment.
        let mut retag_meta = meta.clone();
        retag_meta.lyrics = String::new();
        let retagged = tag_flac(&first, &retag_meta, None).unwrap();
        let tag = metaflac::Tag::read_from(&mut Cursor::new(&retagged)).unwrap();
        assert_eq!(
            tag.get_vorbis("LYRICS").map(|v| v.collect::<Vec<_>>()),
            Some(vec!["line one\nline two"])
        );
    }

    #[test]
    fn mp3_tagging_replaces_an_existing_tag() {
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        let once = tag_mp3(b"audioframes", &meta, None, None).unwrap();
        let twice = tag_mp3(&once, &meta, None, None).unwrap();

        let tag = id3::Tag::read_from2(Cursor::new(&twice)).unwrap();
        assert_eq!(tag.title(), Some("Electric Storm"));
        // Exactly one title frame; the prior tag was replaced, not stacked.
        let title_frames = tag.frames().filter(|frame| frame.id() == "TIT2").count();
        assert_eq!(title_frames, 1);
        assert!(twice.ends_with(b"audioframes"));
    }

    #[test]
    fn flac_round_trips_core_tags_and_preserves_audio() {
        let audio = minimal_flac();
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        let cover = b"\xFF\xD8\xFFflac-cover".to_vec();
        let tagged = tag_flac(&audio, &meta, Some(&cover)).unwrap();

        let tag = metaflac::Tag::read_from(&mut Cursor::new(&tagged)).unwrap();
        let vorbis = tag.vorbis_comments().unwrap();
        assert_eq!(vorbis.get("TITLE").unwrap(), &["Electric Storm"]);
        assert_eq!(vorbis.get("ARTIST").unwrap(), &["alice"]);
        assert_eq!(vorbis.get("ALBUM").unwrap(), &["Weather Series"]);
        assert_eq!(vorbis.get("ALBUMARTIST").unwrap(), &["alice"]);
        // DATE is the per-track date; YEAR carries the lineage root's year.
        assert_eq!(vorbis.get("DATE").unwrap(), &["2024-03-10"]);
        assert_eq!(vorbis.get("YEAR").unwrap(), &["2023"]);
        assert_eq!(vorbis.get("SUNO_MODEL").unwrap(), &["chirp-v4 (v4)"]);
        assert_eq!(vorbis.get("SUNO_PROMPT").unwrap(), &["an orchestral storm"]);
        assert_eq!(
            vorbis.get("LYRICS").unwrap(),
            &["thunder rolls\nover the plains"]
        );
        assert_eq!(vorbis.get("SUNO_PARENT").unwrap(), &["parentid1234"]);
        assert_eq!(vorbis.get("SUNO_ROOT").unwrap(), &["rootid567890"]);
        assert_eq!(
            vorbis.get("SUNO_LINEAGE").unwrap(),
            &["Extended from parentid\nRoot rootid56 (Weather Series)"]
        );
        assert_eq!(
            vorbis.get("DESCRIPTION").unwrap(),
            &["a moody cinematic build"]
        );

        let picture = tag.pictures().next().unwrap();
        assert_eq!(
            picture.picture_type,
            metaflac::block::PictureType::CoverFront
        );
        assert_eq!(picture.data, cover);

        // STREAMINFO is preserved (same sample rate and total samples).
        let info = tag.get_streaminfo().unwrap();
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.total_samples, 44_100);

        // The audio frames after the metadata survive untouched.
        let frames = metaflac::Tag::skip_metadata(&mut Cursor::new(&tagged));
        assert_eq!(frames, FLAC_AUDIO_FRAMES);
    }

    const FLAC_AUDIO_FRAMES: &[u8] = b"\xFF\xF8audio-frame-payload";

    /// Build a minimal but structurally valid FLAC: signature, a STREAMINFO
    /// block, then stand-in audio frames. Enough for metaflac to parse, tag,
    /// and round-trip without invoking an encoder.
    fn minimal_flac() -> Vec<u8> {
        let mut streaminfo = vec![0u8; 34];
        // min/max block size = 4096.
        streaminfo[0..2].copy_from_slice(&4096u16.to_be_bytes());
        streaminfo[2..4].copy_from_slice(&4096u16.to_be_bytes());
        // Pack sample_rate (20 bits), channels-1 (3 bits), bps-1 (5 bits),
        // total_samples (36 bits) across bytes 10..18.
        let sample_rate: u64 = 44_100;
        let channels: u64 = 2;
        let bits_per_sample: u64 = 16;
        let total_samples: u64 = 44_100;
        let packed: u64 = (sample_rate << 44)
            | ((channels - 1) << 41)
            | ((bits_per_sample - 1) << 36)
            | total_samples;
        streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());

        let mut out = Vec::new();
        out.extend_from_slice(b"fLaC");
        // STREAMINFO header: last-block flag set, type 0, length 34.
        out.push(0x80);
        out.extend_from_slice(&[0x00, 0x00, 0x22]);
        out.extend_from_slice(&streaminfo);
        out.extend_from_slice(FLAC_AUDIO_FRAMES);
        out
    }

    // A short stand-in audio payload for the WAV `data` chunk.
    const WAV_AUDIO_DATA: &[u8] = b"\x00\x01\x02wav-sample-payload";

    /// Minimal RIFF/WAVE container with a `fmt ` (PCM) chunk and a `data` chunk.
    fn minimal_wav() -> Vec<u8> {
        let audio_len = WAV_AUDIO_DATA.len() as u32;
        // RIFF size = "WAVE" (4) + fmt chunk header (8) + fmt data (16)
        //           + data chunk header (8) + audio data.
        let riff_size = 4u32 + 8 + 16 + 8 + audio_len;

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        // fmt chunk (PCM, 44100 Hz, mono, 16-bit).
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&44_100u32.to_le_bytes());
        out.extend_from_slice(&88_200u32.to_le_bytes()); // byte rate
        out.extend_from_slice(&2u16.to_le_bytes()); // block align
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        // data chunk.
        out.extend_from_slice(b"data");
        out.extend_from_slice(&audio_len.to_le_bytes());
        out.extend_from_slice(WAV_AUDIO_DATA);
        out
    }

    #[test]
    fn wav_round_trips_core_tags_and_cover() {
        let audio = minimal_wav();
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        let cover = b"\xFF\xD8\xFFwav-cover".to_vec();
        let tagged = tag_wav(&audio, &meta, Some(&cover), None).unwrap();

        let tag = id3::Tag::read_from2(Cursor::new(&tagged)).unwrap();
        assert_eq!(tag.title(), Some("Electric Storm"));
        assert_eq!(tag.artist(), Some("alice"));
        assert_eq!(tag.album(), Some("Weather Series"));
        assert_eq!(tag.album_artist(), Some("alice"));

        let text = |id: &str| tag.get(id).and_then(|f| f.content().text());
        assert_eq!(text("TDRC"), Some("2024-03-10"));
        assert_eq!(text("TDRL"), Some("2023"));

        let extended = |desc: &str| {
            tag.extended_texts()
                .find(|f| f.description == desc)
                .map(|f| f.value.clone())
        };
        assert_eq!(
            extended("SUNO_STYLE").as_deref(),
            Some("ambient, cinematic")
        );
        assert_eq!(extended("SUNO_MODEL").as_deref(), Some("chirp-v4 (v4)"));
        assert_eq!(
            extended("SUNO_PROMPT").as_deref(),
            Some("an orchestral storm")
        );
        assert_eq!(extended("SUNO_PARENT").as_deref(), Some("parentid1234"));
        assert_eq!(extended("SUNO_ROOT").as_deref(), Some("rootid567890"));

        let lyrics = tag.lyrics().next().map(|f| f.text.as_str());
        assert_eq!(lyrics, Some("thunder rolls\nover the plains"));

        let picture = tag.pictures().next().unwrap();
        assert_eq!(picture.picture_type, PictureType::CoverFront);
        assert_eq!(picture.mime_type, COVER_MIME);
        assert_eq!(picture.data, cover);
    }

    #[test]
    fn wav_retag_replaces_rather_than_stacks() {
        let audio = minimal_wav();
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        let once = tag_wav(&audio, &meta, None, None).unwrap();
        let twice = tag_wav(&once, &meta, None, None).unwrap();

        let tag = id3::Tag::read_from2(Cursor::new(&twice)).unwrap();
        assert_eq!(tag.title(), Some("Electric Storm"));
        let title_count = tag.frames().filter(|f| f.id() == "TIT2").count();
        assert_eq!(title_count, 1, "prior tag replaced, not stacked");
    }

    #[test]
    fn wav_retag_preserves_existing_uslt_without_new_lyrics() {
        let audio = minimal_wav();
        let mut meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        meta.lyrics = "first embedded lyrics".to_owned();
        let with_lyrics = tag_wav(&audio, &meta, None, None).unwrap();

        let mut retag_meta = meta.clone();
        retag_meta.lyrics = String::new();
        let retagged = tag_wav(&with_lyrics, &retag_meta, None, None).unwrap();
        let tag = id3::Tag::read_from2(Cursor::new(&retagged)).unwrap();
        assert_eq!(
            tag.lyrics().next().map(|f| f.text.as_str()),
            Some("first embedded lyrics"),
            "USLT preserved on retag with no new lyrics"
        );
    }

    #[test]
    fn wav_audio_samples_preserved_after_tagging() {
        let audio = minimal_wav();
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        let tagged = tag_wav(&audio, &meta, None, None).unwrap();

        // The WAV_AUDIO_DATA bytes must survive byte-for-byte inside the tagged file.
        let found = tagged
            .windows(WAV_AUDIO_DATA.len())
            .any(|w| w == WAV_AUDIO_DATA);
        assert!(found, "audio sample bytes not found in tagged WAV");
    }
}
