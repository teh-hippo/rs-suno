//! Album folder-art planning: turns the desired album art into the
//! `WriteArtifact`/`DeleteArtifact` set, with the album-specific delete gate.

use super::*;

/// Plan the folder-art writes and deletes for this run's albums.
///
/// Writes are keyed on the CHOSEN ART CONTENT HASH (and the target path), never
/// the source clip id: for each present desired kind, a [`Action::WriteArtifact`]
/// is emitted only when the album store lacks that kind, its stored hash differs,
/// its stored path differs, or the tracked file is absent (or empty) on disk.
/// When hash, path, and disk presence all match, nothing is written, so a
/// most-played flip that resolves to the same art content is a no-op
/// (HARDENING H1). Exactly one write can be emitted per album per kind.
///
/// `local` is a path-keyed probe map built by the caller. A stored path that
/// resolves to a missing or zero-size file forces `needs_write = true`.  A path
/// absent from `local` (probe unavailable) falls back to hash/path comparison.
///
/// Deletes cover any stored album/kind no longer desired — the album emptied (no
/// selected clips root there this run) or the kind's source disappeared (no
/// art-bearing or animated variant). Each is emitted only when `can_delete` (the
/// shared [`deletion_allowed`] verdict), so folder art is never removed on an
/// empty, failed, partial, or truncated listing. Folder art has no preserve
/// concept; the `can_delete` gate is the guard.
///
/// The output is deterministic: actions are sorted by `(root_id, kind)`, and a
/// given `(root_id, kind)` yields at most one action (a write or a delete).
pub fn plan_album_artifacts(
    desired: &[AlbumDesired],
    albums: &BTreeMap<String, AlbumArt>,
    can_delete: bool,
    local: &HashMap<String, LocalFile>,
) -> Vec<Action> {
    let mut actions: Vec<Action> = Vec::new();
    let by_root: BTreeMap<&str, &AlbumDesired> =
        desired.iter().map(|d| (d.root_id.as_str(), d)).collect();

    for d in desired {
        let stored = albums.get(&d.root_id);
        for artifact in [
            d.folder_jpg.as_ref(),
            d.folder_webp.as_ref(),
            d.folder_mp4.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let needs_write = needs_write_drift(
                stored
                    .and_then(|a| a.artifact(artifact.kind))
                    .map(|state| (state.hash.as_str(), state.path.as_str())),
                artifact.hash.as_str(),
                artifact.path.as_str(),
                local,
            );
            if needs_write {
                actions.push(Action::WriteArtifact {
                    kind: artifact.kind,
                    path: artifact.path.clone(),
                    source_url: artifact.source_url.clone(),
                    hash: artifact.hash.clone(),
                    owner_id: d.root_id.clone(),
                    content: None,
                });
            }
        }
    }

    // Deletes route through the album gate: nothing is removed unless the shared
    // deletion verdict holds and the stored path is non-empty.
    for (root_id, art) in albums {
        for (kind, state) in album_artifacts(art) {
            let desired_here = by_root
                .get(root_id.as_str())
                .is_some_and(|d| album_desires_kind(d, kind));
            if !desired_here
                && let Some(action) =
                    delete_album_artifact_action(root_id, kind, &state.path, can_delete)
            {
                actions.push(action);
            }
        }
    }

    actions.sort_by(|a, b| album_action_key(a).cmp(&album_action_key(b)));
    actions
}

/// The folder-art artifacts an album currently stores, paired with their kind,
/// in a stable order.
fn album_artifacts(art: &AlbumArt) -> Vec<(ArtifactKind, &ArtifactState)> {
    let mut out = Vec::new();
    if let Some(state) = &art.folder_jpg {
        out.push((ArtifactKind::FolderJpg, state));
    }
    if let Some(state) = &art.folder_webp {
        out.push((ArtifactKind::FolderWebp, state));
    }
    if let Some(state) = &art.folder_mp4 {
        out.push((ArtifactKind::FolderMp4, state));
    }
    out
}

/// Whether an [`AlbumDesired`] desires the given folder-art kind this run.
fn album_desires_kind(d: &AlbumDesired, kind: ArtifactKind) -> bool {
    match kind {
        ArtifactKind::FolderJpg => d.folder_jpg.is_some(),
        ArtifactKind::FolderWebp => d.folder_webp.is_some(),
        ArtifactKind::FolderMp4 => d.folder_mp4.is_some(),
        ArtifactKind::CoverJpg
        | ArtifactKind::CoverWebp
        | ArtifactKind::DetailsTxt
        | ArtifactKind::LyricsTxt
        | ArtifactKind::Lrc
        | ArtifactKind::VideoMp4
        | ArtifactKind::Playlist => false,
    }
}

/// The `(root_id, kind)` sort key for a folder-art action, for deterministic order.
fn album_action_key(action: &Action) -> (&str, ArtifactKind) {
    match action {
        Action::WriteArtifact { owner_id, kind, .. }
        | Action::DeleteArtifact { owner_id, kind, .. } => (owner_id.as_str(), *kind),
        _ => ("", ArtifactKind::CoverJpg),
    }
}

/// The gate every album folder-art `DeleteArtifact` passes through.
///
/// The album analogue of [`delete_artifact_action`]. Folder art is owned by the
/// lineage root rather than a manifest clip and has no preserve concept, so the
/// gate is the shared [`deletion_allowed`] verdict (`can_delete`) plus a
/// non-empty `path` (an empty path can never delete the account root). A `None`
/// result means the caller must keep the folder-art file.
pub(crate) fn delete_album_artifact_action(
    owner_id: &str,
    kind: ArtifactKind,
    path: &str,
    can_delete: bool,
) -> Option<Action> {
    if !can_delete || path.is_empty() {
        return None;
    }
    Some(Action::DeleteArtifact {
        kind,
        path: path.to_string(),
        owner_id: owner_id.to_string(),
    })
}
