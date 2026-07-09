//! Clip decoding: the shared clip mapping used by the feed, playlist, clip,
//! and stem endpoints, plus the small field readers it needs.

use serde_json::Value;

use crate::model::{Clip, ClipRoot, HistoryEntry, MediaUrl, cdn_audio_url};

/// Parse a single-clip response body, accepting either a bare clip object or a
/// `{"clip": {...}}` wrapper. Returns `None` when no clip id is present.
pub(crate) fn parse_clip(body: &[u8]) -> Option<Clip> {
    let data: Value = serde_json::from_slice(body).ok()?;
    let raw = unwrap_clip(&data);
    let has_id = raw
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty());
    has_id.then(|| map_clip(raw))
}

/// Parse a `get_songs_by_ids` `{"clips":[…]}` body into clips with a non-empty
/// id. Returns `None` when the body is not valid JSON or lacks a `clips` array,
/// signalling the caller to fall back to per-id fetches. No downloadability
/// filter is applied: these are lineage ancestors, which may be artefacts.
pub(crate) fn parse_songs_batch(body: &[u8]) -> Option<Vec<Clip>> {
    let data: Value = serde_json::from_slice(body).ok()?;
    let clips = data.get("clips")?.as_array()?;
    Some(
        clips
            .iter()
            .map(map_clip)
            .filter(|clip| !clip.id.is_empty())
            .collect(),
    )
}

/// Unwrap a `{ "clip": {...} }` wrapper to the inner clip object, or return
/// `value` unchanged when it carries no object `clip` key (it is already bare).
pub(super) fn unwrap_clip(value: &Value) -> &Value {
    value
        .get("clip")
        .filter(|clip| clip.is_object())
        .unwrap_or(value)
}

/// The absent-metadata sentinel, borrowed when a clip object carries no
/// `metadata` block so the field readers below share one `&Value` instead of
/// cloning the whole subtree.
static NULL_METADATA: Value = Value::Null;

/// Build a [`Clip`] from one raw API clip object.
///
/// Clip-level fields and lineage live at the top level; content fields like
/// tags and duration live under `metadata`. Temporary `audiopipe` audio URLs
/// expire, so they are rewritten to the permanent CDN URL.
pub(crate) fn map_clip(raw: &Value) -> Clip {
    let metadata: &Value = raw.get("metadata").unwrap_or(&NULL_METADATA);
    let id = string(raw, "id");

    let audio_url = cdn_audio_url(&string(raw, "audio_url"), &id);

    let title = match raw.get("title") {
        Some(Value::String(title)) => title.clone(),
        _ => "Untitled".to_string(),
    };

    Clip {
        id,
        title,
        audio_url,
        media_urls: parse_media_urls(raw),
        image_url: cdn(raw, "image_url"),
        image_large_url: cdn(raw, "image_large_url"),
        video_url: cdn(raw, "video_url"),
        video_cover_url: cdn(raw, "video_cover_url"),
        tags: string(metadata, "tags"),
        duration: metadata
            .get("duration")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        play_count: raw.get("play_count").and_then(Value::as_u64).unwrap_or(0),
        status: raw
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        created_at: string(raw, "created_at"),
        display_name: string_or(raw, "display_name", "user_display_name"),
        handle: string_or(raw, "handle", "user_handle"),
        user_id: string(raw, "user_id"),
        batch_index: raw.get("batch_index").and_then(Value::as_i64),
        avatar_image_url: string_or(raw, "avatar_image_url", "user_avatar_image_url"),
        is_liked: bool_field(raw, "is_liked"),
        is_trashed: bool_field(raw, "is_trashed"),
        has_vocal: bool_field(metadata, "has_vocal"),
        has_stem: bool_field(metadata, "has_stem"),
        stem_from_id: string(metadata, "stem_from_id"),
        stem_task: string(metadata, "stem_task"),
        stem_type_id: int_tolerant(metadata, "stem_type_id"),
        stem_type_group_name: string(metadata, "stem_type_group_name"),
        clip_type: string(metadata, "type"),
        prompt: string(metadata, "prompt"),
        gpt_description_prompt: string(metadata, "gpt_description_prompt"),
        lyrics: string(raw, "lyrics"),
        model_name: string(raw, "model_name"),
        major_model_version: string(raw, "major_model_version"),
        edited_clip_id: string(metadata, "edited_clip_id"),
        task: string(metadata, "task"),
        is_remix: bool_field(metadata, "is_remix"),
        cover_clip_id: string(metadata, "cover_clip_id"),
        upsample_clip_id: string(metadata, "upsample_clip_id"),
        remaster_clip_id: string(metadata, "remaster_clip_id"),
        speed_clip_id: string(metadata, "speed_clip_id"),
        override_history_clip_id: string(metadata, "override_history_clip_id"),
        override_future_clip_id: string(metadata, "override_future_clip_id"),
        history: history_entries(metadata, "history"),
        concat_history: history_entries(metadata, "concat_history"),
        clip_roots: parse_clip_roots(raw),
        clip_attribution_type: raw
            .get("clip_roots")
            .map(|roots| string(roots, "clip_attribution_type"))
            .unwrap_or_default(),
    }
}

/// Read a string field, defaulting to empty when missing or not a string.
fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Read `primary`, falling back to `fallback` when it is missing or empty.
///
/// Suno exposes the owner's identity under bare keys on a feed clip
/// (`display_name`, `handle`) but under `user_`-prefixed keys on a
/// parent-shaped clip; this reads whichever shape is present.
fn string_or(value: &Value, primary: &str, fallback: &str) -> String {
    let first = string(value, primary);
    if first.is_empty() {
        string(value, fallback)
    } else {
        first
    }
}

/// Read a bool field, defaulting to `false` when missing or not a bool.
fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Read an integer field, tolerating the float form Suno's history uses (e.g.
/// `stem_type_id` as `91.0`). Returns `None` when the field is missing,
/// non-numeric, or a non-integral float.
fn int_tolerant(value: &Value, key: &str) -> Option<i64> {
    let field = value.get(key)?;
    field.as_i64().or_else(|| {
        field
            .as_f64()
            .filter(|number| number.fract() == 0.0)
            .map(|number| number as i64)
    })
}

/// Read a CDN URL field, rewriting the unreliable `cdn2` host to `cdn1`.
fn cdn(value: &Value, key: &str) -> String {
    let url = string(value, key);
    if url.contains("cdn2.suno.ai") {
        url.replace("cdn2.suno.ai", "cdn1.suno.ai")
    } else {
        url
    }
}

/// Read the nested `clip_roots.clips[]` array into [`ClipRoot`]s.
///
/// The roots are nested under a `clip_roots` object (`{clips[],
/// clip_attribution_type}`), NOT a top-level array, and each entry carries
/// `user_`-prefixed identity keys. A missing key or non-array yields an empty
/// `Vec`, so a clip without attribution roots degrades rather than fails.
fn parse_clip_roots(raw: &Value) -> Vec<ClipRoot> {
    let Some(Value::Array(items)) = raw.get("clip_roots").and_then(|roots| roots.get("clips"))
    else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| ClipRoot {
            id: string(item, "id"),
            title: string(item, "title"),
            image_url: cdn(item, "image_url"),
            is_public: bool_field(item, "is_public"),
            display_name: string(item, "user_display_name"),
            handle: string(item, "user_handle"),
            avatar_image_url: string(item, "user_avatar_image_url"),
        })
        .collect()
}

/// Read the top-level `media_urls` array into [`MediaUrl`]s.
///
/// A missing key or non-array yields an empty `Vec`, and each element defaults
/// its fields, so a reshaped or partial entry degrades rather than fails.
fn parse_media_urls(raw: &Value) -> Vec<MediaUrl> {
    let Some(Value::Array(items)) = raw.get("media_urls") else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| MediaUrl {
            url: string(item, "url"),
            content_type: string(item, "content_type"),
            delivery: string(item, "delivery"),
            encoding: string(item, "encoding"),
        })
        .collect()
}

/// Read `value[key]` as an array of history records into [`HistoryEntry`]s.
///
/// Each element is mapped verbatim: a bare JSON string becomes an entry with
/// only its `id` set, while an object supplies `id`, `infill`, `continue_at`,
/// `infill_start_s`, `infill_end_s`, and `infill_lyrics`. Anything else (a
/// missing key, a non-array, or an unexpected element type) yields an empty
/// `Vec` or a defaulted entry, so parsing never fails.
fn history_entries(value: &Value, key: &str) -> Vec<HistoryEntry> {
    let Some(Value::Array(items)) = value.get(key) else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(id) => HistoryEntry {
                id: id.clone(),
                ..HistoryEntry::default()
            },
            _ => HistoryEntry {
                id: string(item, "id"),
                infill: bool_field(item, "infill"),
                continue_at: item.get("continue_at").and_then(Value::as_f64),
                infill_start_s: item.get("infill_start_s").and_then(Value::as_f64),
                infill_end_s: item.get("infill_end_s").and_then(Value::as_f64),
                infill_lyrics: string(item, "infill_lyrics"),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audiopipe_url_is_rewritten_to_cdn() {
        let raw =
            serde_json::json!({"id": "x", "audio_url": "https://audiopipe.suno.ai/?item_id=x"});
        assert_eq!(map_clip(&raw).audio_url, "https://cdn1.suno.ai/x.mp3");
    }

    #[test]
    fn parse_clip_accepts_bare_and_wrapped_shapes() {
        let bare = serde_json::json!({"id": "z", "title": "Zed"}).to_string();
        assert_eq!(parse_clip(bare.as_bytes()).unwrap().id, "z");

        let wrapped = serde_json::json!({"clip": {"id": "w", "title": "Wai"}}).to_string();
        assert_eq!(parse_clip(wrapped.as_bytes()).unwrap().id, "w");

        let missing = serde_json::json!({"detail": "not found"}).to_string();
        assert!(parse_clip(missing.as_bytes()).is_none());
    }

    #[test]
    fn from_json_reads_media_urls_user_id_and_batch_index() {
        let raw = serde_json::json!({
            "id": "clip-1",
            "user_id": "owner-9",
            "batch_index": 23,
            "media_urls": [
                {
                    "url": "https://media/clip-1.m4a",
                    "content_type": "m4a-opus",
                    "delivery": "progressive",
                    "encoding": "1.0.0"
                },
                {
                    "url": "https://cdn1.suno.ai/clip-1.mp3",
                    "content_type": "mp3",
                    "delivery": "progressive"
                }
            ]
        });

        let clip = map_clip(&raw);

        assert_eq!(clip.user_id, "owner-9");
        assert_eq!(clip.batch_index, Some(23));
        assert_eq!(clip.media_urls.len(), 2);
        assert_eq!(clip.media_urls[0].content_type, "m4a-opus");
        assert_eq!(clip.media_urls[0].encoding, "1.0.0");
        // The mp3 entry carries no `encoding`, which must default to empty.
        assert_eq!(clip.media_urls[1].content_type, "mp3");
        assert_eq!(clip.media_urls[1].encoding, "");
        assert_eq!(clip.mp3_url(), "https://cdn1.suno.ai/clip-1.mp3");
    }

    #[test]
    fn from_json_defaults_media_urls_user_id_and_batch_index_when_absent() {
        let clip = map_clip(&serde_json::json!({"id": "clip-1"}));
        assert!(clip.media_urls.is_empty());
        assert_eq!(clip.user_id, "");
        assert_eq!(clip.batch_index, None);
        // A non-array media_urls degrades to empty, never a panic.
        let odd = map_clip(&serde_json::json!({"id": "x", "media_urls": "nope"}));
        assert!(odd.media_urls.is_empty());
    }

    #[test]
    fn from_json_parses_nested_clip_roots_and_owner_identity() {
        // The real /api/clip/{id} remix body: clip_roots is a NESTED object
        // ({clips[], clip_attribution_type}), the root carries user_-prefixed
        // identity keys, and the owner identity is top-level.
        let raw = serde_json::json!({
            "id": "00000000-0000-4000-8000-000000000017",
            "title": "Track 1",
            "user_id": "00000000-0000-4000-8000-000000000019",
            "display_name": "Example Artist 4",
            "handle": "example-artist-1",
            "avatar_image_url": "https://cdn1.suno.ai/avatar.jpg",
            "batch_index": 1,
            "clip_roots": {
                "clips": [
                    {
                        "id": "00000000-0000-4000-8000-000000000020",
                        "title": "Track 2",
                        "image_url": "https://cdn2.suno.ai/image_00000000-0000-4000-8000-000000000020.jpeg",
                        "is_public": false,
                        "user_display_name": "Example Artist 4",
                        "user_handle": "example-artist-1",
                        "user_avatar_image_url": "https://cdn1.suno.ai/avatar.jpg"
                    }
                ],
                "clip_attribution_type": "remix"
            }
        });
        let clip = map_clip(&raw);

        assert_eq!(clip.display_name, "Example Artist 4");
        assert_eq!(clip.handle, "example-artist-1");
        assert_eq!(clip.avatar_image_url, "https://cdn1.suno.ai/avatar.jpg");
        assert_eq!(clip.clip_attribution_type, "remix");
        assert_eq!(clip.clip_roots.len(), 1);
        let root = &clip.clip_roots[0];
        assert_eq!(root.id, "00000000-0000-4000-8000-000000000020");
        assert_eq!(root.title, "Track 2");
        assert!(!root.is_public);
        // The root's user_-prefixed identity keys map onto the flat fields.
        assert_eq!(root.display_name, "Example Artist 4");
        assert_eq!(root.handle, "example-artist-1");
        assert_eq!(root.avatar_image_url, "https://cdn1.suno.ai/avatar.jpg");
        // The cdn2 artwork host is rewritten to cdn1, as for the clip's own art.
        assert_eq!(
            root.image_url,
            "https://cdn1.suno.ai/image_00000000-0000-4000-8000-000000000020.jpeg"
        );
    }

    #[test]
    fn from_json_reads_user_prefixed_identity_on_a_parent_shape() {
        // The reduced parent shape carries only user_-prefixed identity keys.
        let raw = serde_json::json!({
            "id": "00000000-0000-4000-8000-000000000020",
            "title": "Track 2",
            "is_public": false,
            "user_display_name": "Example Artist 4",
            "user_handle": "example-artist-1",
            "user_avatar_image_url": "https://cdn1.suno.ai/avatar.jpg"
        });
        let clip = map_clip(&raw);
        assert_eq!(clip.display_name, "Example Artist 4");
        assert_eq!(clip.handle, "example-artist-1");
        assert_eq!(clip.avatar_image_url, "https://cdn1.suno.ai/avatar.jpg");
    }

    #[test]
    fn from_json_prefers_bare_identity_over_user_prefixed() {
        // When both shapes are present, the bare (feed) keys win.
        let raw = serde_json::json!({
            "id": "x",
            "display_name": "Bare Name",
            "user_display_name": "Prefixed Name",
            "handle": "bare-handle",
            "user_handle": "prefixed-handle"
        });
        let clip = map_clip(&raw);
        assert_eq!(clip.display_name, "Bare Name");
        assert_eq!(clip.handle, "bare-handle");
    }

    #[test]
    fn from_json_defaults_clip_roots_when_absent_or_malformed() {
        // Absent clip_roots -> empty, attribution type empty.
        let none = map_clip(&serde_json::json!({"id": "x"}));
        assert!(none.clip_roots.is_empty());
        assert_eq!(none.clip_attribution_type, "");

        // clip_roots present but `clips` missing or non-array -> empty, no panic.
        let no_clips = map_clip(&serde_json::json!({
            "id": "x",
            "clip_roots": {"clip_attribution_type": "remix"}
        }));
        assert!(no_clips.clip_roots.is_empty());
        assert_eq!(no_clips.clip_attribution_type, "remix");

        let odd = map_clip(&serde_json::json!({
            "id": "x",
            "clip_roots": {"clips": "nope"}
        }));
        assert!(odd.clip_roots.is_empty());

        // A top-level (non-object) clip_roots is ignored, never a panic.
        let array_shape = map_clip(&serde_json::json!({
            "id": "x",
            "clip_roots": [{"id": "r"}]
        }));
        assert!(array_shape.clip_roots.is_empty());
        assert_eq!(array_shape.clip_attribution_type, "");
    }

    #[test]
    fn from_json_reads_multiple_clip_roots_in_order() {
        let raw = serde_json::json!({
            "id": "x",
            "clip_roots": {
                "clips": [
                    {"id": "root-a", "title": "A"},
                    {"id": "root-b", "title": "B"}
                ],
                "clip_attribution_type": "remix"
            }
        });
        let clip = map_clip(&raw);
        assert_eq!(clip.clip_roots.len(), 2);
        assert_eq!(clip.clip_roots[0].id, "root-a");
        assert_eq!(clip.clip_roots[1].id, "root-b");
    }

    #[test]
    fn from_json_parses_all_lineage_metadata_fields() {
        let raw = serde_json::json!({
            "id": "self",
            "title": "Lineage",
            "is_trashed": true,
            "metadata": {
                "task": "extend",
                "is_remix": true,
                "cover_clip_id": "cover-1",
                "upsample_clip_id": "upsample-2",
                "remaster_clip_id": "remaster-3",
                "speed_clip_id": "speed-4",
                "override_history_clip_id": "ovh-5",
                "override_future_clip_id": "ovf-6",
                "history": [
                    {
                        "infill": false,
                        "id": "0a3c311a-hist",
                        "source": "ios",
                        "type": "gen",
                        "continue_at": 115.35
                    },
                    {
                        "infill": true,
                        "id": "infill-hist",
                        "source": "web",
                        "type": "gen",
                        "infill_start_s": 12.0,
                        "infill_end_s": 28.5,
                        "infill_lyrics": "new words here"
                    }
                ],
                "concat_history": [
                    {"infill": false, "id": "122d0d15-base", "continue_at": 131.5},
                    {"id": "cf7cb30f-part"}
                ]
            }
        });

        let clip = map_clip(&raw);

        assert_eq!(clip.task, "extend");
        assert!(clip.is_remix);
        assert!(clip.is_trashed);
        assert_eq!(clip.cover_clip_id, "cover-1");
        assert_eq!(clip.upsample_clip_id, "upsample-2");
        assert_eq!(clip.remaster_clip_id, "remaster-3");
        assert_eq!(clip.speed_clip_id, "speed-4");
        assert_eq!(clip.override_history_clip_id, "ovh-5");
        assert_eq!(clip.override_future_clip_id, "ovf-6");

        assert_eq!(
            clip.history,
            vec![
                HistoryEntry {
                    id: "0a3c311a-hist".to_owned(),
                    infill: false,
                    continue_at: Some(115.35),
                    ..Default::default()
                },
                HistoryEntry {
                    id: "infill-hist".to_owned(),
                    infill: true,
                    infill_start_s: Some(12.0),
                    infill_end_s: Some(28.5),
                    infill_lyrics: "new words here".to_owned(),
                    ..Default::default()
                },
            ]
        );

        assert_eq!(
            clip.concat_history,
            vec![
                HistoryEntry {
                    id: "122d0d15-base".to_owned(),
                    continue_at: Some(131.5),
                    ..Default::default()
                },
                HistoryEntry {
                    id: "cf7cb30f-part".to_owned(),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn bare_string_history_element_parses_to_id_only_entry() {
        let raw = serde_json::json!({
            "id": "self",
            "metadata": {"history": ["m_bare-id-verbatim"]}
        });

        let clip = map_clip(&raw);

        assert_eq!(
            clip.history,
            vec![HistoryEntry {
                id: "m_bare-id-verbatim".to_owned(),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn play_count_parses_top_level_and_defaults_to_zero() {
        let with_count = serde_json::json!({"id": "x", "play_count": 4242});
        assert_eq!(map_clip(&with_count).play_count, 4242);
        // Absent or non-integer play_count falls back to zero.
        assert_eq!(map_clip(&serde_json::json!({"id": "x"})).play_count, 0);
        assert_eq!(
            map_clip(&serde_json::json!({"id": "x", "play_count": null})).play_count,
            0
        );
    }

    #[test]
    fn has_stem_parses_from_metadata_and_defaults_to_false() {
        // Present and true in metadata.
        let with_stem = serde_json::json!({"id": "x", "metadata": {"has_stem": true}});
        assert!(map_clip(&with_stem).has_stem);
        // Absent, null, or non-bool metadata.has_stem defaults to false, so a
        // clip is never mistaken for a stem source without an explicit true.
        assert!(!map_clip(&serde_json::json!({"id": "x"})).has_stem);
        assert!(
            !map_clip(&serde_json::json!({"id": "x", "metadata": {"has_stem": null}})).has_stem
        );
        assert!(
            !map_clip(&serde_json::json!({"id": "x", "metadata": {"has_stem": false}})).has_stem
        );
    }

    #[test]
    fn stem_lineage_quartet_parses_from_metadata_float_tolerant() {
        // A stem child on an ordinary feed clip: the quartet is under metadata,
        // and the history form of stem_type_id is a float (91.0).
        let raw = serde_json::json!({
            "id": "stem-child",
            "metadata": {
                "has_stem": false,
                "stem_from_id": "source-074",
                "stem_task": "twelve",
                "stem_type_id": 91.0,
                "stem_type_group_name": "Backing_Vocals"
            }
        });
        let clip = map_clip(&raw);
        assert_eq!(clip.stem_from_id, "source-074");
        assert_eq!(clip.stem_task, "twelve");
        assert_eq!(clip.stem_type_id, Some(91));
        assert_eq!(clip.stem_type_group_name, "Backing_Vocals");
        // The quartet and has_stem are read from the same metadata block: a stem
        // child carries the quartet yet is not itself a stem source.
        assert!(!clip.has_stem);

        // The plain integer form maps identically.
        let as_int = serde_json::json!({"id": "x", "metadata": {"stem_type_id": 91}});
        assert_eq!(map_clip(&as_int).stem_type_id, Some(91));

        // Absent, null, non-integral, or non-numeric stem_type_id is None, and
        // the string members default to empty, so a non-stem clip degrades
        // cleanly rather than fabricating a separation id.
        let bare = map_clip(&serde_json::json!({"id": "x"}));
        assert_eq!(bare.stem_type_id, None);
        assert_eq!(bare.stem_from_id, "");
        assert_eq!(bare.stem_task, "");
        assert_eq!(bare.stem_type_group_name, "");
        for odd in [
            serde_json::json!({"id": "x", "metadata": {"stem_type_id": null}}),
            serde_json::json!({"id": "x", "metadata": {"stem_type_id": 91.5}}),
            serde_json::json!({"id": "x", "metadata": {"stem_type_id": "91"}}),
        ] {
            assert_eq!(map_clip(&odd).stem_type_id, None);
        }
    }

    #[test]
    fn absent_or_null_lineage_metadata_defaults_to_empty() {
        let raw = serde_json::json!({
            "id": "self",
            "metadata": {
                "cover_clip_id": null,
                "is_remix": null,
                "history": null
            }
        });

        let clip = map_clip(&raw);

        assert_eq!(clip.task, "");
        assert!(!clip.is_remix);
        assert!(!clip.is_trashed);
        assert_eq!(clip.cover_clip_id, "");
        assert_eq!(clip.upsample_clip_id, "");
        assert_eq!(clip.remaster_clip_id, "");
        assert_eq!(clip.speed_clip_id, "");
        assert_eq!(clip.override_history_clip_id, "");
        assert_eq!(clip.override_future_clip_id, "");
        assert!(clip.history.is_empty());
        assert!(clip.concat_history.is_empty());
    }
}
