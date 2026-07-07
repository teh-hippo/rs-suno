//! The single JSON-decode home: maps the Suno API's JSON shapes onto the
//! crate's domain types (feed, clip, playlist, stem, and billing).
//!
//! Transport (HTTP calls, retry, and the POST allow-list) stays in
//! [`client`](crate::client); this module is pure and performs no IO. It reads
//! [`model`](crate::model) for the [`Clip`] type and its `cdn_audio_url` helper
//! but nothing in `model` depends on it, so the decode-to-domain edge is
//! one-way.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::client::{BillingInfo, Playlist, Stem, stem_label};
use crate::consts::FEED_PAGE_SIZE;
use crate::error::{Error, Result};
use crate::is_downloadable;
use crate::model::{Clip, ClipRoot, HistoryEntry, MediaUrl, cdn_audio_url};

/// Build the JSON body for a `POST /api/feed/v3` page.
///
/// `filters.trashed` is the string `"False"` so the feed excludes trashed clips
/// exactly as the old v2 listing did; a `liked` walk adds `filters.liked =
/// "True"` (v3 ignores an `is_liked` key). The `cursor` is omitted on the first
/// page and set to the previous page's `next_cursor` thereafter.
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
/// deletion authority for a listing that may have hidden a tracked clip (#248).
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
                .map(Clip::from_json)
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
    // empty-id and filter guards (#248, sibling of #148).
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

/// Parse a `/api/playlist/me` page into playlists, dropping entries with no id.
pub(crate) fn parse_playlists(body: &[u8]) -> Result<Vec<Playlist>> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid playlist JSON: {err}")))?;
    Ok(data
        .get("playlists")
        .and_then(Value::as_array)
        .map(|raw| raw.iter().filter_map(parse_playlist_item).collect())
        .unwrap_or_default())
}

/// Map one raw `/api/playlist/me` entry, or `None` when it carries no id.
///
/// `num_total_results` is the playlist's member count; a missing name defaults
/// to `Untitled` (matching the clip mapping) so the file name is never empty.
fn parse_playlist_item(raw: &Value) -> Option<Playlist> {
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_string();
    let name = match raw.get("name") {
        Some(Value::String(name)) if !name.is_empty() => name.clone(),
        _ => "Untitled".to_string(),
    };
    let num_clips = raw
        .get("num_total_results")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Playlist {
        id,
        name,
        num_clips,
    })
}

/// Parse a `/api/playlist/{id}/` body into its ordered member clips plus a
/// completeness flag.
///
/// Each `playlist_clips[]` entry wraps the clip under `clip`; the wrapper is
/// unwrapped (falling back to the entry itself), order is preserved exactly, and
/// only clips with a non-empty id survive. No downloadability filter is applied:
/// a playlist may hold any clip, and members absent from the local library are
/// reconciled as comment lines by the caller, not dropped here. The scoped-sync
/// path applies [`is_downloadable`](crate::is_downloadable) itself when it fetches
/// members as download candidates.
///
/// The completeness flag is `true` only when the response's `num_total_results`
/// is present, equals the raw `playlist_clips[]` count, and no member was
/// dropped by the empty-id filter, i.e. the whole member set arrived intact on
/// this single page. It gates a Mirror playlist area's deletion authority (D5):
/// a short or paginated page, or one carrying a member with a missing/empty
/// clip id, cannot be authoritative for deletion, so it returns `false`.
pub(crate) fn parse_playlist_clips(body: &[u8]) -> Result<(Vec<Clip>, bool)> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid playlist JSON: {err}")))?;
    let raw = data.get("playlist_clips").and_then(Value::as_array);
    let raw_len = raw.map(|a| a.len()).unwrap_or(0);
    let clips: Vec<Clip> = raw
        .map(|raw| {
            raw.iter()
                .map(|entry| Clip::from_json(unwrap_clip(entry)))
                .filter(|clip| !clip.id.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // Completeness requires the reported total to be present and to match the
    // raw entry count (before the empty-id filter) AND no member to have been
    // dropped by that filter (`clips.len() == raw_len`). A missing or malformed
    // total, a short page, or a single dropped member (empty/missing clip id)
    // all fail safe toward "not authoritative", so a Mirror area can never
    // delete from a page whose whole member set was not seen intact.
    let complete = data
        .get("num_total_results")
        .and_then(Value::as_u64)
        .is_some_and(|total| raw_len as u64 == total && clips.len() == raw_len);
    Ok((clips, complete))
}

/// Parse a single-clip response body, accepting either a bare clip object or a
/// `{"clip": {...}}` wrapper. Returns `None` when no clip id is present.
pub(crate) fn parse_clip(body: &[u8]) -> Option<Clip> {
    let data: Value = serde_json::from_slice(body).ok()?;
    let raw = unwrap_clip(&data);
    let has_id = raw
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty());
    has_id.then(|| Clip::from_json(raw))
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
            .map(Clip::from_json)
            .filter(|clip| !clip.id.is_empty())
            .collect(),
    )
}

/// Parse one page of the stems listing (`{"stems": [<clip>, ...]}`) into
/// [`Stem`]s.
///
/// Each stem is a full clip object, so it is mapped with [`Clip::from_json`]:
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
    let clip = Clip::from_json(raw);
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

/// Parse `/api/billing/info/` into the billing snapshot we report in `doctor`.
///
/// Only genuinely invalid JSON bytes fail; any valid JSON value (even a
/// non-object such as `null` or `[]`) degrades to [`BillingInfo::default`].
pub(crate) fn parse_billing_info(body: &[u8]) -> Result<BillingInfo> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid billing JSON: {err}")))?;
    Ok(from_billing_json(&data))
}

/// Map the raw billing JSON into the domain [`BillingInfo`].
///
/// Reads each field independently through `.get()`, defaulting to `None`/empty
/// on a missing key or type mismatch, and never fails on a single field.
/// `features` is the union of `accessible_features[].name` and
/// `plan.usage_plan_features[].name`.
fn from_billing_json(data: &Value) -> BillingInfo {
    let plan = data.get("plan");
    let mut features = BTreeSet::new();
    collect_feature_names(data.get("accessible_features"), &mut features);
    collect_feature_names(
        plan.and_then(|plan| plan.get("usage_plan_features")),
        &mut features,
    );
    BillingInfo {
        total_credits_left: data.get("total_credits_left").and_then(json_i64),
        monthly_limit: data.get("monthly_limit").and_then(json_i64),
        monthly_usage: data.get("monthly_usage").and_then(json_i64),
        credits: data.get("credits").and_then(json_i64),
        period: json_string(data.get("period")),
        period_end: json_string(data.get("period_end")),
        renews_on: json_string(data.get("renews_on")),
        is_active: data.get("is_active").and_then(Value::as_bool),
        is_paused: data.get("is_paused").and_then(Value::as_bool),
        is_past_due: data.get("is_past_due").and_then(Value::as_bool),
        is_gifted: data.get("is_gifted").and_then(Value::as_bool),
        subscription_platform: json_string(data.get("subscription_platform")),
        plan_key: json_string(plan.and_then(|plan| plan.get("plan_key"))),
        plan_name: json_string(plan.and_then(|plan| plan.get("name"))),
        plan_level: plan.and_then(|plan| plan.get("level")).and_then(json_i64),
        features,
    }
}

/// Add the `name` of each `{ "name": ... }` element of a feature array to
/// `out`, skipping non-arrays, non-object elements, and empty or missing names.
fn collect_feature_names(array: Option<&Value>, out: &mut BTreeSet<String>) {
    let Some(items) = array.and_then(Value::as_array) else {
        return;
    };
    for name in items
        .iter()
        .filter_map(|item| item.get("name").and_then(Value::as_str))
    {
        if !name.is_empty() {
            out.insert(name.to_owned());
        }
    }
}

/// Parse the rendered-WAV response body (`{"wav_file_url": "..."}`).
///
/// Returns the URL when present and non-empty, `None` when the render is not
/// ready (an absent or empty `wav_file_url`), and an [`Error::Api`] only for
/// bytes that are not valid JSON.
pub(crate) fn parse_wav_url(body: &[u8]) -> Result<Option<String>> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid wav_file JSON: {err}")))?;
    Ok(data
        .get("wav_file_url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(str::to_string))
}

/// Unwrap a `{ "clip": {...} }` wrapper to the inner clip object, or return
/// `value` unchanged when it carries no object `clip` key (it is already bare).
fn unwrap_clip(value: &Value) -> &Value {
    value
        .get("clip")
        .filter(|clip| clip.is_object())
        .unwrap_or(value)
}

/// Read a signed integer that Suno may encode as a JSON integer, an integral
/// JSON float (`2450.0`), or a decimal string (`"2450"` or `"2450.0"`).
///
/// Non-integral values (`2450.5`), overflow, and junk yield `None`. The
/// conversion is lossless and never saturates a value into range.
fn json_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().and_then(f64_to_i64)),
        Value::String(text) => str_to_i64(text),
        _ => None,
    }
}

/// Convert a finite, integral `f64` to `i64`, rejecting fractional values and
/// anything outside the exactly representable range.
fn f64_to_i64(value: f64) -> Option<i64> {
    // Beyond 2^53 an f64 cannot losslessly represent an integer: serde has
    // already rounded (or saturated) such a value before we see it, so we
    // refuse rather than return a wrong result. Below 2^53 the cast is exact.
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        Some(value as i64)
    } else {
        None
    }
}

/// Parse a decimal string into `i64`, accepting an all-zero fractional part
/// (`"2450.0"`) but rejecting non-integral values, overflow, and junk.
fn str_to_i64(text: &str) -> Option<i64> {
    match text.split_once('.') {
        Some((integer, fraction)) => {
            let integral = fraction.is_empty() || fraction.bytes().all(|byte| byte == b'0');
            integral.then(|| integer.parse().ok()).flatten()
        }
        None => text.parse().ok(),
    }
}

/// Read an optional string field, cloning the value when present.
fn json_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
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

    /// One real anonymised `POST /api/feed/v3` page (issue #219): a single
    /// downloadable clip carrying `media_urls`, `user_id`, `batch_index`, cdn2
    /// artwork, and a pagination envelope with `has_more`/`next_cursor`.
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

    /// The anonymised full 43-field `GET /api/billing/info/` body from issue
    /// #223, used as a real-shape parse fixture.
    const BILLING_FULL: &str = r#"{
  "subscription_platform": "stripe",
  "is_active": true,
  "is_past_due": false,
  "credits": 0,
  "subscription_type": true,
  "subscription_anchor": "REDACTED",
  "subscription_id": "REDACTED",
  "renews_on": "REDACTED",
  "period": "month",
  "monthly_usage": 50,
  "monthly_limit": 2500,
  "credit_packs": [
    {
      "id": "00000000-0000-4000-8000-000000000001",
      "amount": 500,
      "price_usd": 4
    },
    {
      "id": "00000000-0000-4000-8000-000000000002",
      "amount": 1000,
      "price_usd": 8
    }
  ],
  "plan": {
    "id": "00000000-0000-4000-8000-000000000005",
    "level": 10,
    "plan_key": "pro",
    "name": "Pro Plan",
    "features": "Access to our newest model, v4\n2,500 credits (up to 500 songs), refreshes monthly\nCommercial use rights for songs made while subscribed\nCreate up to 10 songs at once\nEarly access to new features\nPriority creation queue\nAbility to purchase add-on credits",
    "monthly_price_usd": 10.0,
    "annual_price_usd": 96.0,
    "usage_plan_features": [
      {
        "name": "v4"
      },
      {
        "name": "cover"
      },
      {
        "name": "edit_mode"
      },
      {
        "name": "persona"
      },
      {
        "name": "can_buy_credit_top_ups"
      },
      {
        "name": "commercial_rights"
      },
      {
        "name": "get_stems"
      },
      {
        "name": "generate_song_image"
      },
      {
        "name": "auk"
      },
      {
        "name": "negative_tags"
      },
      {
        "name": "remaster"
      },
      {
        "name": "generate_song_video"
      },
      {
        "name": "long_uploads"
      },
      {
        "name": "convert_audio"
      },
      {
        "name": "create_control_sliders"
      },
      {
        "name": "playlist_condition"
      },
      {
        "name": "tag_upsample"
      },
      {
        "name": "custom_models"
      }
    ]
  },
  "models": [
    {
      "can_use": true,
      "max_lengths": {
        "title": 100,
        "prompt": 5000,
        "tags": 1000,
        "negative_tags": 1000,
        "gpt_description_prompt": 3000
      },
      "name": "Example Artist 5",
      "external_key": "chirp-fenix",
      "major_version": 5,
      "description": "[description redacted]",
      "is_default_free_model": false,
      "is_default_model": true,
      "badges": [
        "pro"
      ],
      "model_badges": [
        {
          "display_name": "Example Artist 1",
          "light": {
            "text_color": "000000",
            "background_color": "00000000",
            "border_color": "000000"
          },
          "dark": {
            "text_color": "FFFFFF",
            "background_color": "00000000",
            "border_color": "FFFFFF"
          }
        }
      ],
      "style": {
        "light": {
          "text_color": "FD429C"
        },
        "dark": {
          "text_color": "FD429C"
        }
      },
      "capabilities": [
        "all"
      ],
      "features": [
        "create_control_sliders",
        "tag_upsample",
        "mumble_mode",
        "vox_and_voices",
        "reuse_styles_lyrics"
      ],
      "allowed_condition_combinations": [
        [
          "extend"
        ],
        [
          "cover"
        ],
        [
          "infill"
        ],
        [
          "persona"
        ],
        [
          "persona",
          "extend"
        ],
        [
          "persona",
          "cover"
        ],
        [
          "playlist"
        ],
        [
          "underpaint"
        ],
        [
          "overpaint"
        ],
        [
          "vox"
        ],
        [
          "vox",
          "extend"
        ],
        [
          "vox",
          "cover"
        ],
        [
          "vox",
          "playlist"
        ],
        [
          "persona",
          "infill"
        ],
        [
          "cover",
          "infill"
        ]
      ],
      "id": "00000000-0000-4000-8000-000000000006"
    }
  ],
  "plan_price": 10.0,
  "plan_currency": "AUD",
  "plan_currency_price": 15.0,
  "payment_method_type": "card",
  "can_upgrade_immediately": true,
  "plans": [
    {
      "id": "00000000-0000-4000-8000-000000000015",
      "level": 0,
      "plan_key": "free",
      "name": "Free Plan",
      "features": "50 credits renew daily (10 songs)\nCreate up to 4 songs at once\nNo commercial use\nNo credit top ups\nShared generation queue",
      "monthly_price_usd": 0.0,
      "annual_price_usd": 0.0,
      "usage_plan_features": [
        {
          "name": "tag_upsample"
        }
      ],
      "prices": []
    }
  ],
  "accessible_features": [
    {
      "name": "v4"
    },
    {
      "name": "cover"
    },
    {
      "name": "edit_mode"
    },
    {
      "name": "persona"
    },
    {
      "name": "can_buy_credit_top_ups"
    },
    {
      "name": "commercial_rights"
    },
    {
      "name": "get_stems"
    },
    {
      "name": "generate_song_image"
    },
    {
      "name": "auk"
    },
    {
      "name": "negative_tags"
    },
    {
      "name": "remaster"
    },
    {
      "name": "generate_song_video"
    },
    {
      "name": "long_uploads"
    },
    {
      "name": "convert_audio"
    },
    {
      "name": "create_control_sliders"
    },
    {
      "name": "playlist_condition"
    },
    {
      "name": "tag_upsample"
    },
    {
      "name": "custom_models"
    }
  ],
  "revcat_subscriptions_offering_id": "REDACTED",
  "total_credits_left": 2450,
  "free_persona_clips_remaining": 0,
  "free_cover_clips_remaining": 0,
  "free_remasters_remaining": 0,
  "free_mobile_remasters_remaining": 0,
  "free_mobile_v4_gens_remaining": 0,
  "free_web_v4_gens_remaining": 0,
  "free_vox_gens_remaining": 0,
  "has_been_subscriber_before": true,
  "has_valid_school_email": false,
  "has_been_student_subscriber_before": false,
  "day0_boost": -1,
  "promotions": [],
  "audio_upload_limits": {
    "min": 6,
    "max": 1800
  },
  "voice_upload_limits": {
    "min": 10,
    "max": 900
  },
  "voice_record_limits": {
    "min": 10,
    "max": 240
  },
  "period_end": "REDACTED",
  "remaster_model_types": [
    {
      "name": "Example Artist 5",
      "external_key": "chirp-flounder",
      "is_default_model": true,
      "can_use": false
    },
    {
      "name": "Example Artist 2",
      "external_key": "chirp-carp",
      "is_default_model": false,
      "can_use": false
    },
    {
      "name": "v4.5+",
      "external_key": "chirp-bass",
      "is_default_model": false,
      "can_use": false
    }
  ],
  "is_pause_scheduled": false,
  "is_paused": false,
  "is_gifted": false
}"#;

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
        // flag warns the caller to disarm deletion (#248).
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

    #[test]
    fn audiopipe_url_is_rewritten_to_cdn() {
        let raw =
            serde_json::json!({"id": "x", "audio_url": "https://audiopipe.suno.ai/?item_id=x"});
        assert_eq!(
            Clip::from_json(&raw).audio_url,
            "https://cdn1.suno.ai/x.mp3"
        );
    }

    #[test]
    fn parse_billing_info_reads_full_real_body() {
        let billing = parse_billing_info(BILLING_FULL.as_bytes()).unwrap();
        assert_eq!(billing.total_credits_left, Some(2450));
        assert_eq!(billing.monthly_limit, Some(2500));
        assert_eq!(billing.monthly_usage, Some(50));
        assert_eq!(billing.credits, Some(0));
        assert_eq!(billing.period.as_deref(), Some("month"));
        assert_eq!(billing.is_active, Some(true));
        assert_eq!(billing.is_paused, Some(false));
        assert_eq!(billing.is_past_due, Some(false));
        assert_eq!(billing.is_gifted, Some(false));
        assert_eq!(billing.subscription_platform.as_deref(), Some("stripe"));
        assert_eq!(billing.plan_key.as_deref(), Some("pro"));
        assert_eq!(billing.plan_name.as_deref(), Some("Pro Plan"));
        assert_eq!(billing.plan_level, Some(10));
        assert!(billing.can_get_stems());
        assert!(billing.can_convert_audio());
        assert!(billing.has_feature("custom_models"));
    }

    #[test]
    fn json_i64_reads_string_encoded_integer() {
        let billing = parse_billing_info(br#"{"total_credits_left":"2450"}"#).unwrap();
        assert_eq!(billing.total_credits_left, Some(2450));
    }

    #[test]
    fn json_i64_reads_integral_float() {
        let billing = parse_billing_info(br#"{"total_credits_left":2450.0}"#).unwrap();
        assert_eq!(billing.total_credits_left, Some(2450));
    }

    #[test]
    fn json_i64_reads_negative_sentinel() {
        let billing = parse_billing_info(br#"{"total_credits_left":-1}"#).unwrap();
        assert_eq!(billing.total_credits_left, Some(-1));
    }

    #[test]
    fn json_i64_rejects_non_integral_float_but_object_still_parses() {
        let billing =
            parse_billing_info(br#"{"total_credits_left":2450.5,"period":"month"}"#).unwrap();
        assert_eq!(billing.total_credits_left, None);
        assert_eq!(billing.period.as_deref(), Some("month"));
    }

    #[test]
    fn str_to_i64_handles_encodings_and_junk() {
        assert_eq!(str_to_i64("2450"), Some(2450));
        assert_eq!(str_to_i64("2450.0"), Some(2450));
        assert_eq!(str_to_i64("-1"), Some(-1));
        assert_eq!(str_to_i64("2450.5"), None);
        assert_eq!(str_to_i64(".5"), None);
        assert_eq!(str_to_i64("nope"), None);
        assert_eq!(str_to_i64("99999999999999999999999"), None);
    }

    #[test]
    fn json_i64_rejects_overflow() {
        let billing =
            parse_billing_info(br#"{"total_credits_left":99999999999999999999999}"#).unwrap();
        assert_eq!(billing.total_credits_left, None);
    }

    #[test]
    fn json_i64_covers_i64_and_float_boundaries() {
        // Integers arrive through the lossless i64 path, so the full i64 range works.
        assert_eq!(json_i64(&serde_json::json!(i64::MAX)), Some(i64::MAX));
        assert_eq!(json_i64(&serde_json::json!(i64::MIN)), Some(i64::MIN));
        // A JSON integer of 2^63 exceeds i64::MAX and must not saturate.
        assert_eq!(
            json_i64(&serde_json::json!(9_223_372_036_854_775_808_u64)),
            None
        );
        // Floats are trusted only below 2^53, so both i64 extremes are rejected.
        assert_eq!(f64_to_i64(i64::MAX as f64), None);
        assert_eq!(f64_to_i64(i64::MIN as f64), None);
        assert_eq!(f64_to_i64(2450.5), None);
        assert_eq!(f64_to_i64(f64::NAN), None);
        assert_eq!(f64_to_i64(f64::INFINITY), None);
    }

    #[test]
    fn f64_to_i64_rejects_values_below_i64_min() {
        // A float below i64::MIN must not silently saturate to i64::MIN.
        let below_min: f64 = "-9223372036854775809".parse().unwrap();
        assert_eq!(f64_to_i64(below_min), None);
        // The matching string is rejected by the lossless i64 parse.
        assert_eq!(str_to_i64("-9223372036854775809"), None);
        assert_eq!(json_i64(&serde_json::json!("-9223372036854775809")), None);
    }

    #[test]
    fn f64_to_i64_trusts_only_the_safe_integer_range() {
        // 2^53 - 1 is the largest integer an f64 represents exactly.
        assert_eq!(
            f64_to_i64(9_007_199_254_740_991.0),
            Some(9_007_199_254_740_991)
        );
        // 9007199254740993 (2^53 + 1) is not representable, so serde rounds it to
        // 2^53 before we see it; the rounded value must be refused, not returned.
        let rounded: f64 = "9007199254740993".parse().unwrap();
        assert_eq!(rounded, 9_007_199_254_740_992.0);
        assert_eq!(f64_to_i64(rounded), None);
    }

    #[test]
    fn parse_billing_info_defaults_missing_fields() {
        let billing = parse_billing_info(br#"{"monthly_usage":12}"#).unwrap();
        assert_eq!(billing.total_credits_left, None);
        assert_eq!(billing.monthly_usage, Some(12));
        assert_eq!(billing.plan_key, None);
        assert!(billing.features.is_empty());
        assert!(!billing.can_get_stems());
    }

    #[test]
    fn from_billing_json_ignores_surprising_types() {
        // `subscription_type` is a bool despite its name; a numeric field carrying
        // the wrong type must fall back to None rather than panic.
        let value = serde_json::json!({
            "subscription_type": true,
            "total_credits_left": {"unexpected": "object"},
            "is_active": "yes",
        });
        let billing = from_billing_json(&value);
        assert_eq!(billing.total_credits_left, None);
        assert_eq!(billing.is_active, None);
    }

    #[test]
    fn parse_billing_info_treats_non_object_json_as_default() {
        for body in [
            b"null".as_slice(),
            b"[]".as_slice(),
            br#""hello""#.as_slice(),
        ] {
            assert_eq!(parse_billing_info(body).unwrap(), BillingInfo::default());
        }
    }

    #[test]
    fn parse_billing_info_rejects_non_json_bytes() {
        let err = parse_billing_info(b"nope").unwrap_err();
        assert!(err.to_string().contains("invalid billing JSON"));
    }

    #[test]
    fn from_billing_json_unions_feature_sources() {
        let accessible_only = serde_json::json!({
            "accessible_features": [{"name": "get_stems"}],
        });
        assert!(from_billing_json(&accessible_only).can_get_stems());

        let plan_only = serde_json::json!({
            "plan": {"usage_plan_features": [{"name": "convert_audio"}]},
        });
        assert!(from_billing_json(&plan_only).can_convert_audio());

        let both = serde_json::json!({
            "accessible_features": [{"name": "get_stems"}, {"name": ""}, {"other": "x"}],
            "plan": {"usage_plan_features": [{"name": "convert_audio"}]},
        });
        let billing = from_billing_json(&both);
        assert!(billing.can_get_stems());
        assert!(billing.can_convert_audio());
        // Empty and malformed feature entries are ignored.
        assert_eq!(billing.features.len(), 2);
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
