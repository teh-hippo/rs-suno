//! The [`Clip`] domain model and its mapping from the Suno API JSON shape.

use serde_json::Value;

use crate::consts::CDN_BASE_URL;

/// One finished Suno track, flattened from the API's nested response shape.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Clip {
    pub id: String,
    pub title: String,
    pub audio_url: String,
    /// Every audio asset Suno lists for the clip (an `mp3` plus, usually, an
    /// `m4a-opus`). Empty when the API omits `media_urls`, so a clip with no
    /// listed assets falls back to `audio_url` (then synthesis) exactly as
    /// before. The `mp3` entry is the authoritative, non-expiring source.
    pub media_urls: Vec<MediaUrl>,
    pub image_url: String,
    pub image_large_url: String,
    pub video_url: String,
    pub video_cover_url: String,
    pub tags: String,
    pub duration: f64,
    pub play_count: u64,
    pub status: String,
    pub created_at: String,
    pub display_name: String,
    pub handle: String,
    /// The clip owner's account id (top-level `user_id`). Feeds the
    /// foreign-owner attribution check and cross-account dedup; empty when the
    /// API omits it.
    pub user_id: String,
    /// Index within a generation batch (paired gens), for sibling
    /// disambiguation in naming and dedup. `None` when `batch_index` is absent.
    pub batch_index: Option<i64>,
    /// The clip owner's avatar image URL (`avatar_image_url`, or the
    /// `user_`-prefixed form on a parent-shaped clip). Empty when absent.
    pub avatar_image_url: String,
    pub is_liked: bool,
    pub is_trashed: bool,
    pub has_vocal: bool,
    /// Whether Suno reports this clip already has separated stems, from
    /// `metadata.has_stem`. The stems mirror uses it as a precondition: a clip
    /// whose `has_stem` is false or absent is never queried for stems.
    pub has_stem: bool,
    /// `metadata.stem_from_id`: the clip this one was separated from, when it is
    /// a stem child. Empty when absent. Structured stem lineage, carried on an
    /// ordinary feed clip independently of the `/stems` listing.
    pub stem_from_id: String,
    /// `metadata.stem_task`: the separation-run id grouping one set of stems.
    /// Empty when absent.
    pub stem_task: String,
    /// `metadata.stem_type_id`: the numeric separation-type id. Tolerates both
    /// the integer and the float (`91.0`) forms Suno has used; `None` when
    /// absent or non-numeric.
    pub stem_type_id: Option<i64>,
    /// `metadata.stem_type_group_name`: the canonical stem group in underscore
    /// form (e.g. `Backing_Vocals`). Empty when absent. Preferred, normalised,
    /// over a title parenthetical as the stem label.
    pub stem_type_group_name: String,
    pub clip_type: String,
    pub prompt: String,
    pub gpt_description_prompt: String,
    pub lyrics: String,
    pub model_name: String,
    pub major_model_version: String,
    pub edited_clip_id: String,
    pub task: String,
    pub is_remix: bool,
    pub cover_clip_id: String,
    pub upsample_clip_id: String,
    pub remaster_clip_id: String,
    pub speed_clip_id: String,
    pub override_history_clip_id: String,
    pub override_future_clip_id: String,
    pub history: Vec<HistoryEntry>,
    pub concat_history: Vec<HistoryEntry>,
    /// The remix/attribution origins Suno lists under the nested `clip_roots`
    /// object (`clip_roots.clips[]`). Empty when the key is absent. These feed
    /// attribution edges and a same-owner gap-fill seed only; they are never
    /// read by structural root resolution.
    pub clip_roots: Vec<ClipRoot>,
    /// The attribution kind for `clip_roots` (`clip_roots.clip_attribution_type`,
    /// e.g. `"remix"`). Open string, empty when absent.
    pub clip_attribution_type: String,
}

/// One remix/attribution origin from a clip's nested `clip_roots.clips[]` list.
///
/// Informational lineage the API exposes directly on the clip: the clip was
/// derived from this root. Identity keys are `user_`-prefixed here. Every field
/// defaults to empty/false when absent, so a reshaped or partial entry degrades
/// rather than fails.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClipRoot {
    pub id: String,
    pub title: String,
    pub image_url: String,
    pub is_public: bool,
    pub display_name: String,
    pub handle: String,
    pub avatar_image_url: String,
}

/// One audio asset from a clip's top-level `media_urls` list.
///
/// Suno lists each downloadable rendition (an `mp3`, and usually an
/// `m4a-opus`) with its `content_type`, `delivery` mode, and an optional
/// `encoding` version (only the m4a-opus carries one). Every field defaults to
/// empty when absent, so a reshaped or partial entry degrades rather than
/// fails.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaUrl {
    pub url: String,
    pub content_type: String,
    pub delivery: String,
    pub encoding: String,
}

/// One entry in a clip's `history` or `concat_history`, mirroring the API's
/// per-segment lineage record. Ids are stored verbatim (any `m_` prefix is left
/// for the resolver to strip).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryEntry {
    pub id: String,
    pub infill: bool,
    pub continue_at: Option<f64>,
    pub infill_start_s: Option<f64>,
    pub infill_end_s: Option<f64>,
    pub infill_lyrics: String,
}

impl Clip {
    /// Build a [`Clip`] from one raw API clip object.
    ///
    /// Clip-level fields and lineage live at the top level; content fields like
    /// tags and duration live under `metadata`. Temporary `audiopipe` audio URLs
    /// expire, so they are rewritten to the permanent CDN URL.
    pub fn from_json(raw: &Value) -> Clip {
        let metadata = raw.get("metadata").cloned().unwrap_or(Value::Null);
        let id = string(raw, "id");

        let mut audio_url = string(raw, "audio_url");
        if audio_url.contains("audiopipe") && !id.is_empty() {
            audio_url = format!("{CDN_BASE_URL}/{id}.mp3");
        }

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
            tags: string(&metadata, "tags"),
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
            has_vocal: bool_field(&metadata, "has_vocal"),
            has_stem: bool_field(&metadata, "has_stem"),
            stem_from_id: string(&metadata, "stem_from_id"),
            stem_task: string(&metadata, "stem_task"),
            stem_type_id: int_tolerant(&metadata, "stem_type_id"),
            stem_type_group_name: string(&metadata, "stem_type_group_name"),
            clip_type: string(&metadata, "type"),
            prompt: string(&metadata, "prompt"),
            gpt_description_prompt: string(&metadata, "gpt_description_prompt"),
            lyrics: string(raw, "lyrics"),
            model_name: string(raw, "model_name"),
            major_model_version: string(raw, "major_model_version"),
            edited_clip_id: string(&metadata, "edited_clip_id"),
            task: string(&metadata, "task"),
            is_remix: bool_field(&metadata, "is_remix"),
            cover_clip_id: string(&metadata, "cover_clip_id"),
            upsample_clip_id: string(&metadata, "upsample_clip_id"),
            remaster_clip_id: string(&metadata, "remaster_clip_id"),
            speed_clip_id: string(&metadata, "speed_clip_id"),
            override_history_clip_id: string(&metadata, "override_history_clip_id"),
            override_future_clip_id: string(&metadata, "override_future_clip_id"),
            history: history_entries(&metadata, "history"),
            concat_history: history_entries(&metadata, "concat_history"),
            clip_roots: parse_clip_roots(raw),
            clip_attribution_type: raw
                .get("clip_roots")
                .map(|roots| string(roots, "clip_attribution_type"))
                .unwrap_or_default(),
        }
    }

    /// The MP3 source URL, in priority order: the API-listed `media_urls` `mp3`
    /// asset (authoritative and non-expiring), then the clip's `audio_url`, then
    /// the deterministic CDN URL synthesised from the id.
    ///
    /// When `media_urls` is absent the behaviour is unchanged: a present
    /// `audio_url` is returned verbatim, and an empty one synthesises the CDN
    /// URL.
    pub fn mp3_url(&self) -> String {
        if let Some(mp3) = self
            .media_urls
            .iter()
            .find(|media| media.content_type == "mp3" && !media.url.is_empty())
        {
            return mp3.url.clone();
        }
        if self.audio_url.is_empty() {
            format!("{CDN_BASE_URL}/{}.mp3", self.id)
        } else {
            self.audio_url.clone()
        }
    }

    /// Cover-art URLs in preference order (large image, image, video cover),
    /// dropping any that are empty.
    pub fn cover_candidates(&self) -> Vec<&str> {
        [
            self.image_large_url.as_str(),
            self.image_url.as_str(),
            self.video_cover_url.as_str(),
        ]
        .into_iter()
        .filter(|url| !url.is_empty())
        .collect()
    }

    /// The preferred cover-art URL, or `None` when the clip carries no art.
    pub fn selected_image_url(&self) -> Option<&str> {
        if !self.image_large_url.is_empty() {
            Some(self.image_large_url.as_str())
        } else if !self.image_url.is_empty() {
            Some(self.image_url.as_str())
        } else if !self.video_cover_url.is_empty() {
            Some(self.video_cover_url.as_str())
        } else {
            None
        }
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
    string(value, key).replace("cdn2.suno.ai", "cdn1.suno.ai")
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

    fn art_clip(image_large: &str, image: &str, video_cover: &str) -> Clip {
        Clip {
            image_large_url: image_large.to_owned(),
            image_url: image.to_owned(),
            video_cover_url: video_cover.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn mp3_url_uses_audio_url_or_synthesises_the_cdn_url() {
        let mut clip = Clip {
            id: "z".to_owned(),
            audio_url: "https://x/real.mp3".to_owned(),
            ..Default::default()
        };
        assert_eq!(clip.mp3_url(), "https://x/real.mp3");
        clip.audio_url = String::new();
        assert_eq!(clip.mp3_url(), "https://cdn1.suno.ai/z.mp3");
    }

    #[test]
    fn mp3_url_prefers_the_media_urls_mp3_then_audio_url_then_synthesis() {
        // The API-listed mp3 asset wins over audio_url.
        let clip = Clip {
            id: "z".to_owned(),
            audio_url: "https://x/real.mp3".to_owned(),
            media_urls: vec![
                MediaUrl {
                    url: "https://media/z.m4a".to_owned(),
                    content_type: "m4a-opus".to_owned(),
                    delivery: "progressive".to_owned(),
                    encoding: "1.0.0".to_owned(),
                },
                MediaUrl {
                    url: "https://cdn1.suno.ai/z.mp3".to_owned(),
                    content_type: "mp3".to_owned(),
                    delivery: "progressive".to_owned(),
                    encoding: String::new(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(clip.mp3_url(), "https://cdn1.suno.ai/z.mp3");

        // Absent media_urls falls back to audio_url unchanged (today's behaviour).
        let no_media = Clip {
            id: "z".to_owned(),
            audio_url: "https://x/real.mp3".to_owned(),
            ..Default::default()
        };
        assert_eq!(no_media.mp3_url(), "https://x/real.mp3");

        // A media_urls set with only a non-mp3 asset still falls back.
        let only_m4a = Clip {
            id: "z".to_owned(),
            audio_url: String::new(),
            media_urls: vec![MediaUrl {
                url: "https://media/z.m4a".to_owned(),
                content_type: "m4a-opus".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(only_m4a.mp3_url(), "https://cdn1.suno.ai/z.mp3");
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

        let clip = Clip::from_json(&raw);

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
        let clip = Clip::from_json(&serde_json::json!({"id": "clip-1"}));
        assert!(clip.media_urls.is_empty());
        assert_eq!(clip.user_id, "");
        assert_eq!(clip.batch_index, None);
        // A non-array media_urls degrades to empty, never a panic.
        let odd = Clip::from_json(&serde_json::json!({"id": "x", "media_urls": "nope"}));
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
        let clip = Clip::from_json(&raw);

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
        let clip = Clip::from_json(&raw);
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
        let clip = Clip::from_json(&raw);
        assert_eq!(clip.display_name, "Bare Name");
        assert_eq!(clip.handle, "bare-handle");
    }

    #[test]
    fn from_json_defaults_clip_roots_when_absent_or_malformed() {
        // Absent clip_roots -> empty, attribution type empty.
        let none = Clip::from_json(&serde_json::json!({"id": "x"}));
        assert!(none.clip_roots.is_empty());
        assert_eq!(none.clip_attribution_type, "");

        // clip_roots present but `clips` missing or non-array -> empty, no panic.
        let no_clips = Clip::from_json(&serde_json::json!({
            "id": "x",
            "clip_roots": {"clip_attribution_type": "remix"}
        }));
        assert!(no_clips.clip_roots.is_empty());
        assert_eq!(no_clips.clip_attribution_type, "remix");

        let odd = Clip::from_json(&serde_json::json!({
            "id": "x",
            "clip_roots": {"clips": "nope"}
        }));
        assert!(odd.clip_roots.is_empty());

        // A top-level (non-object) clip_roots is ignored, never a panic.
        let array_shape = Clip::from_json(&serde_json::json!({
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
        let clip = Clip::from_json(&raw);
        assert_eq!(clip.clip_roots.len(), 2);
        assert_eq!(clip.clip_roots[0].id, "root-a");
        assert_eq!(clip.clip_roots[1].id, "root-b");
    }

    #[test]
    fn cover_candidates_are_ordered_and_filtered() {
        let clip = art_clip("L", "", "V");
        assert_eq!(clip.cover_candidates(), vec!["L", "V"]);
    }

    #[test]
    fn selected_image_url_prefers_large_then_image_then_video() {
        assert_eq!(art_clip("L", "I", "V").selected_image_url(), Some("L"));
        assert_eq!(art_clip("", "I", "V").selected_image_url(), Some("I"));
        assert_eq!(art_clip("", "", "V").selected_image_url(), Some("V"));
        assert_eq!(art_clip("", "", "").selected_image_url(), None);
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

        let clip = Clip::from_json(&raw);

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

        let clip = Clip::from_json(&raw);

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
        assert_eq!(Clip::from_json(&with_count).play_count, 4242);
        // Absent or non-integer play_count falls back to zero.
        assert_eq!(
            Clip::from_json(&serde_json::json!({"id": "x"})).play_count,
            0
        );
        assert_eq!(
            Clip::from_json(&serde_json::json!({"id": "x", "play_count": null})).play_count,
            0
        );
    }

    #[test]
    fn has_stem_parses_from_metadata_and_defaults_to_false() {
        // Present and true in metadata.
        let with_stem = serde_json::json!({"id": "x", "metadata": {"has_stem": true}});
        assert!(Clip::from_json(&with_stem).has_stem);
        // Absent, null, or non-bool metadata.has_stem defaults to false, so a
        // clip is never mistaken for a stem source without an explicit true.
        assert!(!Clip::from_json(&serde_json::json!({"id": "x"})).has_stem);
        assert!(
            !Clip::from_json(&serde_json::json!({"id": "x", "metadata": {"has_stem": null}}))
                .has_stem
        );
        assert!(
            !Clip::from_json(&serde_json::json!({"id": "x", "metadata": {"has_stem": false}}))
                .has_stem
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
        let clip = Clip::from_json(&raw);
        assert_eq!(clip.stem_from_id, "source-074");
        assert_eq!(clip.stem_task, "twelve");
        assert_eq!(clip.stem_type_id, Some(91));
        assert_eq!(clip.stem_type_group_name, "Backing_Vocals");
        // The quartet and has_stem are read from the same metadata block: a stem
        // child carries the quartet yet is not itself a stem source.
        assert!(!clip.has_stem);

        // The plain integer form maps identically.
        let as_int = serde_json::json!({"id": "x", "metadata": {"stem_type_id": 91}});
        assert_eq!(Clip::from_json(&as_int).stem_type_id, Some(91));

        // Absent, null, non-integral, or non-numeric stem_type_id is None, and
        // the string members default to empty, so a non-stem clip degrades
        // cleanly rather than fabricating a separation id.
        let bare = Clip::from_json(&serde_json::json!({"id": "x"}));
        assert_eq!(bare.stem_type_id, None);
        assert_eq!(bare.stem_from_id, "");
        assert_eq!(bare.stem_task, "");
        assert_eq!(bare.stem_type_group_name, "");
        for odd in [
            serde_json::json!({"id": "x", "metadata": {"stem_type_id": null}}),
            serde_json::json!({"id": "x", "metadata": {"stem_type_id": 91.5}}),
            serde_json::json!({"id": "x", "metadata": {"stem_type_id": "91"}}),
        ] {
            assert_eq!(Clip::from_json(&odd).stem_type_id, None);
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

        let clip = Clip::from_json(&raw);

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
