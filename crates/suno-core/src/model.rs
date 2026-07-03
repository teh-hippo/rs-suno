//! The [`Clip`] domain model and its mapping from the Suno API JSON shape.

use serde_json::Value;

use crate::consts::CDN_BASE_URL;

/// One finished Suno track, flattened from the API's nested response shape.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Clip {
    pub id: String,
    pub title: String,
    pub audio_url: String,
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
    pub is_liked: bool,
    pub is_trashed: bool,
    pub has_vocal: bool,
    /// Whether Suno reports this clip already has separated stems, from
    /// `metadata.has_stem`. The stems mirror uses it as a precondition: a clip
    /// whose `has_stem` is false or absent is never queried for stems.
    pub has_stem: bool,
    pub clip_type: String,
    pub prompt: String,
    pub gpt_description_prompt: String,
    pub lyrics: String,
    pub model_name: String,
    pub major_model_version: String,
    pub album_title: String,
    pub root_ancestor_id: String,
    pub lineage_status: String,
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
            display_name: string(raw, "display_name"),
            handle: string(raw, "handle"),
            is_liked: bool_field(raw, "is_liked"),
            is_trashed: bool_field(raw, "is_trashed"),
            has_vocal: bool_field(&metadata, "has_vocal"),
            has_stem: bool_field(&metadata, "has_stem"),
            clip_type: string(&metadata, "type"),
            prompt: string(&metadata, "prompt"),
            gpt_description_prompt: string(&metadata, "gpt_description_prompt"),
            lyrics: string(raw, "lyrics"),
            model_name: string(raw, "model_name"),
            major_model_version: string(raw, "major_model_version"),
            album_title: string(raw, "album_title"),
            root_ancestor_id: string(raw, "root_ancestor_id"),
            lineage_status: string(raw, "lineage_status"),
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
        }
    }

    /// The MP3 source URL: the clip's `audio_url`, or the deterministic CDN URL
    /// when it is empty.
    pub fn mp3_url(&self) -> String {
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
        self.cover_candidates().into_iter().next()
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

/// Read a bool field, defaulting to `false` when missing or not a bool.
fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Read a CDN URL field, rewriting the unreliable `cdn2` host to `cdn1`.
fn cdn(value: &Value, key: &str) -> String {
    string(value, key).replace("cdn2.suno.ai", "cdn1.suno.ai")
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
