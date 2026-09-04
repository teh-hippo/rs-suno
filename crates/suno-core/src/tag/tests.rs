use super::*;
use crate::lineage::ResolveStatus;
use crate::lyrics::{AlignedLine, AlignedLineWord};

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
    AlignedLyrics {
        lines: vec![
            AlignedLine {
                text: "Hello world".to_owned(),
                start_s: 0.5,
                end_s: 1.4,
                section: "Verse 1".to_owned(),
                words: vec![
                    AlignedLineWord {
                        text: "Hello".to_owned(),
                        start_s: 0.5,
                        end_s: 0.9,
                    },
                    AlignedLineWord {
                        text: "world".to_owned(),
                        start_s: 1.0,
                        end_s: 1.4,
                    },
                ],
            },
            AlignedLine {
                text: "again".to_owned(),
                start_s: 61.2,
                end_s: 61.8,
                section: "Chorus".to_owned(),
                words: vec![AlignedLineWord {
                    text: "again".to_owned(),
                    start_s: 61.2,
                    end_s: 61.8,
                }],
            },
        ],
        ..Default::default()
    }
}

#[test]
fn aligned_text_fills_only_missing_inline_lyrics() {
    let aligned = sample_aligned();
    let empty = Clip::default();
    let fallback = TrackMetadata::from_clip_with_alignment(
        &empty,
        &LineageContext::own_root(&empty),
        Some(&aligned),
    );
    assert_eq!(fallback.lyrics, aligned.plain_text());

    let inline = Clip {
        lyrics: "authoritative inline words".to_owned(),
        ..Clip::default()
    };
    let preferred = TrackMetadata::from_clip_with_alignment(
        &inline,
        &LineageContext::own_root(&inline),
        Some(&aligned),
    );
    assert_eq!(preferred.lyrics, "authoritative inline words");
}

#[test]
fn build_sylt_produces_ms_word_entries() {
    let sylt = build_sylt(&sample_aligned(), LyricsTiming::Word).unwrap();
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
fn build_sylt_produces_ms_line_entries() {
    let sylt = build_sylt(&sample_aligned(), LyricsTiming::Line).unwrap();
    assert_eq!(
        sylt.content,
        vec![
            (500, "Hello world".to_owned()),
            (61200, "\nagain".to_owned()),
        ]
    );
}

#[test]
fn build_sylt_is_none_for_empty_alignment() {
    assert!(build_sylt(&AlignedLyrics::default(), LyricsTiming::Line).is_none());
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
    assert_eq!(sylt.content.first(), Some(&(500, "Hello world".to_owned())));
    assert!(tagged.ends_with(b"frames"));
}

#[test]
fn mp3_embeds_word_sylt_when_requested() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let aligned = sample_aligned();
    let tagged =
        tag_mp3_with_timing(b"frames", &meta, None, Some(&aligned), LyricsTiming::Word).unwrap();
    let tag = id3::Tag::read_from2(Cursor::new(tagged)).unwrap();
    let sylt = tag.synchronised_lyrics().next().unwrap();
    assert_eq!(sylt.content.first(), Some(&(500, "Hello".to_owned())));
    assert_eq!(sylt.content.get(1), Some(&(1000, " world".to_owned())));
}

#[test]
fn wav_embeds_selected_word_sylt() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let aligned = sample_aligned();
    let tagged = tag_wav_with_timing(
        &minimal_wav(),
        &meta,
        None,
        Some(&aligned),
        LyricsTiming::Word,
    )
    .unwrap();
    let tag = id3::Tag::read_from2(Cursor::new(tagged)).unwrap();
    let sylt = tag.synchronised_lyrics().next().unwrap();
    assert_eq!(sylt.content.first(), Some(&(500, "Hello".to_owned())));
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
fn mp3_retag_preserves_existing_cover_when_requested() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let cover = b"\xFF\xD8\xFFexisting-cover".to_vec();
    let first = tag_mp3(b"frames", &meta, Some(Cover::jpeg(&cover)), None).unwrap();

    let retagged =
        retag_mp3_with_timing(&first, &meta, None, None, LyricsTiming::Line, true).unwrap();
    let tag = id3::Tag::read_from2(Cursor::new(retagged)).unwrap();
    assert_eq!(tag.pictures().next().unwrap().data, cover);
}

#[test]
fn mp3_retag_removes_existing_cover_when_not_preserved() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let cover = b"\xFF\xD8\xFFexisting-cover".to_vec();
    let first = tag_mp3(b"frames", &meta, Some(Cover::jpeg(&cover)), None).unwrap();

    let retagged =
        retag_mp3_with_timing(&first, &meta, None, None, LyricsTiming::Line, false).unwrap();
    let tag = id3::Tag::read_from2(Cursor::new(retagged)).unwrap();
    assert_eq!(tag.pictures().count(), 0);
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
fn flac_zero_length_streaminfo_errors_not_panics() {
    // A STREAMINFO header that declares a zero-length body: metaflac 0.2.8
    // slices the empty buffer for the mandatory 34-byte STREAMINFO and panics.
    // tag_flac must contain that panic and return an error, per the rule that
    // library code never panics on untrusted input.
    let err = tag_flac(b"fLaC\x00\x00\x00\x00", &TrackMetadata::default(), None)
        .expect_err("malformed FLAC must not tag");
    assert!(matches!(err, Error::Tag(_)));
}

#[test]
fn flac_truncated_vorbis_comment_errors_not_panics() {
    // A valid STREAMINFO followed by a VORBIS_COMMENT block whose 24-bit length
    // over-runs the supplied bytes. This drives the read/write path past the
    // STREAMINFO parse and must still return an error rather than panic.
    let mut streaminfo = vec![0u8; 34];
    streaminfo[0..2].copy_from_slice(&4096u16.to_be_bytes());
    streaminfo[2..4].copy_from_slice(&4096u16.to_be_bytes());
    let packed: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 44_100u64;
    streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());

    let mut audio = Vec::new();
    audio.extend_from_slice(b"fLaC");
    // STREAMINFO: not last, type 0, length 34.
    audio.push(0x00);
    audio.extend_from_slice(&[0x00, 0x00, 0x22]);
    audio.extend_from_slice(&streaminfo);
    // VORBIS_COMMENT: last block, type 4, declares 64 bytes but supplies 4.
    audio.push(0x84);
    audio.extend_from_slice(&[0x00, 0x00, 0x40]);
    audio.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);

    let err = tag_flac(&audio, &TrackMetadata::default(), None)
        .expect_err("truncated VORBIS_COMMENT must not tag");
    assert!(matches!(err, Error::Tag(_)));
}

#[test]
fn flac_wellformed_still_tags_under_panic_guard() {
    // Control: the panic guard must not disturb the happy path, so a well-formed
    // minimal FLAC still tags and round-trips.
    let audio = minimal_flac();
    let tagged = tag_flac(&audio, &TrackMetadata::default(), None).expect("well-formed FLAC tags");
    assert!(metaflac::Tag::read_from(&mut Cursor::new(&tagged)).is_ok());
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

    // A small animated WebP embeds as the front cover with a static JPEG fallback.
    let webp = b"RIFF\x00\x00\x00\x00WEBP-small-anim".to_vec();
    let jpeg = b"\xFF\xD8\xFFstatic-fallback".to_vec();
    let tagged = tag_flac(&audio, &meta, Some(Cover::webp_with_jpeg(&webp, &jpeg))).unwrap();
    let tag = metaflac::Tag::read_from(&mut Cursor::new(&tagged)).unwrap();
    let pics: Vec<_> = tag.pictures().collect();
    assert_eq!(pics.len(), 2);
    let front = pics
        .iter()
        .find(|picture| picture.picture_type == metaflac::block::PictureType::CoverFront)
        .unwrap();
    assert_eq!(front.mime_type, "image/webp");
    assert_eq!(front.data, webp);
    let fallback = pics
        .iter()
        .find(|picture| picture.description == STATIC_FALLBACK_DESCRIPTION)
        .unwrap();
    assert_eq!(fallback.picture_type, metaflac::block::PictureType::Other);
    assert_eq!(fallback.mime_type, "image/jpeg");
    assert_eq!(fallback.data, jpeg);

    let observed = crate::observe_bytes(crate::AudioFormat::Flac, &tagged).unwrap();
    assert_eq!(observed.cover.as_ref().unwrap().mime, "image/webp");
    assert_eq!(
        observed.static_fallback.as_ref().unwrap().mime,
        "image/jpeg"
    );
    assert_ne!(
        observed.managed_cover_fingerprint().as_deref(),
        Some(observed.cover.as_ref().unwrap().fingerprint.as_str())
    );

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
fn wav_retag_preserves_existing_cover_when_requested() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let cover = b"\xFF\xD8\xFFwav-existing-cover".to_vec();
    let first = tag_wav(&minimal_wav(), &meta, Some(Cover::jpeg(&cover)), None).unwrap();

    let retagged =
        retag_wav_with_timing(&first, &meta, None, None, LyricsTiming::Line, true).unwrap();
    let tag = id3::Tag::read_from2(Cursor::new(retagged)).unwrap();
    assert_eq!(tag.pictures().next().unwrap().data, cover);
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

// --- Surgical retagging (#537) -------------------------------------------

/// Assemble a FLAC from `(block type, body)` pairs plus stand-in audio frames,
/// stamping the last-block flag on the final block.
fn flac_from_blocks(blocks: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    for (index, (block_type, body)) in blocks.iter().enumerate() {
        let mut head = *block_type;
        if index + 1 == blocks.len() {
            head |= 0x80;
        }
        out.push(head);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(body);
    }
    out.extend_from_slice(FLAC_AUDIO_FRAMES);
    out
}

/// The 34-byte STREAMINFO body from [`minimal_flac`].
fn streaminfo_body() -> Vec<u8> {
    minimal_flac()[8..42].to_vec()
}

/// A VORBIS_COMMENT block body holding `vendor` and `entries`, in order.
fn comment_block(vendor: &str, entries: &[&str]) -> Vec<u8> {
    let mut out = (vendor.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        out.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        out.extend_from_slice(entry.as_bytes());
    }
    out
}

/// A PICTURE block body of the given API type, mime, and data.
fn picture_block(picture_type: u32, mime: &str, data: &[u8]) -> Vec<u8> {
    picture_block_with_description(picture_type, mime, "", data)
}

fn picture_block_with_description(
    picture_type: u32,
    mime: &str,
    description: &str,
    data: &[u8],
) -> Vec<u8> {
    let mut out = picture_type.to_be_bytes().to_vec();
    out.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    out.extend_from_slice(mime.as_bytes());
    out.extend_from_slice(&(description.len() as u32).to_be_bytes());
    out.extend_from_slice(description.as_bytes());
    for _ in 0..4 {
        out.extend_from_slice(&0u32.to_be_bytes());
    }
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// Split a FLAC back into `(block type, body)` pairs. Deliberately independent
/// of the implementation so the tests check the bytes, not the writer.
fn flac_blocks(audio: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut blocks = Vec::new();
    let mut at = 4;
    loop {
        let head = audio[at];
        let len = u32::from_be_bytes([0, audio[at + 1], audio[at + 2], audio[at + 3]]) as usize;
        blocks.push((head & 0x7f, audio[at + 4..at + 4 + len].to_vec()));
        at += 4 + len;
        if head & 0x80 != 0 {
            break;
        }
    }
    blocks
}

/// The vendor string and ordered `KEY=value` entries of a FLAC's comments.
fn flac_comments(audio: &[u8]) -> (String, Vec<String>) {
    let body = flac_blocks(audio)
        .into_iter()
        .find(|(block_type, _)| *block_type == 4)
        .map(|(_, body)| body)
        .expect("a VORBIS_COMMENT block");
    let mut at = 0;
    let read = |len: usize, at: &mut usize| {
        let taken = String::from_utf8_lossy(&body[*at..*at + len]).into_owned();
        *at += len;
        taken
    };
    let vendor_len = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
    at += 4;
    let vendor = read(vendor_len, &mut at);
    let count = u32::from_le_bytes(body[at..at + 4].try_into().unwrap()) as usize;
    at += 4;
    let mut entries = Vec::new();
    for _ in 0..count {
        let len = u32::from_le_bytes(body[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        entries.push(read(len, &mut at));
    }
    (vendor, entries)
}

/// The `KEY=value` entries a fresh tag of `meta` would write, as a set.
fn owned_entries(meta: &TrackMetadata) -> Vec<String> {
    meta.standard_fields()
        .into_iter()
        .chain(meta.suno_fields())
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

#[test]
fn flac_retag_returns_original_bytes_when_only_the_order_differs() {
    // #537: metaflac serialises the comments through a HashMap, so a run that
    // changed nothing still rewrote every entry in a new order. A retag that
    // finds the same fields, whatever their order, must not touch the file.
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let mut entries = owned_entries(&meta);
    entries.reverse();
    let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
    let audio = flac_from_blocks(&[
        (0, streaminfo_body()),
        (4, comment_block("reference libFLAC 1.4.3 20230623", &refs)),
        (1, vec![0u8; 16]),
    ]);

    let retagged = retag_flac(&audio, &meta, None, true).unwrap();
    assert_eq!(retagged, audio, "an order-only difference is not a change");
}

#[test]
fn flac_retag_adding_lyrics_keeps_order_and_every_other_block() {
    let mut meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    meta.lyrics = String::new();
    let before = owned_entries(&meta);
    let refs: Vec<&str> = before.iter().map(String::as_str).collect();
    let cover = picture_block(3, "image/jpeg", b"\xFF\xD8\xFFcover");
    let padding = vec![0u8; 24];
    let audio = flac_from_blocks(&[
        (0, streaminfo_body()),
        (4, comment_block("vendor", &refs)),
        (6, cover.clone()),
        (1, padding.clone()),
    ]);

    meta.lyrics = "thunder rolls\nover the plains".to_owned();
    let retagged = retag_flac(&audio, &meta, None, true).unwrap();

    let (vendor, entries) = flac_comments(&retagged);
    assert_eq!(vendor, "vendor");
    assert_eq!(
        entries[..before.len()],
        before[..],
        "the original entries keep their order"
    );
    assert_eq!(
        entries[before.len()],
        "LYRICS=thunder rolls\nover the plains",
        "the new field is appended"
    );
    let blocks = flac_blocks(&retagged);
    assert_eq!(
        blocks
            .iter()
            .map(|(block_type, _)| *block_type)
            .collect::<Vec<_>>(),
        vec![0, 4, 6, 1],
        "the block layout is unchanged"
    );
    assert_eq!(blocks[0].1, streaminfo_body());
    assert_eq!(blocks[2].1, cover, "the picture block is byte-identical");
    assert_eq!(blocks[3].1, padding, "the padding block is byte-identical");
    assert!(retagged.ends_with(FLAC_AUDIO_FRAMES));
}

#[test]
fn flac_retag_preserves_vendor_unknown_comments_and_key_casing() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let audio = flac_from_blocks(&[
        (0, streaminfo_body()),
        (
            4,
            comment_block(
                "another tagger 2.1",
                &[
                    "MOOD=calm",
                    "artist=stale",
                    "REPLAYGAIN_TRACK_GAIN=-3.20 dB",
                    "ARTIST=duplicate",
                    "MOOD=also calm",
                ],
            ),
        ),
    ]);

    let retagged = retag_flac(&audio, &meta, None, true).unwrap();
    let (vendor, entries) = flac_comments(&retagged);

    assert_eq!(vendor, "another tagger 2.1", "the vendor string survives");
    assert_eq!(
        entries[0], "MOOD=calm",
        "an unknown comment keeps its place"
    );
    assert_eq!(
        entries[1], "artist=alice",
        "an owned field is rewritten in place, keeping its own casing"
    );
    assert_eq!(
        entries[2], "REPLAYGAIN_TRACK_GAIN=-3.20 dB",
        "another tool's field is untouched"
    );
    assert_eq!(
        entries[3], "MOOD=also calm",
        "a duplicate unknown comment survives"
    );
    assert!(
        !entries.iter().any(|entry| entry == "ARTIST=duplicate"),
        "a duplicate of an owned field is collapsed"
    );
    assert!(entries.contains(&"TITLE=Electric Storm".to_owned()));
}

#[test]
fn flac_retag_removes_an_owned_field_that_lost_its_value() {
    let mut meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    meta.style = String::new();
    let audio = flac_from_blocks(&[
        (0, streaminfo_body()),
        (
            4,
            comment_block("v", &["SUNO_STYLE=ambient, cinematic", "MOOD=calm"]),
        ),
    ]);

    let retagged = retag_flac(&audio, &meta, None, true).unwrap();
    let (_, entries) = flac_comments(&retagged);
    assert!(
        !entries.iter().any(|entry| entry.starts_with("SUNO_STYLE=")),
        "an owned field with no value is dropped"
    );
    assert_eq!(entries[0], "MOOD=calm", "the unknown comment is kept");
}

#[test]
fn flac_retag_keeps_lyrics_when_the_run_has_none() {
    let mut meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    meta.lyrics = String::new();
    let audio = flac_from_blocks(&[
        (0, streaminfo_body()),
        (4, comment_block("v", &["LYRICS=already embedded"])),
    ]);

    let retagged = retag_flac(&audio, &meta, None, true).unwrap();
    let (_, entries) = flac_comments(&retagged);
    assert!(entries.contains(&"LYRICS=already embedded".to_owned()));
}

#[test]
fn flac_retag_writes_and_clears_track_numbers() {
    let mut meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    meta.track = 3;
    meta.track_total = 12;
    let audio = flac_from_blocks(&[(0, streaminfo_body()), (4, comment_block("v", &[]))]);

    let numbered = retag_flac(&audio, &meta, None, true).unwrap();
    let (_, entries) = flac_comments(&numbered);
    assert!(entries.contains(&"TRACKNUMBER=3".to_owned()));
    assert!(entries.contains(&"TRACKTOTAL=12".to_owned()));

    meta.track = 0;
    meta.track_total = 0;
    let cleared = retag_flac(&numbered, &meta, None, true).unwrap();
    let (_, entries) = flac_comments(&cleared);
    assert!(!entries.iter().any(|entry| entry.starts_with("TRACK")));
}

#[test]
fn flac_retag_replaces_only_the_front_cover() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let front = picture_block(3, "image/jpeg", b"\xFF\xD8\xFFold-front");
    let band = picture_block(19, "image/png", b"\x89PNGband-logo");
    let audio = flac_from_blocks(&[
        (0, streaminfo_body()),
        (4, comment_block("v", &["MOOD=calm"])),
        (6, front),
        (6, band.clone()),
    ]);

    let new_cover = b"\xFF\xD8\xFFnew-front".to_vec();
    let retagged = retag_flac(&audio, &meta, Some(Cover::jpeg(&new_cover)), false).unwrap();
    let blocks = flac_blocks(&retagged);
    let pictures: Vec<&Vec<u8>> = blocks
        .iter()
        .filter(|(block_type, _)| *block_type == 6)
        .map(|(_, body)| body)
        .collect();
    assert_eq!(pictures.len(), 2, "the band logo is not dropped");
    assert_eq!(pictures[0], &picture_block(3, "image/jpeg", &new_cover));
    assert_eq!(pictures[1], &band, "an unrelated picture is untouched");

    // With no replacement, preserving keeps both and clearing drops only ours.
    let kept = retag_flac(&retagged, &meta, None, true).unwrap();
    assert_eq!(kept, retagged, "preserving art is a no-op");
    let cleared = retag_flac(&retagged, &meta, None, false).unwrap();
    let remaining: Vec<u8> = flac_blocks(&cleared)
        .into_iter()
        .filter(|(block_type, _)| *block_type == 6)
        .map(|(_, body)| body[3])
        .collect();
    assert_eq!(remaining, vec![19], "only the front cover is removed");
}

#[test]
fn flac_retag_replaces_managed_static_fallback_and_preserves_foreign_pictures() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let front = picture_block(3, "image/webp", b"old-webp");
    let fallback =
        picture_block_with_description(0, "image/jpeg", STATIC_FALLBACK_DESCRIPTION, b"old-jpeg");
    let band = picture_block(19, "image/png", b"\x89PNGband-logo");
    let audio = flac_from_blocks(&[
        (0, streaminfo_body()),
        (4, comment_block("v", &[])),
        (6, front),
        (6, fallback),
        (6, band.clone()),
    ]);

    let retagged = retag_flac(
        &audio,
        &meta,
        Some(Cover::webp_with_jpeg(b"new-webp", b"new-jpeg")),
        false,
    )
    .unwrap();
    let pictures: Vec<Vec<u8>> = flac_blocks(&retagged)
        .into_iter()
        .filter(|(block_type, _)| *block_type == 6)
        .map(|(_, body)| body)
        .collect();
    assert_eq!(
        pictures,
        vec![
            picture_block(3, "image/webp", b"new-webp"),
            picture_block_with_description(
                0,
                "image/jpeg",
                STATIC_FALLBACK_DESCRIPTION,
                b"new-jpeg",
            ),
            band.clone(),
        ]
    );

    let cleared = retag_flac(&retagged, &meta, None, false).unwrap();
    let remaining: Vec<Vec<u8>> = flac_blocks(&cleared)
        .into_iter()
        .filter(|(block_type, _)| *block_type == 6)
        .map(|(_, body)| body)
        .collect();
    assert_eq!(remaining, vec![band]);
}

#[test]
fn flac_retag_rejects_an_oversized_cover() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let audio = flac_from_blocks(&[(0, streaminfo_body()), (4, comment_block("v", &[]))]);
    let too_big = vec![0u8; flac_picture_data_budget("image/webp") + 1];
    let err = retag_flac(&audio, &meta, Some(Cover::webp(&too_big)), false).unwrap_err();
    assert!(matches!(err, Error::Tag(_)));
}

#[test]
fn flac_retag_errors_rather_than_panics_on_malformed_input() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"not a flac stream".to_vec(),
        b"fLaC".to_vec(),
        // A STREAMINFO header promising 34 bytes that are not there.
        b"fLaC\x80\x00\x00\x22cut".to_vec(),
        // A comment block whose entry count runs past the block body.
        {
            let mut body = comment_block("v", &["MOOD=calm"]);
            body[4 + 1..4 + 1 + 4].copy_from_slice(&9u32.to_le_bytes());
            flac_from_blocks(&[(0, streaminfo_body()), (4, body)])
        },
    ];
    for case in cases {
        let err = retag_flac(&case, &meta, None, true)
            .expect_err("malformed input must not tag or panic");
        assert!(matches!(err, Error::Tag(_)));
    }
}

#[test]
fn flac_retag_accepts_a_file_with_no_comment_block() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let audio = minimal_flac();
    let retagged = retag_flac(&audio, &meta, None, true).unwrap();
    let blocks = flac_blocks(&retagged);
    assert_eq!(
        blocks
            .iter()
            .map(|(block_type, _)| *block_type)
            .collect::<Vec<_>>(),
        vec![0, 4],
        "the comments are inserted after STREAMINFO"
    );
    assert!(retagged.ends_with(FLAC_AUDIO_FRAMES));
    // Reading it back through metaflac proves the block is well formed.
    let tag = metaflac::Tag::read_from(&mut Cursor::new(&retagged)).unwrap();
    assert_eq!(
        tag.get_vorbis("TITLE").map(|v| v.collect::<Vec<_>>()),
        Some(vec!["Electric Storm"])
    );
}

#[test]
fn flac_retag_settles_after_a_fresh_tag() {
    // The fresh path writes through metaflac; a retag of its output must find
    // nothing to change, which is what stops the rewrite loop in #537.
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let fresh = tag_flac(&minimal_flac(), &meta, None).unwrap();
    let retagged = retag_flac(&fresh, &meta, None, true).unwrap();
    assert_eq!(retagged, fresh, "a retag of a current file changes nothing");
}

/// An ID3 tag holding frames this crate does not own, to prove a retag keeps
/// them: a genre, a rating, a ReplayGain `TXXX`, a French comment, a described
/// `USLT`, and a `SYLT` in another language.
fn foreign_id3(audio: &[u8]) -> Vec<u8> {
    let mut tag = id3::Tag::new();
    tag.set_genre("Ambient");
    tag.add_frame(id3::frame::Popularimeter {
        user: "rater@example.com".to_owned(),
        rating: 196,
        counter: 7,
    });
    tag.add_frame(ExtendedText {
        description: "REPLAYGAIN_TRACK_GAIN".to_owned(),
        value: "-3.20 dB".to_owned(),
    });
    tag.add_frame(Comment {
        lang: "fra".to_owned(),
        description: String::new(),
        text: "une note".to_owned(),
    });
    tag.add_frame(Lyrics {
        lang: LANG.to_owned(),
        description: "translation".to_owned(),
        text: "a translated line".to_owned(),
    });
    let mut cursor = Cursor::new(audio.to_vec());
    tag.write_to_file(&mut cursor, id3::Version::Id3v24)
        .unwrap();
    cursor.into_inner()
}

#[test]
fn mp3_retag_preserves_frames_this_crate_does_not_own() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let existing = foreign_id3(b"frames");

    let retagged =
        retag_mp3_with_timing(&existing, &meta, None, None, LyricsTiming::Line, true).unwrap();
    let tag = id3::Tag::read_from2(Cursor::new(&retagged)).unwrap();

    assert_eq!(tag.genre(), Some("Ambient"), "an unowned text frame stays");
    assert_eq!(
        tag.frames()
            .find(|frame| frame.id() == "POPM")
            .and_then(|frame| frame.content().popularimeter())
            .map(|popm| popm.rating),
        Some(196),
        "a rating survives"
    );
    assert_eq!(
        tag.extended_texts()
            .find(|txxx| txxx.description == "REPLAYGAIN_TRACK_GAIN")
            .map(|txxx| txxx.value.as_str()),
        Some("-3.20 dB"),
        "another tool's TXXX survives"
    );
    assert_eq!(
        tag.comments()
            .find(|comment| comment.lang == "fra")
            .map(|comment| comment.text.as_str()),
        Some("une note"),
        "a comment in another language survives"
    );
    assert_eq!(
        tag.lyrics()
            .find(|lyrics| lyrics.description == "translation")
            .map(|lyrics| lyrics.text.as_str()),
        Some("a translated line"),
        "a described USLT survives"
    );
    // And this crate's own fields are written.
    assert_eq!(tag.title(), Some("Electric Storm"));
    assert_eq!(
        tag.extended_texts()
            .find(|txxx| txxx.description == "SUNO_ID")
            .map(|txxx| txxx.value.as_str()),
        Some("clip-1234abcd")
    );
}

#[test]
fn mp3_fresh_tagging_still_drops_frames_it_does_not_own() {
    // The fresh path is a rebuild, not a merge: only lyrics and art carry over.
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let tagged = tag_mp3(&foreign_id3(b"frames"), &meta, None, None).unwrap();
    let tag = id3::Tag::read_from2(Cursor::new(&tagged)).unwrap();
    assert_eq!(tag.genre(), None);
    assert!(tag.frames().all(|frame| frame.id() != "POPM"));
    assert_eq!(tag.comments().filter(|c| c.lang == "fra").count(), 0);
    assert_eq!(
        tag.lyrics().count(),
        1,
        "this run's own lyrics replace the whole existing set"
    );
}

#[test]
fn mp3_retag_returns_original_bytes_when_nothing_changed() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let first = tag_mp3(b"frames", &meta, None, None).unwrap();
    let retagged =
        retag_mp3_with_timing(&first, &meta, None, None, LyricsTiming::Line, true).unwrap();
    assert_eq!(retagged, first, "a semantic no-op does not rewrite bytes");
}

#[test]
fn mp3_retag_rewrites_when_a_field_changes() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let first = tag_mp3(b"frames", &meta, None, None).unwrap();
    let mut changed = meta.clone();
    changed.album = "Storm Series".to_owned();
    let retagged =
        retag_mp3_with_timing(&first, &changed, None, None, LyricsTiming::Line, true).unwrap();
    assert_ne!(retagged, first);
    let tag = id3::Tag::read_from2(Cursor::new(&retagged)).unwrap();
    assert_eq!(tag.album(), Some("Storm Series"));
    assert_eq!(
        tag.frames().filter(|frame| frame.id() == "TALB").count(),
        1,
        "the owned frame is replaced, not stacked"
    );
}

#[test]
fn mp3_retag_clears_an_owned_field_that_lost_its_value() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let first = tag_mp3(b"frames", &meta, None, None).unwrap();
    let mut cleared = meta.clone();
    cleared.style = String::new();
    cleared.album = String::new();
    let retagged =
        retag_mp3_with_timing(&first, &cleared, None, None, LyricsTiming::Line, true).unwrap();
    let tag = id3::Tag::read_from2(Cursor::new(&retagged)).unwrap();
    assert_eq!(tag.album(), None, "an owned frame with no value is removed");
    assert!(
        tag.extended_texts()
            .all(|txxx| txxx.description != "SUNO_STYLE"),
        "an owned TXXX with no value is removed"
    );
    assert_eq!(tag.title(), Some("Electric Storm"), "the rest is intact");
}

#[test]
fn mp3_retag_of_an_untagged_file_falls_back_to_fresh_tagging() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let retagged =
        retag_mp3_with_timing(b"frames", &meta, None, None, LyricsTiming::Line, true).unwrap();
    let tag = id3::Tag::read_from2(Cursor::new(&retagged)).unwrap();
    assert_eq!(tag.title(), Some("Electric Storm"));
}

#[test]
fn wav_retag_preserves_foreign_frames_and_no_ops() {
    let meta = TrackMetadata::from_clip(&full_clip(), &full_lineage());
    let first = tag_wav(&minimal_wav(), &meta, None, None).unwrap();
    let mut tag = id3::Tag::read_from2(Cursor::new(&first)).unwrap();
    tag.set_genre("Ambient");
    let mut cursor = Cursor::new(first.clone());
    tag.write_to_file(&mut cursor, id3::Version::Id3v24)
        .unwrap();
    let with_genre = cursor.into_inner();

    let retagged =
        retag_wav_with_timing(&with_genre, &meta, None, None, LyricsTiming::Line, true).unwrap();
    assert_eq!(
        retagged, with_genre,
        "a semantic no-op leaves the WAV alone"
    );
    let tag = id3::Tag::read_from2(Cursor::new(&retagged)).unwrap();
    assert_eq!(tag.genre(), Some("Ambient"));
    assert!(
        retagged
            .windows(WAV_AUDIO_DATA.len())
            .any(|window| window == WAV_AUDIO_DATA),
        "the audio samples are intact"
    );
}
