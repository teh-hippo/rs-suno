//! Reconciled album and playlist art-file state.
//!
//! The sync writes album folder art (`folder.jpg`/`cover.webp`/`cover.mp4`) and
//! per-playlist `.m3u8` sidecars beside the library, and records what it wrote
//! here so a later reconcile rewrites only on a genuine content change. These
//! rows live on the durable [`LineageStore`](crate::LineageStore) (its `albums`
//! and `playlists` maps) but are a concern distinct from the lineage graph, so
//! the types and their store accessors live in their own module. Kept
//! relational so they migrate cleanly to SQLite `album_art`/`playlists` tables.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::manifest::ArtifactState;
use crate::reconcile::ArtifactKind;

/// The reconciled folder-art state for one album (one stable root id).
///
/// Folder art is album-scoped, not per-clip, so it lives here rather than on a
/// [`ManifestEntry`](crate::manifest::ManifestEntry). Each slot records the
/// sidecar's path and the content hash of the art it was rendered from, so a
/// later reconcile rewrites only on a genuine content change (HARDENING H1: a
/// most-played flip that yields the same art hash is a no-op). Kept relational
/// (two explicit slots) so it migrates cleanly to a SQLite `album_art` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlbumArt {
    /// The album's static `folder.jpg`, sourced from the most-played variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_jpg: Option<ArtifactState>,
    /// The album's animated `cover.webp`, from the first-created animated variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_webp: Option<ArtifactState>,
    /// The album's raw `cover.mp4`: the same variant's `video_cover_url` kept
    /// verbatim (no transcode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_mp4: Option<ArtifactState>,
}

impl AlbumArt {
    /// The stored state for one folder-art `kind`, if present. Per-clip and
    /// library kinds have no album slot and map to `None`.
    pub fn artifact(&self, kind: ArtifactKind) -> Option<&ArtifactState> {
        match kind {
            ArtifactKind::FolderJpg => self.folder_jpg.as_ref(),
            ArtifactKind::FolderWebp => self.folder_webp.as_ref(),
            ArtifactKind::FolderMp4 => self.folder_mp4.as_ref(),
            ArtifactKind::CoverJpg
            | ArtifactKind::CoverWebp
            | ArtifactKind::DetailsTxt
            | ArtifactKind::LyricsTxt
            | ArtifactKind::Lrc
            | ArtifactKind::VideoMp4
            | ArtifactKind::Playlist => None,
        }
    }

    /// Set (or clear, with `None`) the state for one folder-art `kind`.
    ///
    /// The executor calls this after a folder-art write (with the new state) or
    /// delete (with `None`), so the kind-to-slot mapping lives in one place.
    /// Non-album kinds have no slot here and are no-ops.
    pub fn set(&mut self, kind: ArtifactKind, state: Option<ArtifactState>) {
        match kind {
            ArtifactKind::FolderJpg => self.folder_jpg = state,
            ArtifactKind::FolderWebp => self.folder_webp = state,
            ArtifactKind::FolderMp4 => self.folder_mp4 = state,
            ArtifactKind::CoverJpg
            | ArtifactKind::CoverWebp
            | ArtifactKind::DetailsTxt
            | ArtifactKind::LyricsTxt
            | ArtifactKind::Lrc
            | ArtifactKind::VideoMp4
            | ArtifactKind::Playlist => {}
        }
    }

    /// True when the album holds no folder art at all (every slot empty), so the
    /// store can prune the now-dead album row.
    pub fn is_empty(&self) -> bool {
        self.folder_jpg.is_none() && self.folder_webp.is_none() && self.folder_mp4.is_none()
    }
}

/// The reconciled `.m3u8` state for one playlist.
///
/// A playlist's body is *generated*, not fetched, so unlike per-clip artifacts
/// its change detection is a single content hash over the full rendered text
/// (HARDENING B1: name, order, and every member's path/title/duration feed it).
/// The `path` is the sidecar's library-relative location, tracked so a rename
/// (a playlist renamed on Suno) is detected and the old file removed. Kept as a
/// flat row so it migrates cleanly to a SQLite `playlists` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistState {
    /// The playlist's display name at the time it was last written.
    pub name: String,
    /// The `.m3u8` file's library-relative path (`<sanitised name>.m3u8`).
    pub path: String,
    /// The content hash of the rendered `.m3u8` this row was written from.
    pub hash: String,
}

/// Upsert (`Some`) or clear (`None`) one folder-art `kind` for the album rooted
/// at `root_id`. A clear that empties the row removes it, so the store never
/// keeps a dead all-`None` album entry. Single home for the prune-when-empty
/// invariant shared by the executor write/clear paths.
pub(crate) fn set_album_artifact(
    albums: &mut BTreeMap<String, AlbumArt>,
    root_id: &str,
    kind: ArtifactKind,
    state: Option<ArtifactState>,
) {
    match state {
        Some(state) => albums
            .entry(root_id.to_owned())
            .or_default()
            .set(kind, Some(state)),
        None => {
            if let Some(art) = albums.get_mut(root_id) {
                art.set(kind, None);
                if art.is_empty() {
                    albums.remove(root_id);
                }
            }
        }
    }
}

/// Upsert (`Some`) or remove (`None`) the `.m3u8` state for playlist `id`, so a
/// delete never leaves a dangling row.
pub(crate) fn set_playlist(
    playlists: &mut BTreeMap<String, PlaylistState>,
    id: &str,
    state: Option<PlaylistState>,
) {
    match state {
        Some(state) => {
            playlists.insert(id.to_owned(), state);
        }
        None => {
            playlists.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LineageStore;

    #[test]
    fn album_art_roundtrips_and_reads_by_kind() {
        let mut store = LineageStore::new();
        store.albums.insert(
            "root-1".to_owned(),
            AlbumArt {
                folder_jpg: Some(ArtifactState {
                    path: "alice/Album/folder.jpg".to_owned(),
                    hash: "jpg-h".to_owned(),
                }),
                folder_webp: Some(ArtifactState {
                    path: "alice/Album/cover.webp".to_owned(),
                    hash: "webp-h".to_owned(),
                }),
                folder_mp4: Some(ArtifactState {
                    path: "alice/Album/cover.mp4".to_owned(),
                    hash: "mp4-h".to_owned(),
                }),
            },
        );

        let json = serde_json::to_string(&store).unwrap();
        let back: LineageStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store, back);

        // The serialised shape is a relational `albums` map keyed by root id.
        let value: serde_json::Value = serde_json::to_value(&store).unwrap();
        let album = value.get("albums").unwrap().get("root-1").unwrap();
        assert_eq!(
            album.get("folder_jpg").unwrap().get("hash").unwrap(),
            "jpg-h"
        );

        let art = back.albums.get("root-1").unwrap();
        assert_eq!(
            art.artifact(ArtifactKind::FolderJpg).unwrap().path,
            "alice/Album/folder.jpg"
        );
        assert_eq!(
            art.artifact(ArtifactKind::FolderWebp).unwrap().hash,
            "webp-h"
        );
        assert_eq!(art.artifact(ArtifactKind::FolderMp4).unwrap().hash, "mp4-h");
        // A per-clip kind has no album slot.
        assert!(art.artifact(ArtifactKind::CoverJpg).is_none());
    }

    #[test]
    fn empty_album_art_omits_slots_when_serialised() {
        // An all-`None` AlbumArt round-trips and writes an empty object, so the
        // absent-slot default holds both ways.
        let empty = AlbumArt::default();
        assert!(empty.is_empty());
        let value = serde_json::to_value(&empty).unwrap();
        assert!(value.get("folder_jpg").is_none());
        assert!(value.get("folder_webp").is_none());
        let back: AlbumArt = serde_json::from_str("{}").unwrap();
        assert_eq!(back, empty);
    }

    #[test]
    fn set_album_artifact_upserts_then_prunes_when_emptied() {
        let mut store = LineageStore::new();
        let jpg = ArtifactState {
            path: "a/folder.jpg".to_owned(),
            hash: "h1".to_owned(),
        };
        set_album_artifact(
            &mut store.albums,
            "root-1",
            ArtifactKind::FolderJpg,
            Some(jpg.clone()),
        );
        assert_eq!(store.albums.get("root-1").unwrap().folder_jpg, Some(jpg));

        // Clearing the only slot prunes the whole album row (no dead entries).
        set_album_artifact(&mut store.albums, "root-1", ArtifactKind::FolderJpg, None);
        assert!(!store.albums.contains_key("root-1"));
        assert!(store.albums.is_empty());
    }

    #[test]
    fn album_row_survives_until_the_last_slot_including_folder_mp4_is_cleared() {
        // Regression: `is_empty` must count every slot. A `both`-retention album
        // owns folder_webp + folder_mp4; clearing folder_webp first must NOT
        // prune the row while folder_mp4 is still stored, or the later cover.mp4
        // delete would lose its store entry and never retry on failure.
        let mut store = LineageStore::new();
        let state = |p: &str| ArtifactState {
            path: p.to_owned(),
            hash: "h".to_owned(),
        };
        set_album_artifact(
            &mut store.albums,
            "root-1",
            ArtifactKind::FolderWebp,
            Some(state("a/cover.webp")),
        );
        set_album_artifact(
            &mut store.albums,
            "root-1",
            ArtifactKind::FolderMp4,
            Some(state("a/cover.mp4")),
        );

        // FolderWebp is cleared first (its kind sorts before FolderMp4); the row
        // must stay because the raw cover is still tracked.
        set_album_artifact(&mut store.albums, "root-1", ArtifactKind::FolderWebp, None);
        let art = store
            .albums
            .get("root-1")
            .expect("row kept while folder_mp4 remains");
        assert!(!art.is_empty());
        assert!(art.folder_mp4.is_some());

        // Clearing the last slot finally prunes the row.
        set_album_artifact(&mut store.albums, "root-1", ArtifactKind::FolderMp4, None);
        assert!(!store.albums.contains_key("root-1"));
        assert!(store.albums.is_empty());
    }

    #[test]
    fn playlist_state_roundtrips_by_id() {
        let mut store = LineageStore::new();
        store.playlists.insert(
            "pl1".to_owned(),
            PlaylistState {
                name: "Road Trip".to_owned(),
                path: "Road Trip.m3u8".to_owned(),
                hash: "abc123".to_owned(),
            },
        );

        let json = serde_json::to_string(&store).unwrap();
        let back: LineageStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store, back);

        // The serialised shape is a relational `playlists` map keyed by id.
        let value: serde_json::Value = serde_json::to_value(&store).unwrap();
        let pl = value.get("playlists").unwrap().get("pl1").unwrap();
        assert_eq!(pl.get("path").unwrap(), "Road Trip.m3u8");
        assert_eq!(pl.get("hash").unwrap(), "abc123");

        let stored = back.playlists.get("pl1").unwrap();
        assert_eq!(stored.name, "Road Trip");
        assert_eq!(stored.hash, "abc123");
    }

    #[test]
    fn set_playlist_upserts_then_clears() {
        let mut store = LineageStore::new();
        let state = PlaylistState {
            name: "Mix".to_owned(),
            path: "Mix.m3u8".to_owned(),
            hash: "h1".to_owned(),
        };
        set_playlist(&mut store.playlists, "pl1", Some(state.clone()));
        assert_eq!(store.playlists.get("pl1"), Some(&state));

        // A rewrite replaces the row in place.
        let renamed = PlaylistState {
            name: "Mix v2".to_owned(),
            path: "Mix v2.m3u8".to_owned(),
            hash: "h2".to_owned(),
        };
        set_playlist(&mut store.playlists, "pl1", Some(renamed.clone()));
        assert_eq!(store.playlists.get("pl1"), Some(&renamed));

        // Clearing removes the row so no dangling entry survives a delete.
        set_playlist(&mut store.playlists, "pl1", None);
        assert!(!store.playlists.contains_key("pl1"));
        assert!(store.playlists.is_empty());
    }
}
