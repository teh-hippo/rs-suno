//! Stem page decoding (`/api/clip/{id}/stems*`).

use serde_json::Value;

use crate::model::{Stem, stem_label};

use super::map_clip;

/// Parse one page of the stems listing (`{"stems": [<clip>, ...]}`) into
/// [`Stem`]s.
///
/// Each stem is a full clip object, so it is mapped with [`map_clip`]:
/// the id is the stem clip id, the label is the trailing parenthetical of its
/// title, and the download URL is its public CDN MP3. Only stems carrying both a
/// non-empty id and URL are kept — a stem with no id cannot be WAV-rendered, and
/// one with no URL cannot be mirrored. Malformed JSON yields no stems (never a
/// panic), so a bad body is treated as an empty, non-authoritative page.
pub(crate) fn parse_stems_page(body: &[u8]) -> Vec<Stem> {
    let Ok(data) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let items = if let Some(array) = data.as_array() {
        array.as_slice()
    } else {
        data.get("stems")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    };
    items
        .iter()
        .map(parse_stem)
        .filter(|stem| !stem.id.is_empty() && !stem.url.is_empty())
        .collect()
}

/// Map one raw stem clip element to a [`Stem`]: its clip id, its stem label,
/// and its public CDN MP3 URL.
fn parse_stem(raw: &Value) -> Stem {
    let clip = map_clip(raw);
    Stem {
        id: clip.id.clone(),
        label: stem_label(&clip),
        url: clip.mp3_url(),
    }
}

/// Parse the stems page count from `GET /api/clip/{id}/stems/pages`
/// (`{"pages": N}`).
///
/// A missing, non-numeric, or negative `pages` reads as `0` (no stems), so a
/// malformed body is treated as indeterminate rather than guessing a count.
pub(crate) fn parse_stem_page_count(body: &[u8]) -> u32 {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|data| data.get("pages").and_then(Value::as_u64))
        .and_then(|pages| u32::try_from(pages).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stems page body: each stem is a full clip object whose title carries
    /// the label in a trailing parenthetical, as the live endpoint returns.
    fn stem_page(stems: &[(&str, &str, &str)]) -> String {
        let entries: Vec<Value> = stems
            .iter()
            .map(|(id, label, url)| {
                serde_json::json!({
                    "id": id,
                    "title": format!("My Song ({label})"),
                    "status": "complete",
                    "audio_url": url,
                })
            })
            .collect();
        serde_json::json!({ "stems": entries }).to_string()
    }

    #[test]
    fn parse_stems_page_maps_full_clips_and_skips_idless() {
        // A stem is a full clip: id, label from the title parenthetical, and the
        // public CDN MP3 url.
        let page = stem_page(&[("x", "Backing Vocals", "https://cdn1.suno.ai/x.mp3")]);
        let stems = parse_stems_page(page.as_bytes());
        assert_eq!(stems.len(), 1);
        assert_eq!(stems[0].id, "x");
        assert_eq!(stems[0].label, "Backing Vocals");
        assert_eq!(stems[0].url, "https://cdn1.suno.ai/x.mp3");
        // An entry with no id cannot be keyed or WAV-rendered and is dropped.
        let no_id = br#"{"stems": [{"title": "Ghost (Vocals)", "audio_url": "https://cdn1.suno.ai/g.mp3"}]}"#;
        assert!(parse_stems_page(no_id).is_empty());
        // A stem with an id but no audio_url still resolves a deterministic CDN
        // url from its id, so it remains downloadable.
        let no_url = br#"{"stems": [{"id": "y", "title": "Song (Bass)"}]}"#;
        let recovered = parse_stems_page(no_url);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].url, "https://cdn1.suno.ai/y.mp3");
        // Malformed JSON never panics; it yields no stems.
        assert!(parse_stems_page(b"not json").is_empty());
    }

    #[test]
    fn parse_stem_page_count_reads_pages_field() {
        assert_eq!(parse_stem_page_count(br#"{"pages": 12}"#), 12);
        assert_eq!(parse_stem_page_count(br#"{"pages": 0}"#), 0);
        // Missing, negative, or non-numeric pages read as 0 (indeterminate).
        assert_eq!(parse_stem_page_count(br#"{}"#), 0);
        assert_eq!(parse_stem_page_count(br#"{"pages": -1}"#), 0);
        assert_eq!(parse_stem_page_count(b"not json"), 0);
    }
}
