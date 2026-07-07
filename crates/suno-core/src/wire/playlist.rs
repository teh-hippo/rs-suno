//! Playlist listing and member decoding (`/api/playlist/me`, `/api/playlist/{id}/`).

use serde_json::Value;

use crate::error::{Error, Result};
use crate::model::{Clip, Playlist};

use super::clip::unwrap_clip;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_playlists_reads_fields_and_drops_idless_defaulting_name() {
        let body = serde_json::json!({
            "playlists": [
                {"id": "p1", "name": "Mix", "num_total_results": 3},
                {"id": "", "name": "Ghost"},
                {"name": "No Id"},
                {"id": "p2"}
            ]
        })
        .to_string();
        let playlists = parse_playlists(body.as_bytes()).unwrap();
        // The two id-less entries are dropped; a missing/empty name defaults to
        // Untitled so the file name is never empty.
        assert_eq!(playlists.len(), 2);
        assert_eq!(playlists[0].id, "p1");
        assert_eq!(playlists[0].name, "Mix");
        assert_eq!(playlists[0].num_clips, 3);
        assert_eq!(playlists[1].id, "p2");
        assert_eq!(playlists[1].name, "Untitled");
        assert_eq!(playlists[1].num_clips, 0);
    }

    #[test]
    fn parse_playlists_rejects_non_json_bytes() {
        assert!(parse_playlists(b"nope").is_err());
    }

    #[test]
    fn parse_playlist_clips_unwraps_members_and_preserves_order() {
        let body = serde_json::json!({
            "num_total_results": 2,
            "playlist_clips": [
                {"clip": {"id": "a", "title": "A"}},
                {"id": "b", "title": "B"}
            ]
        })
        .to_string();
        let (clips, complete) = parse_playlist_clips(body.as_bytes()).unwrap();
        assert_eq!(
            clips.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(complete);
    }

    #[test]
    fn parse_playlist_clips_completeness_gates_deletion_authority() {
        // D5: a page is authoritative for deletion only when num_total_results is
        // present AND equals the raw member count AND no member was dropped by the
        // empty-id filter. A short/paginated page, a missing total, or a dropped
        // empty-id member each fail safe to `false`.
        let short = serde_json::json!({
            "num_total_results": 5,
            "playlist_clips": [{"id": "a"}, {"id": "b"}]
        })
        .to_string();
        assert!(!parse_playlist_clips(short.as_bytes()).unwrap().1);

        let missing_total = serde_json::json!({"playlist_clips": [{"id": "a"}]}).to_string();
        assert!(!parse_playlist_clips(missing_total.as_bytes()).unwrap().1);

        let dropped = serde_json::json!({
            "num_total_results": 2,
            "playlist_clips": [{"id": "a"}, {"id": ""}]
        })
        .to_string();
        let (clips, complete) = parse_playlist_clips(dropped.as_bytes()).unwrap();
        assert_eq!(clips.len(), 1);
        assert!(!complete);
    }

    #[test]
    fn parse_playlist_clips_rejects_non_json_bytes() {
        assert!(parse_playlist_clips(b"nope").is_err());
    }
}
