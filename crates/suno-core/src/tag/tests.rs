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
        track: 0,
        track_total: 0,
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
        track: 0,
        track_total: 0,
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
        track: 0,
        track_total: 0,
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
    let tagged = tag_mp3(b"", &meta, Some(Cover::jpeg(&cover)), None).unwrap();

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
    assert_eq!(picture.mime_type, "image/jpeg");
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
    let tagged = tag_flac(&audio, &meta, Some(Cover::jpeg(&cover))).unwrap();

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

#[test]
fn from_clip_carries_id_url_and_track() {
    let lineage = LineageContext {
        track: 3,
        track_total: 10,
        ..full_lineage()
    };
    let meta = TrackMetadata::from_clip(&full_clip(), &lineage);
    assert_eq!(meta.id, "clip-1234abcd");
    assert_eq!(meta.url, "https://suno.com/song/clip-1234abcd");
    assert_eq!(meta.track, 3);
    assert_eq!(meta.track_total, 10);
}

#[test]
fn from_clip_leaves_url_empty_without_an_id() {
    let clip = Clip {
        title: "No Id".to_owned(),
        ..Clip::default()
    };
    let meta = TrackMetadata::from_clip(&clip, &LineageContext::own_root(&clip));
    assert_eq!(meta.id, "");
    assert_eq!(meta.url, "");
}

#[test]
fn flac_writes_track_number_total_and_identity() {
    let audio = minimal_flac();
    let lineage = LineageContext {
        track: 3,
        track_total: 10,
        ..full_lineage()
    };
    let meta = TrackMetadata::from_clip(&full_clip(), &lineage);
    let tagged = tag_flac(&audio, &meta, None).unwrap();

    let tag = metaflac::Tag::read_from(&mut Cursor::new(&tagged)).unwrap();
    let vorbis = tag.vorbis_comments().unwrap();
    assert_eq!(vorbis.get("TRACKNUMBER").unwrap(), &["3"]);
    assert_eq!(vorbis.get("TRACKTOTAL").unwrap(), &["10"]);
    assert_eq!(vorbis.get("SUNO_ID").unwrap(), &["clip-1234abcd"]);
    assert_eq!(
        vorbis.get("SUNO_URL").unwrap(),
        &["https://suno.com/song/clip-1234abcd"]
    );
}

#[test]
fn flac_omits_track_when_unnumbered() {
    let audio = minimal_flac();
    // full_lineage() has track 0 (unnumbered).
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let tagged = tag_flac(&audio, &meta, None).unwrap();

    let tag = metaflac::Tag::read_from(&mut Cursor::new(&tagged)).unwrap();
    let vorbis = tag.vorbis_comments().unwrap();
    assert!(vorbis.get("TRACKNUMBER").is_none());
    assert!(vorbis.get("TRACKTOTAL").is_none());
}

#[test]
fn mp3_writes_track_number_total_and_identity() {
    let lineage = LineageContext {
        track: 3,
        track_total: 10,
        ..full_lineage()
    };
    let meta = TrackMetadata::from_clip(&full_clip(), &lineage);
    let tagged = tag_mp3(b"", &meta, None, None).unwrap();

    let tag = id3::Tag::read_from2(Cursor::new(tagged)).unwrap();
    assert_eq!(tag.track(), Some(3));
    assert_eq!(tag.total_tracks(), Some(10));
    let extended = |desc: &str| {
        tag.extended_texts()
            .find(|frame| frame.description == desc)
            .map(|frame| frame.value.clone())
    };
    assert_eq!(extended("SUNO_ID").as_deref(), Some("clip-1234abcd"));
    assert_eq!(
        extended("SUNO_URL").as_deref(),
        Some("https://suno.com/song/clip-1234abcd")
    );
}

#[test]
fn mp3_omits_track_when_unnumbered() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let tagged = tag_mp3(b"", &meta, None, None).unwrap();

    let tag = id3::Tag::read_from2(Cursor::new(tagged)).unwrap();
    assert_eq!(tag.track(), None);
    assert_eq!(tag.total_tracks(), None);
}

#[test]
fn flac_embeds_webp_cover_and_rejects_oversized() {
    let audio = minimal_flac();
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());

    // A small animated WebP embeds with the image/webp mime, exactly once.
    let webp = b"RIFF\x00\x00\x00\x00WEBP-small-anim".to_vec();
    let tagged = tag_flac(&audio, &meta, Some(Cover::webp(&webp))).unwrap();
    let tag = metaflac::Tag::read_from(&mut Cursor::new(&tagged)).unwrap();
    let pics: Vec<_> = tag.pictures().collect();
    assert_eq!(pics.len(), 1, "exactly one front cover");
    assert_eq!(pics[0].mime_type, "image/webp");
    assert_eq!(pics[0].data, webp);

    // A cover one byte over the 24-bit FLAC picture budget is refused, never
    // silently truncated into a corrupt file.
    let too_big = vec![0u8; flac_picture_data_budget("image/webp") + 1];
    let err = tag_flac(&audio, &meta, Some(Cover::webp(&too_big))).unwrap_err();
    assert!(matches!(err, Error::Tag(_)));
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
    let tagged = tag_wav(&audio, &meta, Some(Cover::jpeg(&cover)), None).unwrap();

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
    assert_eq!(picture.mime_type, "image/jpeg");
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
