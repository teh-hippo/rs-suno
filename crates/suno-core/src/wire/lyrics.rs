//! Aligned-lyrics decoding (`/api/gen/{id}/aligned_lyrics/v2/`).

use serde_json::Value;

use crate::lyrics::{AlignedLine, AlignedLineWord, AlignedLyrics, AlignedWord};

/// Parse the `aligned_lyrics/v2` response bytes, or the empty value when the
/// body is not valid JSON (defensive: an odd body means "no synced lyrics").
pub(crate) fn parse_aligned_lyrics(body: &[u8]) -> AlignedLyrics {
    serde_json::from_slice::<Value>(body)
        .map(|value| map_aligned_lyrics(&value))
        .unwrap_or_default()
}

/// Map the `aligned_lyrics/v2` response body, tolerating missing keys.
///
/// A non-object body, or one whose arrays are missing, maps to the empty
/// value, so a malformed or instrumental response is simply "no synced
/// lyrics" rather than an error.
fn map_aligned_lyrics(raw: &Value) -> AlignedLyrics {
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
        let aligned = map_aligned_lyrics(&sample_json());
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
        let aligned = map_aligned_lyrics(&json);
        assert!(aligned.is_empty());
        assert_eq!(aligned.plain_text(), "");
        assert_eq!(aligned.lrc_body(), "");
        assert!(aligned.sylt_entries().is_empty());
    }

    #[test]
    fn missing_keys_map_to_empty() {
        assert!(map_aligned_lyrics(&serde_json::json!({})).is_empty());
        assert!(map_aligned_lyrics(&Value::Null).is_empty());
        assert!(parse_aligned_lyrics(b"not json").is_empty());
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
        let aligned = map_aligned_lyrics(&json);
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
        let bare = map_aligned_lyrics(&serde_json::json!({}));
        assert!(bare.waveform_data.is_empty());
        assert_eq!(bare.hoot_cer, None);
        assert_eq!(bare.is_streamed, None);
        // Wrong-typed values are ignored the same way rather than erroring.
        let odd = map_aligned_lyrics(&serde_json::json!({
            "waveform_data": "nope", "hoot_cer": "high", "is_streamed": 1
        }));
        assert!(odd.waveform_data.is_empty());
        assert_eq!(odd.hoot_cer, None);
        assert_eq!(odd.is_streamed, None);
    }
}
