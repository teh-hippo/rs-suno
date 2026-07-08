//! Word- and line-level timed (synced) lyrics from Suno's aligned-lyrics API.
//!
//! [`AlignedLyrics`] is the domain shape of `GET /api/gen/{id}/aligned_lyrics/v2/`
//! ([`SunoClient::aligned_lyrics`](crate::SunoClient::aligned_lyrics)): a flat
//! word-level list plus a line-level list carrying section labels and nested
//! per-word timing. Decoding the API body into it lives in
//! `wire::parse_aligned_lyrics`; everything here is pure and free of direct IO,
//! rendering the synced artefacts (the line-synced `.lrc` body, a word-level ID3
//! `SYLT` table, and a plain-text fallback), so the formatting is unit tested
//! without a network.
//!
//! Instrumentals (and any clip Suno could not force-align) return `200` with
//! empty arrays, so [`AlignedLyrics::is_empty`] is the signal to write no synced
//! artefact for that clip, exactly as an empty cover URL writes no cover.

use std::fmt::Write as _;

/// One force-aligned word from the flat `aligned_words` list.
///
/// `success` is Suno's per-word alignment flag (it can be `false` where forced
/// alignment failed) and `p_align` its confidence; both are carried so callers
/// can gate on them, though the line-level [`AlignedLine::words`] is preferred
/// for rendering because it already reflects Suno's own line grouping.
#[derive(Debug, Clone, PartialEq)]
pub struct AlignedWord {
    pub word: String,
    pub success: bool,
    pub start_s: f64,
    pub end_s: f64,
    pub p_align: f64,
}

/// One word within a line, from the nested `aligned_lyrics[].words` list.
///
/// The API keys the word text as `text` here (the flat list keys it as `word`).
/// These carry no `success`/`p_align`; they are Suno's authoritative grouping of
/// words into lines and are what the `.lrc` and `SYLT` renderers use.
#[derive(Debug, Clone, PartialEq)]
pub struct AlignedLineWord {
    pub text: String,
    pub start_s: f64,
    pub end_s: f64,
}

/// One aligned line: its text, span, section label, and nested words.
#[derive(Debug, Clone, PartialEq)]
pub struct AlignedLine {
    pub text: String,
    pub start_s: f64,
    pub end_s: f64,
    /// Structural section label (e.g. `Verse 1`, `Chorus`), empty when absent.
    pub section: String,
    pub words: Vec<AlignedLineWord>,
}

/// A clip's aligned lyrics: the flat word list and the line list.
///
/// Both are empty for an instrumental or an un-alignable clip; see
/// [`is_empty`](Self::is_empty).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlignedLyrics {
    pub words: Vec<AlignedWord>,
    pub lines: Vec<AlignedLine>,
    /// `waveform_data`: the amplitude/peak envelope Suno returns for waveform
    /// display, empty when absent. Additive metadata, not lyric content, so it
    /// does not affect [`is_empty`](Self::is_empty).
    pub waveform_data: Vec<f64>,
    /// `hoot_cer`: Suno's alignment/transcription error metric (higher is
    /// worse), `None` when absent.
    pub hoot_cer: Option<f64>,
    /// `is_streamed`: Suno's streaming flag, `None` when absent.
    pub is_streamed: Option<bool>,
}

impl AlignedLyrics {
    /// True when the clip carries no aligned lyrics (an instrumental, or a clip
    /// Suno could not align). No synced artefact is written for such a clip.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.words.is_empty()
    }

    /// The plain lyric text, one line per aligned line (falling back to the flat
    /// word list when there are no lines), for the unsynced `LYRICS`/`USLT` tag.
    ///
    /// Returns an empty string when there is nothing to embed.
    pub fn plain_text(&self) -> String {
        if !self.lines.is_empty() {
            return self
                .lines
                .iter()
                .map(|line| line.text.trim_end())
                .collect::<Vec<_>>()
                .join("\n");
        }
        self.words
            .iter()
            .map(|word| word.word.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The body of a standard (line-level) `.lrc`: one `[mm:ss.xx]` stamp per
    /// aligned line, followed by the line text.
    ///
    /// Line-level is the universally supported LRC form, so every player syncs
    /// and displays it cleanly; the enhanced "A2" per-word `<mm:ss.xx>` tags are
    /// parsed by only a few karaoke players and are shown as literal text by the
    /// rest, so they are not emitted here. Word-level timing is carried instead
    /// in the MP3 `SYLT` frame (see [`sylt_entries`](Self::sylt_entries)). A line
    /// with empty text falls back to its nested words joined by spaces. The body
    /// is empty when there are no lines; callers treat that as "no `.lrc`".
    pub fn lrc_body(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            let text = if line.text.trim().is_empty() {
                line.words
                    .iter()
                    .map(|w| w.text.trim())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                line.text.trim().to_owned()
            };
            let _ = writeln!(out, "[{}]{text}", lrc_stamp(line.start_s));
        }
        out
    }

    /// Word-level `SYLT` content: `(offset_ms, text)` pairs in time order.
    ///
    /// Each new line's first word carries a leading newline so a player renders
    /// line breaks (the ID3v2 `SYLT` convention). Uses Suno's own line grouping;
    /// a line with no nested words contributes its whole text as one segment.
    pub fn sylt_entries(&self) -> Vec<(u32, String)> {
        let mut entries = Vec::new();
        for (line_index, line) in self.lines.iter().enumerate() {
            let words: Vec<&AlignedLineWord> = line
                .words
                .iter()
                .filter(|w| !w.text.trim().is_empty())
                .collect();
            let prefix = if line_index == 0 { "" } else { "\n" };
            if words.is_empty() {
                let text = line.text.trim();
                if !text.is_empty() {
                    entries.push((to_ms(line.start_s), format!("{prefix}{text}")));
                }
                continue;
            }
            for (word_index, word) in words.iter().enumerate() {
                let text = word.text.trim();
                let segment = if word_index == 0 {
                    format!("{prefix}{text}")
                } else {
                    format!(" {text}")
                };
                entries.push((to_ms(word.start_s), segment));
            }
        }
        entries
    }
}

/// Total whole milliseconds for `secs`, clamped at zero (never negative).
fn to_ms(secs: f64) -> u32 {
    if !secs.is_finite() || secs <= 0.0 {
        return 0;
    }
    (secs * 1000.0).round() as u32
}

/// Format `secs` as an LRC line stamp `mm:ss.xx` (centiseconds), with minutes
/// allowed to exceed 59 so a long track is not wrapped.
fn lrc_stamp(secs: f64) -> String {
    let cs = centiseconds(secs);
    format!("{:02}:{:02}.{:02}", cs / 6000, (cs / 100) % 60, cs % 100)
}

fn centiseconds(secs: f64) -> u64 {
    if !secs.is_finite() || secs <= 0.0 {
        return 0;
    }
    (secs * 100.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same two-line sample as the decode tests, built directly so the
    /// render tests depend on the domain types, not on JSON decoding.
    fn sample_aligned() -> AlignedLyrics {
        AlignedLyrics {
            words: vec![
                AlignedWord {
                    word: "Hello".to_owned(),
                    success: true,
                    start_s: 0.5,
                    end_s: 0.9,
                    p_align: 0.99,
                },
                AlignedWord {
                    word: "world".to_owned(),
                    success: true,
                    start_s: 1.0,
                    end_s: 1.4,
                    p_align: 0.98,
                },
                AlignedWord {
                    word: "again".to_owned(),
                    success: true,
                    start_s: 61.2,
                    end_s: 61.8,
                    p_align: 0.97,
                },
            ],
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
                    text: "[Chorus]".to_owned(),
                    start_s: 60.0,
                    end_s: 60.0,
                    section: "Chorus".to_owned(),
                    words: vec![],
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
    fn lrc_body_has_line_level_stamps() {
        let aligned = sample_aligned();
        let body = aligned.lrc_body();
        let expected = "[00:00.50]Hello world\n\
             [01:00.00][Chorus]\n\
             [01:01.20]again\n";
        assert_eq!(body, expected);
    }

    #[test]
    fn plain_text_joins_line_text() {
        let aligned = sample_aligned();
        assert_eq!(aligned.plain_text(), "Hello world\n[Chorus]\nagain");
    }

    #[test]
    fn sylt_entries_are_word_level_with_line_breaks() {
        let aligned = sample_aligned();
        let entries = aligned.sylt_entries();
        assert_eq!(
            entries,
            vec![
                (500, "Hello".to_owned()),
                (1000, " world".to_owned()),
                (60000, "\n[Chorus]".to_owned()),
                (61200, "\nagain".to_owned()),
            ]
        );
    }

    #[test]
    fn stamps_round_and_do_not_wrap_minutes() {
        // 61.2s -> 01:01.20; a value over an hour stays in minutes (not hours).
        assert_eq!(lrc_stamp(61.2), "01:01.20");
        assert_eq!(lrc_stamp(3661.0), "61:01.00");
        assert_eq!(to_ms(1.2346), 1235);
        assert_eq!(to_ms(-1.0), 0);
    }
}
