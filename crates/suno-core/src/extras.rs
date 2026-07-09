//! Pure "media extras" generators: M3U8 playlists, per-song text sidecars, and
//! the library index.
//!
//! Every function is pure: clip data and relative paths in, the text the CLI
//! writes to disk out, with no IO, clock, or network.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::consts::SUNO_SONG_BASE_URL;
use crate::graph::LineageStore;
use crate::lineage::LineageContext;
use crate::manifest::Manifest;
use crate::model::Clip;
use crate::tag::{TrackMetadata, non_empty};
use crate::textfmt::{format_duration, to_single_line};
use crate::vocab::AudioFormat;

/// The schema version of the library index document.
///
/// Bump this only when the index shape changes. The field set is additive:
/// fields may be added, never renamed or repurposed.
pub const INDEX_SCHEMA_VERSION: u32 = 1;

/// One ordered entry in an extended-M3U8 playlist. Order is significant and
/// preserved exactly as given.
///
/// An empty `relative_path` marks a member absent from the local library, which
/// renders as a `# (not in library) <title>` comment rather than an `#EXTINF` +
/// path pair, so the playlist never carries a dangling path (HARDENING L1).
#[derive(Debug, Clone, Copy)]
pub struct M3u8Entry<'a> {
    pub title: &'a str,
    pub duration_secs: f64,
    pub relative_path: &'a str,
}

/// Render an extended-M3U8 playlist named `name` from `entries`, preserving
/// their order.
///
/// Opens with `#EXTM3U` and `#PLAYLIST:<name>`, then per entry emits either an
/// `#EXTINF:<seconds>,<title>` line plus the path, or a `# (not in library)`
/// comment for an empty relative path (HARDENING L1). CR/LF in any field is
/// folded to spaces so one field can never break the line structure.
pub fn render_m3u8(name: &str, entries: &[M3u8Entry<'_>]) -> String {
    let mut out = String::from("#EXTM3U\n");
    let _ = writeln!(out, "#PLAYLIST:{}", to_single_line(name));
    for entry in entries {
        let title = to_single_line(entry.title);
        if entry.relative_path.is_empty() {
            // L1: an absent member renders as a comment, never a dangling path.
            let _ = writeln!(out, "# (not in library) {title}");
            continue;
        }
        let path = to_single_line(entry.relative_path);
        let seconds = extinf_seconds(entry.duration_secs);
        let _ = write!(out, "#EXTINF:{seconds},{title}\n{path}\n");
    }
    out
}

/// One clip's row in the library index.
///
/// The field set is stable and additive: add fields, never rename them. Genuinely
/// unknown live-only fields are `null` (`Option::None`), never an empty string or
/// `0`, so a consumer can tell "absent from this run's live feed" from "empty".
#[derive(Debug, Serialize)]
struct IndexEntry {
    id: String,
    path: String,
    format: AudioFormat,
    size: u64,
    title: String,
    artist: Option<String>,
    handle: Option<String>,
    album: String,
    root_id: String,
    created_at: Option<String>,
    duration: Option<f64>,
    tags: Option<String>,
}

/// The serialised shape of the whole-library index.
#[derive(Debug, Serialize)]
struct LibraryIndex {
    schema_version: u32,
    clips: Vec<IndexEntry>,
}

/// Render the whole-library index as a stable, pretty-printed JSON document.
///
/// One row per `manifest` entry in clip-id order, listing only clips whose file
/// exists on disk so the index never advertises a missing file. Durable fields
/// come from the manifest and the archived [`LineageStore`]; live-only fields
/// (artist, handle, duration, tags) come from `live` when the clip was seen this
/// run and are `null` otherwise. `album` is the raw logical title, which
/// legitimately differs from the sanitised segment inside `path`.
pub fn render_library_index(
    manifest: &Manifest,
    store: &LineageStore,
    live: &HashMap<&str, &Clip>,
) -> String {
    let clips = manifest
        .iter()
        .map(|(id, entry)| {
            let live_clip = live.get(id.as_str()).copied();
            let title = live_clip
                .map(|clip| clip.title.clone())
                .filter(|title| !title.is_empty())
                .or_else(|| {
                    store
                        .node(id)
                        .map(|node| node.title.clone())
                        .filter(|title| !title.is_empty())
                })
                .unwrap_or_else(|| "Untitled".to_owned());
            let artist =
                live_clip.map(|clip| non_empty(&clip.display_name).unwrap_or("Suno").to_owned());
            let handle = live_clip.and_then(|clip| non_empty(&clip.handle).map(str::to_owned));
            let album = match live_clip {
                Some(clip) => store.context_for(clip).album(&clip.title),
                None => store.album_for_id(id),
            };
            let root_id = store
                .get_root(id)
                .map(|cached| cached.root_id.clone())
                .filter(|root| !root.is_empty())
                .unwrap_or_else(|| id.clone());
            let created_at = store
                .node(id)
                .map(|node| node.created_at.clone())
                .filter(|created| !created.is_empty());
            let duration = live_clip.map(|clip| clip.duration);
            let tags = live_clip.map(|clip| clip.tags.clone());
            IndexEntry {
                id: id.clone(),
                path: entry.path.clone(),
                format: entry.format,
                size: entry.size,
                title,
                artist,
                handle,
                album,
                root_id,
                created_at,
                duration,
                tags,
            }
        })
        .collect();
    let index = LibraryIndex {
        schema_version: INDEX_SCHEMA_VERSION,
        clips,
    };
    // LibraryIndex is owned strings and numbers, so serialising to a String cannot fail.
    #[allow(clippy::expect_used)]
    let json = serde_json::to_string_pretty(&index).expect("library index serialises");
    json
}

/// Round a duration in seconds to the nearest whole second for `#EXTINF`.
///
/// Non-finite inputs fold to `0` so the playlist line stays well-formed.
fn extinf_seconds(duration_secs: f64) -> i64 {
    if duration_secs.is_finite() {
        duration_secs.round() as i64
    } else {
        0
    }
}
/// Render the plain-text per-song details sidecar for `clip`.
///
/// A fixed-order block of `Label: value` lines from the same [`TrackMetadata`]
/// that drives the embedded tags, plus the clip id, `mm:ss` duration, and the
/// canonical `https://suno.com/song/<id>` page URL. Empty fields are omitted.
/// The generation prompt is labelled `Prompt:`, never `Lyrics:`.
/// [`TrackMetadata`] carries no URLs, so signed CDN links are excluded
/// automatically. Every value is folded to one line so a field can never break
/// the block.
pub fn render_clip_details(clip: &Clip, lineage: &LineageContext) -> String {
    let meta = TrackMetadata::from_clip(clip, lineage);
    let url = if clip.id.is_empty() {
        String::new()
    } else {
        format!("{SUNO_SONG_BASE_URL}/{}", clip.id)
    };
    let fields: [(&str, &str); 17] = [
        ("Title", &meta.title),
        ("Artist", &meta.artist),
        ("Album", &meta.album),
        ("Album Artist", &meta.album_artist),
        ("Date", &meta.date),
        ("Duration", &format_duration(clip.duration)),
        ("Model", &meta.model),
        ("Handle", &meta.handle),
        ("Style", &meta.style),
        ("Style Summary", &meta.style_summary),
        ("Comment", &meta.comment),
        ("Prompt", &clip.prompt),
        ("Parent", &meta.parent),
        ("Root", &meta.root),
        ("Lineage", &meta.lineage),
        ("Id", &clip.id),
        ("Url", &url),
    ];
    let mut out = String::new();
    for (label, value) in fields {
        if value.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{label}: {}", to_single_line(value));
    }
    out
}

#[cfg(test)]
mod tests;
