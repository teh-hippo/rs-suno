//! Playlist `.m3u8` planning: turns the desired playlists into the
//! `WriteArtifact`/`DeleteArtifact` set, with the playlist-specific delete gate.

use super::*;

/// Plan the `.m3u8` writes and deletes for this run's playlists.
///
/// # Writes
///
/// For each desired playlist a single [`Action::WriteArtifact`] of kind
/// [`Playlist`](ArtifactKind::Playlist) is emitted (carrying the rendered body
/// inline in `content`) when the store lacks the playlist, its stored hash
/// differs, its stored path differs, or the tracked file is absent (or empty)
/// on disk. The hash is taken over the full rendered text, so a name, order,
/// path, title, or duration change all trigger a rewrite (HARDENING B1); an
/// unchanged, present playlist writes nothing (idempotent).
///
/// `local` is a path-keyed probe map built by the caller. A stored path that
/// resolves to a missing or zero-size file forces `needs_write = true`.  A path
/// absent from `local` (probe unavailable) falls back to hash/path comparison.
///
/// A **rename** (the same id whose sanitised name, and so path, changed) writes
/// the new file and, gated exactly like a stale delete (`can_delete &&
/// list_fully_enumerated`), also deletes the old stored path so the previous
/// `<oldname>.m3u8` does not linger.
///
/// # Deletes (HARDENING B2 — paramount)
///
/// A stored playlist absent from `desired` is stale (removed on Suno) and its
/// file is deleted **only** when `can_delete` AND `list_fully_enumerated`. The
/// second gate is the playlist-specific safety valve: `list_fully_enumerated`
/// is `true` only when the `/api/playlist/me` listing succeeded and was fully
/// paginated. If that listing **failed or was not fully enumerated**, the caller
/// passes `list_fully_enumerated = false` (and an empty `desired`), so this
/// function emits **zero deletes and zero writes** and every existing `.m3u8` is
/// left untouched. A failed *member* fetch for one playlist is handled upstream
/// by excluding that id from BOTH `desired` and `stored`, so it is never treated
/// as stale here.
///
/// The output is deterministic (sorted by `(owner_id, kind)`) and self-suppresses
/// path aliasing, so a rename to a name another playlist also renders this run
/// downgrades the colliding delete rather than removing a just-written file.
pub fn plan_playlist_artifacts(
    desired: &[PlaylistDesired],
    stored: &BTreeMap<String, PlaylistState>,
    can_delete: bool,
    list_fully_enumerated: bool,
    local: &HashMap<String, LocalFile>,
) -> Vec<Action> {
    let mut actions: Vec<Action> = Vec::new();
    let desired_ids: BTreeSet<&str> = desired.iter().map(|d| d.id.as_str()).collect();

    for d in desired {
        let stored_here = stored.get(&d.id);
        let needs_write = needs_write_drift(
            stored_here.map(|state| (state.hash.as_str(), state.path.as_str())),
            d.hash.as_str(),
            d.path.as_str(),
            local,
        );
        if needs_write {
            actions.push(Action::WriteArtifact {
                kind: ArtifactKind::Playlist,
                path: d.path.clone(),
                source_url: String::new(),
                hash: d.hash.clone(),
                owner_id: d.id.clone(),
                content: Some(d.content.clone()),
            });
        }
        // A rename changed the path: remove the old file, under the playlist gate.
        if let Some(state) = stored_here
            && state.path != d.path
            && let Some(action) = delete_playlist_artifact_action(
                &d.id,
                &state.path,
                can_delete,
                list_fully_enumerated,
            )
        {
            actions.push(action);
        }
    }

    // Stale playlists (removed on Suno) are deleted only under the full playlist
    // gate, so a failed or partial listing never removes an existing `.m3u8` (B2).
    for (id, state) in stored {
        if !desired_ids.contains(id.as_str())
            && let Some(action) =
                delete_playlist_artifact_action(id, &state.path, can_delete, list_fully_enumerated)
        {
            actions.push(action);
        }
    }

    actions.sort_by(|a, b| playlist_action_key(a).cmp(&playlist_action_key(b)));
    // A rename to a name another playlist also renders this run must not delete
    // the file that write just produced; downgrade any such colliding delete.
    suppress_path_aliasing(&mut actions);
    actions
}

/// The `(owner_id, is_delete)` sort key for a playlist action, so writes and
/// deletes for one id stay adjacent and order is deterministic.
fn playlist_action_key(action: &Action) -> (&str, u8) {
    match action {
        Action::WriteArtifact { owner_id, .. } => (owner_id.as_str(), 0),
        Action::DeleteArtifact { owner_id, .. } => (owner_id.as_str(), 1),
        Action::Skip { clip_id } => (clip_id.as_str(), 2),
        _ => ("", 3),
    }
}

/// The gate every playlist `.m3u8` `DeleteArtifact` passes through.
///
/// The playlist analogue of [`delete_artifact_action`]. A playlist is owned by
/// its Suno UUID rather than a manifest clip and carries no preserve mark, so
/// the gate is the shared [`deletion_allowed`] verdict (`can_delete`) AND the
/// stricter playlist valve `list_fully_enumerated` (HARDENING B2 — a failed or
/// partial `/api/playlist/me` listing must remove nothing), plus a non-empty
/// `path`. A `None` result means the caller must keep the `.m3u8`.
pub(crate) fn delete_playlist_artifact_action(
    owner_id: &str,
    path: &str,
    can_delete: bool,
    list_fully_enumerated: bool,
) -> Option<Action> {
    if !can_delete || !list_fully_enumerated || path.is_empty() {
        return None;
    }
    Some(Action::DeleteArtifact {
        kind: ArtifactKind::Playlist,
        path: path.to_string(),
        owner_id: owner_id.to_string(),
    })
}
