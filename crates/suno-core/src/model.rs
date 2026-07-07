//! The [`Clip`] domain model and its accessors; the JSON decode that builds a
//! [`Clip`] from the Suno API shape lives in [`wire`](crate::wire).

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
            return cdn_audio_url(&mp3.url, &self.id);
        }
        if self.audio_url.is_empty() {
            format!("{CDN_BASE_URL}/{}.mp3", self.id)
        } else {
            self.audio_url.clone()
        }
    }

    /// Static cover-art image URLs in preference order (large image, then
    /// image), dropping any that are empty.
    ///
    /// The `video_cover_url` preview is deliberately excluded: it is an MP4, not
    /// an embeddable still image, so embedding it as a JPEG/WebP picture would
    /// corrupt the artwork. A clip with only a video preview therefore yields no
    /// embeddable cover (the animated cover is handled separately, by embedding a
    /// transcoded WebP).
    pub fn cover_candidates(&self) -> Vec<&str> {
        [self.image_large_url.as_str(), self.image_url.as_str()]
            .into_iter()
            .filter(|url| !url.is_empty())
            .collect()
    }

    /// The preferred static cover-art image URL, or `None` when the clip carries
    /// no still image.
    ///
    /// Like [`cover_candidates`](Self::cover_candidates), the `video_cover_url`
    /// preview (an MP4) is deliberately excluded: this drives the static `.jpg`
    /// sidecars, the album `folder.jpg`, and the embedded-cover identity hash,
    /// none of which can use a video. A clip with only a video preview yields
    /// `None`.
    pub fn selected_image_url(&self) -> Option<&str> {
        if !self.image_large_url.is_empty() {
            Some(self.image_large_url.as_str())
        } else if !self.image_url.is_empty() {
            Some(self.image_url.as_str())
        } else {
            None
        }
    }
}

/// Rewrite an expiring `audiopipe` audio URL to the permanent CDN URL for `id`.
/// Any other URL, including an empty one, is returned unchanged, and an empty
/// `id` leaves the URL untouched because the CDN URL cannot be synthesised
/// without it. Shared by `audio_url` mapping and `mp3_url` so no single URL
/// source can leak an expiring link.
pub(crate) fn cdn_audio_url(url: &str, id: &str) -> String {
    if url.contains("audiopipe") && !id.is_empty() {
        format!("{CDN_BASE_URL}/{id}.mp3")
    } else {
        url.to_string()
    }
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
    fn mp3_url_rewrites_an_expiring_audiopipe_media_url() {
        // An audiopipe mp3 in media_urls expires, so mp3_url rewrites it to the
        // permanent CDN URL, matching how audio_url is rewritten at parse time.
        let expiring = Clip {
            id: "z".to_owned(),
            media_urls: vec![MediaUrl {
                url: "https://audiopipe.suno.ai/item?id=z".to_owned(),
                content_type: "mp3".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(expiring.mp3_url(), "https://cdn1.suno.ai/z.mp3");

        // A permanent (non-audiopipe) mp3 asset is returned verbatim.
        let permanent = Clip {
            id: "z".to_owned(),
            media_urls: vec![MediaUrl {
                url: "https://cdn1.suno.ai/z.mp3".to_owned(),
                content_type: "mp3".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(permanent.mp3_url(), "https://cdn1.suno.ai/z.mp3");
    }

    #[test]
    fn cover_candidates_are_static_images_ordered_and_filtered() {
        // The video preview (an MP4) is never an embeddable still, so it is
        // excluded; only the static image URLs remain, in preference order.
        assert_eq!(art_clip("L", "I", "V").cover_candidates(), vec!["L", "I"]);
        assert_eq!(art_clip("L", "", "V").cover_candidates(), vec!["L"]);
        assert!(art_clip("", "", "V").cover_candidates().is_empty());
    }

    #[test]
    fn selected_image_url_prefers_large_then_image_and_excludes_video() {
        assert_eq!(art_clip("L", "I", "V").selected_image_url(), Some("L"));
        assert_eq!(art_clip("", "I", "V").selected_image_url(), Some("I"));
        // A video-only clip has no still image to embed or write as a `.jpg`.
        assert_eq!(art_clip("", "", "V").selected_image_url(), None);
        assert_eq!(art_clip("", "", "").selected_image_url(), None);
    }
}
