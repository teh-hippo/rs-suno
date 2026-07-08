//! Clip-level lyric sidecar renderers: the `.lyrics.txt` text and the
//! timed/untimed `.lrc`, built from a clip, its lineage, and Suno's aligned
//! lyrics. The pure timed-lyrics primitives live in the sibling `aligned` leaf.

use std::fmt::Write as _;

use super::AlignedLyrics;
use crate::lineage::LineageContext;
use crate::model::Clip;
use crate::tag::TrackMetadata;
use crate::textfmt::{format_duration, to_single_line};

/// Render the plain-text lyrics sidecar for `clip`, or `None` when it has none.
///
/// The clip's own `lyrics`, verbatim, normalised to one trailing newline. Empty
/// or whitespace-only lyrics return `None` so no empty `.lyrics.txt` is written.
/// The generation prompt is not used here (it lives in the details sidecar).
pub fn render_clip_lyrics(clip: &Clip) -> Option<String> {
    if clip.lyrics.trim().is_empty() {
        return None;
    }
    Some(format!("{}\n", clip.lyrics.trim_end()))
}

/// Render an untimed `.lrc` sidecar for `clip`, or `None` when it has no lyrics.
///
/// Plain lyric lines with no timestamps, under the shared `.lrc` header. Empty
/// or whitespace-only lyrics return `None` so no empty `.lrc` is written. This is
/// the fallback when Suno has no alignment; the synced [`render_synced_lrc`]
/// supersedes it at the same path when available.
pub fn render_clip_lrc(clip: &Clip, lineage: &LineageContext) -> Option<String> {
    if clip.lyrics.trim().is_empty() {
        return None;
    }
    let mut out = lrc_headers(clip, lineage);
    for line in clip.lyrics.trim_end().lines() {
        let _ = writeln!(out, "{line}");
    }
    Some(out)
}

/// Render a synced (timed) `.lrc` sidecar for `clip` from Suno's `aligned`
/// lyrics, or `None` when there is nothing to time (an instrumental).
///
/// Same header as [`render_clip_lrc`]; the body is the line-level form from
/// [`AlignedLyrics::lrc_body`], one `[mm:ss.xx]` stamp per line. Word-level
/// timing rides the MP3 `SYLT` frame, not the `.lrc`. Returns `None` when there
/// are no timed lines.
pub fn render_synced_lrc(
    clip: &Clip,
    lineage: &LineageContext,
    aligned: &AlignedLyrics,
) -> Option<String> {
    let body = aligned.lrc_body();
    if body.is_empty() {
        return None;
    }
    let mut out = lrc_headers(clip, lineage);
    out.push_str(&body);
    Some(out)
}

/// The shared `.lrc` header block: `[ti:]`, `[ar:]`, `[al:]`, `[length:]` (each
/// omitted when empty or unknown), plus the constant `[re:rs-suno]` tool tag.
fn lrc_headers(clip: &Clip, lineage: &LineageContext) -> String {
    let meta = TrackMetadata::from_clip(clip, lineage);
    let length = format_duration(clip.duration);
    let headers: [(&str, &str); 5] = [
        ("ti", &meta.title),
        ("ar", &meta.artist),
        ("al", &meta.album),
        ("length", &length),
        ("re", "rs-suno"),
    ];
    let mut out = String::new();
    for (tag, value) in headers {
        if value.is_empty() {
            continue;
        }
        let _ = writeln!(out, "[{tag}:{}]", to_single_line(value));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lineage::{EdgeType, ResolveStatus};

    fn full_clip() -> Clip {
        Clip {
            id: "clip-1234abcd".to_owned(),
            title: "Electric Storm".to_owned(),
            tags: "ambient, cinematic".to_owned(),
            duration: 211.6,
            created_at: "2024-03-10T14:22:01Z".to_owned(),
            display_name: "alice".to_owned(),
            handle: "alice".to_owned(),
            prompt: "an orchestral storm".to_owned(),
            gpt_description_prompt: "a moody cinematic build".to_owned(),
            lyrics: "thunder rolls\nover the plains".to_owned(),
            model_name: "chirp-v4".to_owned(),
            major_model_version: "v4".to_owned(),
            image_large_url: "https://cdn1.suno.ai/signed?token=secret".to_owned(),
            audio_url: "https://cdn1.suno.ai/clip-1234abcd.mp3".to_owned(),
            ..Clip::default()
        }
    }

    fn full_lineage() -> LineageContext {
        LineageContext {
            root_id: "rootid567890".to_owned(),
            root_title: "Weather Series".to_owned(),
            root_date: String::new(),
            parent_id: "parentid1234".to_owned(),
            edge_type: Some(EdgeType::Extend),
            status: ResolveStatus::Resolved,
            track: 0,
            track_total: 0,
        }
    }

    #[test]
    fn lyrics_render_verbatim_with_one_trailing_newline() {
        let clip = Clip {
            lyrics: "line one\nline two".to_owned(),
            ..Clip::default()
        };
        assert_eq!(
            render_clip_lyrics(&clip),
            Some("line one\nline two\n".to_owned())
        );
    }

    #[test]
    fn lyrics_normalise_trailing_whitespace_to_one_newline() {
        let clip = Clip {
            lyrics: "verse\n\n\n".to_owned(),
            ..Clip::default()
        };
        assert_eq!(render_clip_lyrics(&clip), Some("verse\n".to_owned()));
    }

    #[test]
    fn lyrics_none_when_empty_or_whitespace_only() {
        assert_eq!(render_clip_lyrics(&Clip::default()), None);
        let clip = Clip {
            lyrics: "  \n\t \n".to_owned(),
            ..Clip::default()
        };
        assert_eq!(render_clip_lyrics(&clip), None);
    }

    #[test]
    fn lyrics_use_clip_lyrics_not_prompt() {
        let clip = Clip {
            prompt: "the generation prompt".to_owned(),
            lyrics: "the actual sung words".to_owned(),
            ..Clip::default()
        };
        let rendered = render_clip_lyrics(&clip).unwrap();
        assert!(rendered.contains("the actual sung words"));
        assert!(!rendered.contains("the generation prompt"));
    }

    #[test]
    fn lrc_none_when_lyrics_blank() {
        let empty = Clip::default();
        assert_eq!(
            render_clip_lrc(&empty, &LineageContext::own_root(&empty)),
            None
        );
        let clip = Clip {
            lyrics: "  \n\t \n".to_owned(),
            ..Clip::default()
        };
        assert_eq!(
            render_clip_lrc(&clip, &LineageContext::own_root(&clip)),
            None
        );
    }

    #[test]
    fn lrc_renders_untimed_body_with_headers() {
        let rendered = render_clip_lrc(&full_clip(), &full_lineage()).unwrap();
        let expected = "[ti:Electric Storm]\n\
        [ar:alice]\n\
        [al:Weather Series]\n\
        [length:3:32]\n\
        [re:rs-suno]\n\
        thunder rolls\n\
        over the plains\n";
        assert_eq!(rendered, expected);
        // Untimed: no per-line `[mm:ss.xx]` timestamps.
        assert!(!rendered.contains("[00:"));
    }

    #[test]
    fn lrc_omits_unknown_headers() {
        let clip = Clip {
            title: "Bare".to_owned(),
            lyrics: "one line".to_owned(),
            ..Clip::default()
        };
        let rendered = render_clip_lrc(&clip, &LineageContext::own_root(&clip)).unwrap();
        // No duration, so `[length:]` is omitted; artist falls back to Suno and
        // album to the title. The constant tool tag is always present.
        assert!(!rendered.contains("[length:"));
        assert!(rendered.contains("[ti:Bare]\n"));
        assert!(rendered.contains("[re:rs-suno]\n"));
        assert!(rendered.ends_with("one line\n"));
    }

    fn sample_aligned() -> crate::lyrics::AlignedLyrics {
        crate::lyrics::AlignedLyrics {
            lines: vec![crate::lyrics::AlignedLine {
                text: "thunder rolls".to_owned(),
                start_s: 1.5,
                end_s: 2.4,
                section: "Verse 1".to_owned(),
                words: vec![
                    crate::lyrics::AlignedLineWord {
                        text: "thunder".to_owned(),
                        start_s: 1.5,
                        end_s: 2.0,
                    },
                    crate::lyrics::AlignedLineWord {
                        text: "rolls".to_owned(),
                        start_s: 2.1,
                        end_s: 2.4,
                    },
                ],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn synced_lrc_has_headers_then_line_stamps() {
        let rendered = render_synced_lrc(&full_clip(), &full_lineage(), &sample_aligned()).unwrap();
        let expected = "[ti:Electric Storm]\n\
        [ar:alice]\n\
        [al:Weather Series]\n\
        [length:3:32]\n\
        [re:rs-suno]\n\
        [00:01.50]thunder rolls\n";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn synced_lrc_is_none_for_empty_alignment() {
        // An instrumental (empty arrays) writes no synced `.lrc`, exactly as an
        // empty cover URL writes no cover.
        let empty = crate::lyrics::AlignedLyrics::default();
        assert_eq!(
            render_synced_lrc(&full_clip(), &full_lineage(), &empty),
            None
        );
    }
}
