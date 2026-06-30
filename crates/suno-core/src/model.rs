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
    pub status: String,
    pub created_at: String,
    pub display_name: String,
    pub handle: String,
    pub is_liked: bool,
    pub has_vocal: bool,
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
            status: raw
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            created_at: string(raw, "created_at"),
            display_name: string(raw, "display_name"),
            handle: string(raw, "handle"),
            is_liked: raw
                .get("is_liked")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            has_vocal: metadata
                .get("has_vocal")
                .and_then(Value::as_bool)
                .unwrap_or(false),
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

/// Read a CDN URL field, rewriting the unreliable `cdn2` host to `cdn1`.
fn cdn(value: &Value, key: &str) -> String {
    string(value, key).replace("cdn2.suno.ai", "cdn1.suno.ai")
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
}
