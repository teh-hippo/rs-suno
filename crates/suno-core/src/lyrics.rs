//! Word- and line-level timed (synced) lyrics from Suno's aligned-lyrics API.
//!
//! [`AlignedLyrics`] is the parsed shape of `GET /api/gen/{id}/aligned_lyrics/v2/`
//! ([`SunoClient::aligned_lyrics`](crate::SunoClient::aligned_lyrics)): a flat
//! word-level list plus a line-level list carrying section labels and nested
//! per-word timing. Everything here is pure and free of direct IO — it renders
//! the synced artefacts (the line-synced `.lrc` body, a word-level ID3 `SYLT`
//! table, and a plain-text fallback), so the mapping and formatting are unit
//! tested without a network.
//!
//! Instrumentals (and any clip Suno could not force-align) return `200` with
//! empty arrays, so [`AlignedLyrics::is_empty`] is the signal to write no synced
//! artefact for that clip, exactly as an empty cover URL writes no cover.

use std::fmt::Write as _;

use serde_json::Value;

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
    /// Map the `aligned_lyrics/v2` response body, tolerating missing keys.
    ///
    /// A non-object body, or one whose arrays are missing, maps to the empty
    /// value, so a malformed or instrumental response is simply "no synced
    /// lyrics" rather than an error.
    pub fn from_json(raw: &Value) -> AlignedLyrics {
        let words = raw
            .get("aligned_words")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(parse_word).collect())
            .unwrap_or_default();
        let lines = raw
            .get("aligned_lyrics")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(parse_line).collect())
            .unwrap_or_default();
        let waveform_data = raw
            .get("waveform_data")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        let hoot_cer = raw.get("hoot_cer").and_then(Value::as_f64);
        let is_streamed = raw.get("is_streamed").and_then(Value::as_bool);
        AlignedLyrics {
            words,
            lines,
            waveform_data,
            hoot_cer,
            is_streamed,
        }
    }

    /// Parse the `aligned_lyrics/v2` response bytes, or the empty value when the
    /// body is not valid JSON (defensive: an odd body means "no synced lyrics").
    pub fn from_bytes(body: &[u8]) -> AlignedLyrics {
        serde_json::from_slice::<Value>(body)
            .map(|value| Self::from_json(&value))
            .unwrap_or_default()
    }

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

fn parse_word(raw: &Value) -> AlignedWord {
    AlignedWord {
        word: string(raw, "word"),
        success: raw.get("success").and_then(Value::as_bool).unwrap_or(false),
        start_s: f64_field(raw, "start_s"),
        end_s: f64_field(raw, "end_s"),
        p_align: f64_field(raw, "p_align"),
    }
}

fn parse_line(raw: &Value) -> AlignedLine {
    let words = raw
        .get("words")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|word| AlignedLineWord {
                    text: string(word, "text"),
                    start_s: f64_field(word, "start_s"),
                    end_s: f64_field(word, "end_s"),
                })
                .collect()
        })
        .unwrap_or_default();
    AlignedLine {
        text: string(raw, "text"),
        start_s: f64_field(raw, "start_s"),
        end_s: f64_field(raw, "end_s"),
        section: string(raw, "section"),
        words,
    }
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn f64_field(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
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

    /// A small two-line sample with per-word timing, mirroring the live shape.
    fn sample_json() -> Value {
        serde_json::json!({
            "aligned_words": [
                {"word": "Hello", "success": true, "start_s": 0.5, "end_s": 0.9, "p_align": 0.99},
                {"word": "world", "success": true, "start_s": 1.0, "end_s": 1.4, "p_align": 0.98},
                {"word": "again", "success": true, "start_s": 61.2, "end_s": 61.8, "p_align": 0.97}
            ],
            "aligned_lyrics": [
                {"text": "Hello world", "start_s": 0.5, "end_s": 1.4, "section": "Verse 1",
                 "words": [
                     {"text": "Hello", "start_s": 0.5, "end_s": 0.9},
                     {"text": "world", "start_s": 1.0, "end_s": 1.4}
                 ]},
                {"text": "[Chorus]", "start_s": 60.0, "end_s": 60.0, "section": "Chorus", "words": []},
                {"text": "again", "start_s": 61.2, "end_s": 61.8, "section": "Chorus",
                 "words": [{"text": "again", "start_s": 61.2, "end_s": 61.8}]}
            ],
            "hoot_cer": 0.22,
            "is_streamed": false
        })
    }

    #[test]
    fn parses_words_and_lines() {
        let aligned = AlignedLyrics::from_json(&sample_json());
        assert_eq!(aligned.words.len(), 3);
        assert_eq!(aligned.lines.len(), 3);
        assert_eq!(aligned.words[0].word, "Hello");
        assert!(aligned.words[0].success);
        assert!((aligned.words[0].p_align - 0.99).abs() < 1e-9);
        assert_eq!(aligned.lines[0].section, "Verse 1");
        assert_eq!(aligned.lines[0].words.len(), 2);
        assert_eq!(aligned.lines[0].words[1].text, "world");
        assert!(!aligned.is_empty());
    }

    #[test]
    fn empty_arrays_are_empty() {
        let json = serde_json::json!({
            "aligned_words": [], "aligned_lyrics": [], "hoot_cer": 1.0, "is_streamed": false
        });
        let aligned = AlignedLyrics::from_json(&json);
        assert!(aligned.is_empty());
        assert_eq!(aligned.plain_text(), "");
        assert_eq!(aligned.lrc_body(), "");
        assert!(aligned.sylt_entries().is_empty());
    }

    #[test]
    fn missing_keys_map_to_empty() {
        assert!(AlignedLyrics::from_json(&serde_json::json!({})).is_empty());
        assert!(AlignedLyrics::from_json(&Value::Null).is_empty());
        assert!(AlignedLyrics::from_bytes(b"not json").is_empty());
    }

    #[test]
    fn captures_waveform_hoot_cer_and_is_streamed_absent_safe() {
        // The v2 body carries a waveform envelope, an alignment-error metric, and
        // a streaming flag alongside the words and lines; all are additive
        // metadata captured verbatim.
        let json = serde_json::json!({
            "aligned_words": [],
            "aligned_lyrics": [],
            "waveform_data": [0.00044, 0.0, 0.00014, 0.0008, 0.00146],
            "hoot_cer": 0.22907083716651333_f64,
            "is_streamed": false
        });
        let aligned = AlignedLyrics::from_json(&json);
        assert_eq!(aligned.waveform_data.len(), 5);
        assert!((aligned.waveform_data[3] - 0.0008).abs() < 1e-9);
        assert!(
            aligned
                .hoot_cer
                .is_some_and(|cer| (cer - 0.229_070_837).abs() < 1e-6)
        );
        assert_eq!(aligned.is_streamed, Some(false));
        // They are metadata, not lyric content: an otherwise-empty body is still
        // "no synced lyrics", so no synced artefact is written.
        assert!(aligned.is_empty());

        // Absent: the extras degrade to empty/None, never a panic.
        let bare = AlignedLyrics::from_json(&serde_json::json!({}));
        assert!(bare.waveform_data.is_empty());
        assert_eq!(bare.hoot_cer, None);
        assert_eq!(bare.is_streamed, None);
        // Wrong-typed values are ignored the same way rather than erroring.
        let odd = AlignedLyrics::from_json(&serde_json::json!({
            "waveform_data": "nope", "hoot_cer": "high", "is_streamed": 1
        }));
        assert!(odd.waveform_data.is_empty());
        assert_eq!(odd.hoot_cer, None);
        assert_eq!(odd.is_streamed, None);
    }

    #[test]
    fn lrc_body_has_line_level_stamps() {
        let aligned = AlignedLyrics::from_json(&sample_json());
        let body = aligned.lrc_body();
        let expected = "[00:00.50]Hello world\n\
             [01:00.00][Chorus]\n\
             [01:01.20]again\n";
        assert_eq!(body, expected);
    }

    #[test]
    fn plain_text_joins_line_text() {
        let aligned = AlignedLyrics::from_json(&sample_json());
        assert_eq!(aligned.plain_text(), "Hello world\n[Chorus]\nagain");
    }

    #[test]
    fn sylt_entries_are_word_level_with_line_breaks() {
        let aligned = AlignedLyrics::from_json(&sample_json());
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
