//! Feed request body and page decoding (`POST /api/feed/v3`).

use serde_json::Value;

use crate::consts::FEED_PAGE_SIZE;
use crate::error::{Error, Result};
use crate::is_downloadable;
use crate::model::Clip;

use super::map_clip;

/// Build the JSON body for a `POST /api/feed/v3` page.
///
/// `filters.trashed` is the string `"False"` so the feed excludes trashed clips;
/// a `liked` walk adds `filters.liked = "True"` (v3 ignores an `is_liked` key).
/// The `cursor` is omitted on the first page and set to the previous page's
/// `next_cursor` thereafter.
pub(crate) fn feed_v3_body(liked: bool, cursor: Option<&str>) -> Vec<u8> {
    let mut filters = serde_json::Map::new();
    filters.insert("trashed".to_string(), Value::String("False".to_string()));
    if liked {
        filters.insert("liked".to_string(), Value::String("True".to_string()));
    }
    let mut body = serde_json::Map::new();
    body.insert("limit".to_string(), Value::from(FEED_PAGE_SIZE));
    body.insert("filters".to_string(), Value::Object(filters));
    if let Some(cursor) = cursor {
        body.insert("cursor".to_string(), Value::String(cursor.to_string()));
    }
    serde_json::to_vec(&Value::Object(body)).unwrap_or_default()
}

/// One parsed v3 feed page.
///
/// `has_more` is [`None`] when the key is missing or not a bool, so the caller
/// can refuse to treat an unrecognised page as a fully drained feed. An empty
/// `next_cursor` string maps to [`None`] so it is never re-sent as a cursor.
/// `any_filtered` is `true` when the raw `clips[]` array held more entries than
/// survived the downloadable and non-empty-id filters, so the caller can disarm
/// deletion authority for a listing that may have hidden a tracked clip.
pub(crate) struct FeedPage {
    pub(crate) clips: Vec<Clip>,
    pub(crate) has_more: Option<bool>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) any_filtered: bool,
}

/// Parse a v3 feed page into a [`FeedPage`].
pub(crate) fn parse_feed_v3(body: &[u8]) -> Result<FeedPage> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid feed JSON: {err}")))?;
    let Some(object) = data.as_object() else {
        return Ok(FeedPage {
            clips: Vec::new(),
            has_more: None,
            next_cursor: None,
            any_filtered: false,
        });
    };
    let raw = object.get("clips").and_then(Value::as_array);
    let raw_len = raw.map(|clips| clips.len()).unwrap_or(0);
    let clips: Vec<Clip> = raw
        .map(|raw| {
            raw.iter()
                .map(map_clip)
                .filter(is_downloadable)
                .filter(|clip| !clip.id.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // A member the feed still lists may have flipped off `complete` (or into an
    // excluded type/task) since it was downloaded, or arrived with a corrupted
    // (empty) id; dropping it silently here would make a tracked clip look
    // absent and delete its master. Surface any such loss so the caller can
    // refuse deletion authority for this listing, matching the playlist path's
    // empty-id and filter guards.
    let any_filtered = clips.len() < raw_len;
    let has_more = object.get("has_more").and_then(Value::as_bool);
    let next_cursor = object
        .get("next_cursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_string);
    Ok(FeedPage {
        clips,
        has_more,
        next_cursor,
        any_filtered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_body() -> String {
        serde_json::json!({
            "has_more": false,
            "clips": [
                {
                    "id": "a", "title": "Song A", "status": "complete",
                    "audio_url": "https://cdn1.suno.ai/a.mp3",
                    "metadata": {"tags": "rock", "duration": 120.5, "type": "gen"}
                },
                {"id": "b", "title": "Infill", "status": "complete", "metadata": {"task": "infill"}},
                {"id": "c", "title": "Streaming", "status": "streaming", "metadata": {}},
                {
                    "id": "d", "title": "Context", "status": "complete",
                    "metadata": {"type": "rendered_context_window"}
                }
            ]
        })
        .to_string()
    }

    /// One real anonymised `POST /api/feed/v3` page: a single downloadable clip
    /// carrying `media_urls`, `user_id`, `batch_index`, cdn2 artwork, and a
    /// pagination envelope with `has_more`/`next_cursor`.
    const FEED_V3_PAGE: &str = r#"{
      "clips": [
        {
          "status": "complete",
          "title": "Track 31",
          "id": "00000000-0000-4000-8000-000000000076",
          "entity_type": "song_schema",
          "video_url": "",
          "audio_url": "https://cdn1.suno.ai/00000000-0000-4000-8000-000000000076.mp3",
          "media_urls": [
            {
              "url": "https://media.cloudfront.net/1/clip/00000000-0000-4000-8000-000000000076.m4a",
              "content_type": "m4a-opus",
              "delivery": "progressive",
              "encoding": "1.0.0"
            },
            {
              "url": "https://cdn1.suno.ai/00000000-0000-4000-8000-000000000076.mp3",
              "content_type": "mp3",
              "delivery": "progressive"
            }
          ],
          "image_url": "https://cdn2.suno.ai/image_00000000-0000-4000-8000-000000000076.jpeg",
          "image_large_url": "https://cdn2.suno.ai/image_large_00000000-0000-4000-8000-000000000076.jpeg",
          "major_model_version": "v4.5",
          "model_name": "chirp-ahi",
          "metadata": {
            "tags": "",
            "type": "gen",
            "duration": 272.0,
            "task": "gen_stem",
            "has_stem": false
          },
          "is_liked": false,
          "user_id": "00000000-0000-4000-8000-000000000019",
          "display_name": "Example Artist 4",
          "handle": "example-artist-1",
          "is_trashed": false,
          "is_hidden": false,
          "created_at": "2026-07-03T13:15:10.635Z",
          "is_public": false,
          "explicit": false,
          "batch_index": 23,
          "clip_roots": {
            "clips": [
              {
                "id": "00000000-0000-4000-8000-000000000028",
                "title": "Track 7",
                "image_url": "https://cdn2.suno.ai/image_00000000-0000-4000-8000-000000000028.jpeg",
                "is_public": false,
                "user_display_name": "Example Artist 4",
                "user_handle": "example-artist-1",
                "user_avatar_image_url": "https://cdn1.suno.ai/avatar.jpg"
              }
            ],
            "clip_attribution_type": "remix"
          }
        }
      ],
      "has_more": true,
      "next_cursor": "cursor-token"
    }"#;

    #[test]
    fn parse_feed_v3_filters_and_reads_pagination() {
        let page = parse_feed_v3(feed_body().as_bytes()).unwrap();
        assert_eq!(page.has_more, Some(false));
        assert_eq!(page.next_cursor, None);
        assert_eq!(page.clips.len(), 1);
        assert_eq!(page.clips[0].id, "a");
        assert_eq!(page.clips[0].tags, "rock");
        assert!((page.clips[0].duration - 120.5).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_feed_v3_flags_a_dropped_clip_as_filtered() {
        // feed_body() lists four clips but only one survives is_downloadable, so
        // a tracked clip that flipped off `complete` would be hidden here; the
        // flag warns the caller to disarm deletion.
        let page = parse_feed_v3(feed_body().as_bytes()).unwrap();
        assert_eq!(page.clips.len(), 1);
        assert!(page.any_filtered);

        // A page whose every clip is downloadable loses nothing.
        let clean = serde_json::json!({
            "has_more": false,
            "clips": [{"id": "a", "status": "complete", "metadata": {"type": "gen"}}]
        })
        .to_string();
        let page = parse_feed_v3(clean.as_bytes()).unwrap();
        assert_eq!(page.clips.len(), 1);
        assert!(!page.any_filtered);

        // A complete clip with a corrupted (empty) id is dropped and counted as
        // loss, so a tracked clip cannot be hidden behind an id-less entry
        // (parity with the playlist path).
        let empty_id = serde_json::json!({
            "has_more": false,
            "clips": [
                {"id": "kept", "status": "complete", "metadata": {"type": "gen"}},
                {"id": "", "status": "complete", "metadata": {"type": "gen"}}
            ]
        })
        .to_string();
        let page = parse_feed_v3(empty_id.as_bytes()).unwrap();
        assert_eq!(page.clips.len(), 1);
        assert_eq!(page.clips[0].id, "kept");
        assert!(page.any_filtered);
    }

    #[test]
    fn parse_feed_v3_page_maps_real_body_and_pagination() {
        let FeedPage {
            clips,
            has_more,
            next_cursor,
            ..
        } = parse_feed_v3(FEED_V3_PAGE.as_bytes()).unwrap();
        assert_eq!(has_more, Some(true));
        assert_eq!(next_cursor.as_deref(), Some("cursor-token"));
        // The single gen_stem clip is complete and passes is_downloadable.
        assert_eq!(clips.len(), 1);
        let clip = &clips[0];
        assert_eq!(clip.id, "00000000-0000-4000-8000-000000000076");
        assert_eq!(clip.title, "Track 31");
        assert_eq!(clip.model_name, "chirp-ahi");
        assert_eq!(clip.major_model_version, "v4.5");
        assert_eq!(clip.user_id, "00000000-0000-4000-8000-000000000019");
        assert_eq!(clip.batch_index, Some(23));
        // The cdn2 artwork host is rewritten to cdn1.
        assert_eq!(
            clip.image_url,
            "https://cdn1.suno.ai/image_00000000-0000-4000-8000-000000000076.jpeg"
        );
        assert!(clip.image_large_url.starts_with("https://cdn1.suno.ai/"));
        // media_urls carries both assets; mp3_url prefers the listed mp3.
        assert_eq!(clip.media_urls.len(), 2);
        assert_eq!(clip.media_urls[0].content_type, "m4a-opus");
        assert_eq!(
            clip.mp3_url(),
            "https://cdn1.suno.ai/00000000-0000-4000-8000-000000000076.mp3"
        );
        // A feed clip carries the same nested clip_roots shape as /api/clip/{id}.
        assert_eq!(clip.clip_attribution_type, "remix");
        assert_eq!(clip.clip_roots.len(), 1);
        assert_eq!(
            clip.clip_roots[0].id,
            "00000000-0000-4000-8000-000000000028"
        );
        assert_eq!(clip.clip_roots[0].handle, "example-artist-1");
    }

    #[test]
    fn parse_feed_v3_page_survives_stripped_optional_fields() {
        // A clip with explicit/ownership/clip_roots/media_urls all stripped still
        // parses with sane defaults (the 490/458-of-500 optionality reality).
        let stripped = serde_json::json!({
            "clips": [{
                "id": "bare", "title": "Bare", "status": "complete",
                "metadata": {"type": "gen"}
            }],
            "has_more": false
        })
        .to_string();
        let FeedPage {
            clips,
            has_more,
            next_cursor,
            ..
        } = parse_feed_v3(stripped.as_bytes()).unwrap();
        assert_eq!(has_more, Some(false));
        assert_eq!(next_cursor, None);
        assert_eq!(clips.len(), 1);
        assert!(clips[0].media_urls.is_empty());
        assert_eq!(clips[0].user_id, "");
        assert_eq!(clips[0].batch_index, None);
    }

    #[test]
    fn feed_v3_body_carries_filters_and_optional_cursor() {
        let first: Value = serde_json::from_slice(&feed_v3_body(false, None)).unwrap();
        assert_eq!(first["filters"]["trashed"], "False");
        assert!(first.get("cursor").is_none());
        assert!(first["filters"].get("liked").is_none());

        let liked: Value = serde_json::from_slice(&feed_v3_body(true, Some("cur42"))).unwrap();
        assert_eq!(liked["filters"]["liked"], "True");
        assert_eq!(liked["cursor"], "cur42");
    }
}
