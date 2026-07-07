use super::*;

#[test]
fn playlist_missing_on_disk_forces_rewrite() {
    // The playlist store records a matching entry, but the file is absent:
    // the probe must force a WriteArtifact.
    let desired = vec![pl_desired("pl1", "Mix", "Mix.m3u8", "h1")];
    let stored = pl_store(&[("pl1", pl_state("Mix", "Mix.m3u8", "h1"))]);
    let mut local: HashMap<String, LocalFile> = HashMap::new();
    local.insert("Mix.m3u8".to_owned(), LocalFile::default());
    let actions = plan_playlist_artifacts(&desired, &stored, true, true, &local);
    assert_eq!(actions.len(), 1, "missing playlist file must be rewritten");
    assert!(matches!(
        &actions[0],
        Action::WriteArtifact {
            kind: ArtifactKind::Playlist,
            ..
        }
    ));
}

#[test]
fn playlist_present_on_disk_no_churn() {
    // Matching hash+path and the file is present: no write.
    let desired = vec![pl_desired("pl1", "Mix", "Mix.m3u8", "h1")];
    let stored = pl_store(&[("pl1", pl_state("Mix", "Mix.m3u8", "h1"))]);
    let mut local: HashMap<String, LocalFile> = HashMap::new();
    local.insert("Mix.m3u8".to_owned(), present(200));
    let actions = plan_playlist_artifacts(&desired, &stored, true, true, &local);
    assert!(
        actions.is_empty(),
        "present playlist with matching hash must not churn"
    );
}

fn pl_desired(id: &str, name: &str, path: &str, hash: &str) -> PlaylistDesired {
    PlaylistDesired {
        id: id.to_owned(),
        name: name.to_owned(),
        path: path.to_owned(),
        content: format!("#EXTM3U\n#PLAYLIST:{name}\n<{hash}>\n"),
        hash: hash.to_owned(),
    }
}

fn pl_state(name: &str, path: &str, hash: &str) -> PlaylistState {
    PlaylistState {
        name: name.to_owned(),
        path: path.to_owned(),
        hash: hash.to_owned(),
    }
}

fn pl_store(entries: &[(&str, PlaylistState)]) -> BTreeMap<String, PlaylistState> {
    entries
        .iter()
        .map(|(id, state)| ((*id).to_owned(), state.clone()))
        .collect()
}

#[test]
fn playlist_write_emitted_for_a_new_playlist() {
    let desired = vec![pl_desired("pl1", "Road Trip", "Road Trip.m3u8", "h1")];
    let actions = plan_playlist_artifacts(&desired, &BTreeMap::new(), true, true, &HashMap::new());
    assert_eq!(
        actions,
        vec![Action::WriteArtifact {
            kind: ArtifactKind::Playlist,
            path: "Road Trip.m3u8".to_owned(),
            source_url: String::new(),
            hash: "h1".to_owned(),
            owner_id: "pl1".to_owned(),
            content: Some("#EXTM3U\n#PLAYLIST:Road Trip\n<h1>\n".to_owned()),
        }]
    );
}

#[test]
fn playlist_write_emitted_when_hash_changes() {
    // Same id and path, different content hash (a member's title, an order
    // flip, a new path) — the m3u8 is rewritten (B1).
    let desired = vec![pl_desired("pl1", "Mix", "Mix.m3u8", "h2")];
    let stored = pl_store(&[("pl1", pl_state("Mix", "Mix.m3u8", "h1"))]);
    let actions = plan_playlist_artifacts(&desired, &stored, true, true, &HashMap::new());
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::WriteArtifact { hash, owner_id, .. } if hash == "h2" && owner_id == "pl1"
    ));
}

#[test]
fn playlist_unchanged_is_idempotent() {
    let desired = vec![pl_desired("pl1", "Mix", "Mix.m3u8", "h1")];
    let stored = pl_store(&[("pl1", pl_state("Mix", "Mix.m3u8", "h1"))]);
    let actions = plan_playlist_artifacts(&desired, &stored, true, true, &HashMap::new());
    assert!(actions.is_empty(), "an unchanged playlist plans nothing");
}

#[test]
fn playlist_rename_writes_new_and_deletes_old_path() {
    // The playlist was renamed on Suno, so its sanitised path changed: write
    // the new file and delete the old one, both under the full delete gate.
    let desired = vec![pl_desired("pl1", "Summer", "Summer.m3u8", "h2")];
    let stored = pl_store(&[("pl1", pl_state("Spring", "Spring.m3u8", "h1"))]);
    let actions = plan_playlist_artifacts(&desired, &stored, true, true, &HashMap::new());
    assert_eq!(
        actions,
        vec![
            Action::WriteArtifact {
                kind: ArtifactKind::Playlist,
                path: "Summer.m3u8".to_owned(),
                source_url: String::new(),
                hash: "h2".to_owned(),
                owner_id: "pl1".to_owned(),
                content: Some("#EXTM3U\n#PLAYLIST:Summer\n<h2>\n".to_owned()),
            },
            Action::DeleteArtifact {
                kind: ArtifactKind::Playlist,
                path: "Spring.m3u8".to_owned(),
                owner_id: "pl1".to_owned(),
            },
        ]
    );
}

#[test]
fn playlist_rename_keeps_old_file_when_deletes_disallowed() {
    // A rename still writes the new file, but the OLD-path cleanup is a
    // delete and is gated: no can_delete means no removal (B2).
    let desired = vec![pl_desired("pl1", "Summer", "Summer.m3u8", "h2")];
    let stored = pl_store(&[("pl1", pl_state("Spring", "Spring.m3u8", "h1"))]);
    let actions = plan_playlist_artifacts(&desired, &stored, false, true, &HashMap::new());
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::WriteArtifact { path, .. } if path == "Summer.m3u8"
    ));
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::DeleteArtifact { .. })),
        "old path must not be deleted when deletes are disallowed"
    );
}

#[test]
fn playlist_stale_removed_only_under_full_gate() {
    // A stored playlist absent from desired is stale. It is deleted only when
    // BOTH can_delete and list_fully_enumerated hold.
    let stored = pl_store(&[("gone", pl_state("Gone", "Gone.m3u8", "h1"))]);

    let deleted = plan_playlist_artifacts(&[], &stored, true, true, &HashMap::new());
    assert_eq!(
        deleted,
        vec![Action::DeleteArtifact {
            kind: ArtifactKind::Playlist,
            path: "Gone.m3u8".to_owned(),
            owner_id: "gone".to_owned(),
        }]
    );

    // Any gate off → no delete.
    assert!(plan_playlist_artifacts(&[], &stored, false, true, &HashMap::new()).is_empty());
    assert!(plan_playlist_artifacts(&[], &stored, true, false, &HashMap::new()).is_empty());
    assert!(plan_playlist_artifacts(&[], &stored, false, false, &HashMap::new()).is_empty());
}

#[test]
fn b2_failed_list_emits_zero_writes_and_zero_deletes() {
    // B2 BLOCKER: when the /api/playlist/me listing fails, the caller passes
    // an empty desired and list_fully_enumerated=false. Even with a
    // non-empty store and can_delete, NOTHING is planned — every existing
    // .m3u8 is left untouched.
    let stored = pl_store(&[
        ("pl1", pl_state("Mix", "Mix.m3u8", "h1")),
        ("pl2", pl_state("Chill", "Chill.m3u8", "h2")),
    ]);
    let actions = plan_playlist_artifacts(&[], &stored, true, false, &HashMap::new());
    assert!(
        actions.is_empty(),
        "a failed playlist listing must plan zero actions, got {actions:?}"
    );
}

#[test]
fn b2_empty_list_deletes_only_when_fully_enumerated() {
    // An empty desired that contradicts a non-empty store is a genuine
    // wipe ONLY when the listing was fully enumerated (and can_delete). That
    // path IS a mass delete — the CLI cap/confirmation then guards it — but
    // an unreliable listing (not fully enumerated) plans nothing here (B2).
    let stored = pl_store(&[
        ("pl1", pl_state("Mix", "Mix.m3u8", "h1")),
        ("pl2", pl_state("Chill", "Chill.m3u8", "h2")),
    ]);

    // Not fully enumerated: zero deletes (the safety valve).
    assert!(plan_playlist_artifacts(&[], &stored, true, false, &HashMap::new()).is_empty());

    // Fully enumerated and allowed: both are deleted (the caller's cap
    // catches this mass removal).
    let wiped = plan_playlist_artifacts(&[], &stored, true, true, &HashMap::new());
    assert_eq!(
        wiped
            .iter()
            .filter(|a| matches!(a, Action::DeleteArtifact { .. }))
            .count(),
        2
    );
}

#[test]
fn b2_failed_member_playlist_is_untouched_while_others_reconcile() {
    // A playlist whose member fetch failed is excluded upstream from BOTH
    // desired and the stored map handed here, so it is neither rewritten nor
    // treated as stale: its .m3u8 survives while a sibling reconciles.
    // `pl_ok` reconciles; `pl_fail` is simply absent from both maps.
    let desired = vec![pl_desired("pl_ok", "Ok", "Ok.m3u8", "h2")];
    let stored = pl_store(&[("pl_ok", pl_state("Ok", "Ok.m3u8", "h1"))]);
    let actions = plan_playlist_artifacts(&desired, &stored, true, true, &HashMap::new());
    // Only the healthy playlist is rewritten; nothing references pl_fail.
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::WriteArtifact { owner_id, .. } if owner_id == "pl_ok"
    ));
    assert!(
        !actions.iter().any(|a| match a {
            Action::WriteArtifact { owner_id, .. } | Action::DeleteArtifact { owner_id, .. } =>
                owner_id == "pl_fail",
            _ => false,
        }),
        "a protected (failed-member) playlist must have no action"
    );
}

#[test]
fn playlist_rename_collision_downgrades_the_delete() {
    // pl1 renames Old -> Shared.m3u8; pl2 already renders Shared.m3u8 this
    // run. The delete of pl1's old path is fine, but a delete must never
    // alias a write target, so if the OLD path equals another write target
    // it is downgraded. Here we force the collision: pl1's old path is the
    // very path pl2 writes.
    let desired = vec![
        pl_desired("pl1", "Shared", "Shared.m3u8", "h2"),
        pl_desired("pl2", "Shared", "Shared.m3u8", "h3"),
    ];
    let stored = pl_store(&[("pl1", pl_state("Old", "Shared.m3u8", "h1"))]);
    let actions = plan_playlist_artifacts(&desired, &stored, true, true, &HashMap::new());
    // No DeleteArtifact survives against a path some write produces.
    let write_paths: BTreeSet<&str> = actions
        .iter()
        .filter_map(|a| match a {
            Action::WriteArtifact { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    for a in &actions {
        if let Action::DeleteArtifact { path, .. } = a {
            assert!(
                !write_paths.contains(path.as_str()),
                "a playlist delete aliases a write target: {path}"
            );
        }
    }
}
