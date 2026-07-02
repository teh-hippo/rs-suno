//! Track metadata and pure, byte-to-byte audio tagging.
//!
//! [`TrackMetadata`] is the tag set derived from a [`Clip`], mirroring the
//! `ha-suno` reference. [`tag_mp3`] and [`tag_flac`] take audio bytes plus
//! metadata and return tagged bytes, working entirely in memory so the engine
//! stays free of direct IO and the logic is unit-testable without a network.

use std::io::Cursor;

use id3::TagLike;
use id3::frame::{Comment, ExtendedText, Lyrics, Picture, PictureType};

use crate::error::{Error, Result};
use crate::lineage::{EdgeType, LineageContext};
use crate::model::Clip;

const COVER_MIME: &str = "image/jpeg";
const LANG: &str = "eng";

/// The metadata tags written into a downloaded audio file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub date: String,
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
    /// `YYYY-MM-DD` prefix of `created_at`. The `album`, `parent`, `root`, and
    /// `lineage` tags come from the resolved context, never the now-defunct
    /// `album_title`/`edited_clip_id`/`root_ancestor_id` feed fields. The
    /// `lyrics` tag carries the clip's real lyrics, and the generation `prompt`
    /// is preserved in its own `SUNO_PROMPT` tag.
    pub fn from_clip(clip: &Clip, lineage: &LineageContext) -> TrackMetadata {
        let artist = non_empty(&clip.display_name).unwrap_or("Suno").to_owned();
        let album = lineage.album(&clip.title);
        TrackMetadata {
            title: clip.title.clone(),
            artist: artist.clone(),
            album,
            album_artist: artist,
            date: first_chars(&clip.created_at, 10),
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
    fn suno_fields(&self) -> [(&'static str, &str); 8] {
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
/// as a front-cover `APIC` frame when provided.
pub fn tag_mp3(audio: &[u8], meta: &TrackMetadata, cover: Option<&[u8]>) -> Result<Vec<u8>> {
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
    if let Some(bytes) = cover {
        tag.add_frame(Picture {
            mime_type: COVER_MIME.to_owned(),
            picture_type: PictureType::CoverFront,
            description: String::new(),
            data: bytes.to_vec(),
        });
    }

    let mut cursor = Cursor::new(audio.to_vec());
    tag.write_to_file(&mut cursor, id3::Version::Id3v24)
        .map_err(|err| Error::Tag(format!("could not write ID3 tag: {err}")))?;
    Ok(cursor.into_inner())
}

/// Tag `audio` (a FLAC byte stream) with `meta`, returning the tagged bytes.
///
/// Replaces the Vorbis comments, embeds `cover` as a front-cover `PICTURE`
/// block, and preserves the original `STREAMINFO` and audio frames. Works in
/// memory: the existing metadata blocks are rewritten and the audio frames are
/// appended unchanged.
pub fn tag_flac(audio: &[u8], meta: &TrackMetadata, cover: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut tag = metaflac::Tag::read_from(&mut Cursor::new(audio))
        .map_err(|err| Error::Tag(format!("could not read FLAC metadata: {err}")))?;

    tag.remove_blocks(metaflac::BlockType::VorbisComment);
    for (key, value) in flac_fields(meta) {
        if !value.is_empty() {
            tag.set_vorbis(key, vec![value.to_owned()]);
        }
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

/// The Vorbis comment fields, in `(KEY, value)` order.
fn flac_fields(meta: &TrackMetadata) -> [(&'static str, &str); 15] {
    [
        ("TITLE", &meta.title),
        ("ARTIST", &meta.artist),
        ("ALBUM", &meta.album),
        ("ALBUMARTIST", &meta.album_artist),
        ("DATE", &meta.date),
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
fn non_empty(s: &str) -> Option<&str> {
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
            album_title: "Weather Series".to_owned(),
            edited_clip_id: "parentid1234".to_owned(),
            root_ancestor_id: "rootid567890".to_owned(),
            lineage_status: "continuation".to_owned(),
            ..Clip::default()
        }
    }

    /// A resolved context for [`full_clip`]: an extension whose root carries the
    /// "Weather Series" album title.
    fn full_lineage() -> LineageContext {
        LineageContext {
            root_id: "rootid567890".to_owned(),
            root_title: "Weather Series".to_owned(),
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
        let tagged = tag_mp3(b"", &meta, Some(&cover)).unwrap();

        let tag = id3::Tag::read_from2(Cursor::new(tagged)).unwrap();
        assert_eq!(tag.title(), Some("Electric Storm"));
        assert_eq!(tag.artist(), Some("alice"));
        assert_eq!(tag.album(), Some("Weather Series"));
        assert_eq!(tag.album_artist(), Some("alice"));

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

        let tagged = tag_mp3(b"", &meta, None).unwrap();
        let tag = id3::Tag::read_from2(Cursor::new(tagged)).unwrap();
        let uslt = tag.lyrics().next().map(|frame| frame.text.clone());
        assert_eq!(uslt.as_deref(), Some("the sung words"));
        let prompt = tag
            .extended_texts()
            .find(|frame| frame.description == "SUNO_PROMPT")
            .map(|frame| frame.value.clone());
        assert_eq!(prompt.as_deref(), Some("the generation prompt"));
    }

    #[test]
    fn mp3_tagging_replaces_an_existing_tag() {
        let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
        let once = tag_mp3(b"audioframes", &meta, None).unwrap();
        let twice = tag_mp3(&once, &meta, None).unwrap();

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
}
