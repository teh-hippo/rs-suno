use super::*;

// ── Per-clip classification ─────────────────────────────────────

#[test]
fn not_in_manifest_downloads() {
    let manifest = Manifest::new();
    let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "art")];
    let plan = reconcile(&manifest, &d, &HashMap::new(), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Download {
            clip: clip("a"),
            lineage: lineage("a"),
            path: "a.flac".to_string(),
            format: AudioFormat::Flac,
        }]
    );
}

#[test]
fn unchanged_clip_skips() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "art")];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Skip {
            clip_id: "a".to_string()
        }]
    );
}

#[test]
fn nested_manifest_path_reconciles_without_rename_or_delete() {
    // Deletion-safety pin (#236). A manifest written by a prior run stores
    // forward-slash paths (rel_to_string is '/'-only on every OS). Recomputing
    // the desired state with build_desired must reproduce byte-identical
    // paths, so reconcile sees no drift and emits neither a Rename nor a
    // Delete — a Windows '\'-separator regression would surface here as a
    // spurious Rename that could strand the prior file.
    let clip = Clip {
        id: "clipaaaa-1234".to_owned(),
        title: "Song".to_owned(),
        display_name: "alice".to_owned(),
        image_large_url: "https://art.suno.ai/clipaaaa-1234/large.jpg".to_owned(),
        ..Clip::default()
    };
    let clips = [&clip];
    let modes: HashMap<String, Vec<SourceMode>> = [(clip.id.clone(), vec![SourceMode::Mirror])]
        .into_iter()
        .collect();
    let desired = crate::desired::build_desired(
        &clips,
        AudioFormat::Flac,
        &modes,
        &HashMap::new(),
        &BTreeSet::new(),
        crate::desired::ArtifactToggles::default(),
        &crate::naming::NamingConfig::default(),
    );
    let d = &desired[0];

    // The forward-slash form a prior run would have stored for every path.
    let stored_audio = d.path.replace('\\', "/");
    assert!(
        !stored_audio.contains('\\') && stored_audio.contains('/'),
        "expected a nested forward-slash path, got {stored_audio}"
    );
    let cover = d
        .artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::CoverJpg)
        .expect("an art-bearing clip yields a cover.jpg");

    let mut manifest = Manifest::new();
    manifest.insert(
        clip.id.clone(),
        ManifestEntry {
            path: stored_audio.clone(),
            format: AudioFormat::Flac,
            meta_hash: d.meta_hash.clone(),
            art_hash: d.art_hash.clone(),
            size: 100,
            cover_jpg: Some(ArtifactState {
                path: cover.path.replace('\\', "/"),
                hash: cover.hash.clone(),
            }),
            ..Default::default()
        },
    );
    let local: HashMap<String, LocalFile> = [(clip.id.clone(), present(100))].into_iter().collect();

    let plan = reconcile(&manifest, &desired, &local, &mirror_ok());

    assert_eq!(
        plan.renames(),
        0,
        "the recomputed path drifted from the stored forward-slash path"
    );
    assert_eq!(plan.deletes(), 0, "no clip should be deleted");
    assert_eq!(plan.reformats(), 0);
    assert_eq!(plan.downloads(), 0);
    assert_eq!(plan.retags(), 0);
    assert_eq!(plan.artifact_writes(), 0, "the cover.jpg drifted");
    assert_eq!(plan.artifact_moves(), 0);
    assert_eq!(plan.skips(), 1, "the unchanged clip is skipped");
}

#[test]
fn meta_change_retags_in_place() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "old", "art"));
    let d = vec![desired("a", "a.flac", AudioFormat::Flac, "new", "art")];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Retag {
            clip: clip("a"),
            lineage: lineage("a"),
            path: "a.flac".to_string(),
        }]
    );
}

#[test]
fn art_change_retags_in_place() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "old-art"));
    let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "new-art")];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Retag {
            clip: clip("a"),
            lineage: lineage("a"),
            path: "a.flac".to_string(),
        }]
    );
}

#[test]
fn rename_when_path_changes() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("old/a.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![desired("a", "new/a.flac", AudioFormat::Flac, "m", "art")];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Rename {
            from: "old/a.flac".to_string(),
            to: "new/a.flac".to_string(),
        }]
    );
}

#[test]
fn case_only_path_change_is_not_a_rename() {
    // MEDIUM (#269): a same-clip path that changed only by case (or NFC/NFD)
    // between runs names one file on a case-insensitive or NFC-folding
    // filesystem, so it must not emit a rename-onto-self.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        entry("Creator/Song.flac", AudioFormat::Flac, "m", "art"),
    );
    let d = vec![desired(
        "a",
        "Creator/song.flac",
        AudioFormat::Flac,
        "m",
        "art",
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Skip {
            clip_id: "a".to_string()
        }]
    );
}

#[test]
fn case_only_path_change_with_meta_drift_retags_in_place() {
    // The canonical-equal path still retags when metadata drifted, at the
    // existing (old-cased) path, with no rename.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        entry("Creator/Song.flac", AudioFormat::Flac, "old", "art"),
    );
    let d = vec![desired(
        "a",
        "Creator/song.flac",
        AudioFormat::Flac,
        "new",
        "art",
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Retag {
            clip: clip("a"),
            lineage: lineage("a"),
            path: "Creator/Song.flac".to_string(),
        }]
    );
}

#[test]
fn rename_with_meta_change_also_retags() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("old/a.flac", AudioFormat::Flac, "old", "art"));
    let d = vec![desired("a", "new/a.flac", AudioFormat::Flac, "new", "art")];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![
            Action::Rename {
                from: "old/a.flac".to_string(),
                to: "new/a.flac".to_string(),
            },
            Action::Retag {
                clip: clip("a"),
                lineage: lineage("a"),
                path: "new/a.flac".to_string(),
            },
        ]
    );
}

#[test]
fn bulk_album_rename_moves_and_retags_without_redownload() {
    // Renaming an album (a manual override) changes both the folder path and
    // the ALBUM tag/hash for every member clip. Reconcile must emit a Rename
    // (a filesystem move) plus an in-place Retag per clip, and NEVER a
    // Download: deletion safety holds (no Delete) and no audio is re-fetched.
    let mut manifest = Manifest::new();
    for id in ["a", "b", "c"] {
        manifest.insert(
            id,
            entry(
                &format!("Creator/Old Album/{id}.flac"),
                AudioFormat::Flac,
                "old-meta",
                "art",
            ),
        );
    }
    let d: Vec<Desired> = ["a", "b", "c"]
        .iter()
        .map(|id| {
            desired(
                id,
                &format!("Creator/New Album/{id}.flac"),
                AudioFormat::Flac,
                "new-meta",
                "art",
            )
        })
        .collect();
    let local: HashMap<String, LocalFile> = ["a", "b", "c"]
        .iter()
        .map(|id| (id.to_string(), present(100)))
        .collect();

    let plan = reconcile(&manifest, &d, &local, &mirror_ok());

    assert_eq!(plan.renames(), 3, "every member folder move is a rename");
    assert_eq!(
        plan.retags(),
        3,
        "the album tag change retags each in place"
    );
    assert_eq!(
        plan.downloads(),
        0,
        "an album rename must never re-download"
    );
    assert_eq!(
        plan.deletes(),
        0,
        "deletion safety: a rename deletes nothing"
    );
    for id in ["a", "b", "c"] {
        assert!(plan.actions.contains(&Action::Rename {
            from: format!("Creator/Old Album/{id}.flac"),
            to: format!("Creator/New Album/{id}.flac"),
        }));
    }
}

#[test]
fn mis_rooted_clip_moves_never_deletes_even_when_deletion_is_armed() {
    // Deletion safety: if a clip's resolved root changes between runs (its
    // album folder moves from {root A} to {root B}), reconcile must relocate
    // the file with a Rename, never Delete the old copy and re-download.
    // This holds with deletion fully armed (mirror_ok => can_delete), so a
    // future clip_roots-driven root shift can never arm an audio delete.
    let mut manifest = Manifest::new();
    manifest.insert(
        "child",
        entry("Creator/Root A/child.flac", AudioFormat::Flac, "m", "art"),
    );
    let d = vec![desired(
        "child",
        "Creator/Root B/child.flac",
        AudioFormat::Flac,
        "m",
        "art",
    )];
    let plan = reconcile(&manifest, &d, &local_present("child"), &mirror_ok());

    assert_eq!(
        plan.actions,
        vec![Action::Rename {
            from: "Creator/Root A/child.flac".to_string(),
            to: "Creator/Root B/child.flac".to_string(),
        }],
        "a mis-rooted clip is moved, not deleted or re-downloaded"
    );
    assert_eq!(
        plan.deletes(),
        0,
        "deletion safety: a re-root deletes nothing"
    );
    assert_eq!(plan.downloads(), 0, "a re-root never re-fetches audio");
}

#[test]
fn format_change_reformats() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![desired("a", "a.mp3", AudioFormat::Mp3, "m", "art")];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Reformat {
            clip: clip("a"),
            path: "a.mp3".to_string(),
            from_path: "a.flac".to_string(),
            from: AudioFormat::Flac,
            to: AudioFormat::Mp3,
        }]
    );
}

#[test]
fn format_change_takes_precedence_over_rename_and_retag() {
    // Format, path, and metadata all changed at once: a single reformat
    // replaces the file, so no separate rename or retag is emitted.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        entry("old/a.flac", AudioFormat::Flac, "old", "old-art"),
    );
    let d = vec![desired(
        "a",
        "new/a.mp3",
        AudioFormat::Mp3,
        "new",
        "new-art",
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.reformats(), 1);
    assert_eq!(plan.renames(), 0);
    assert_eq!(plan.retags(), 0);
}

// ── SYNC-10: zero-length / missing local file ───────────────────

#[test]
fn zero_length_file_downloads_even_when_hashes_match() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let local: HashMap<String, LocalFile> = [(
        "a".to_string(),
        LocalFile {
            exists: true,
            size: 0,
        },
    )]
    .into_iter()
    .collect();
    let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "art")];
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.downloads(), 1);
    assert_eq!(plan.skips(), 0);
}

#[test]
fn missing_file_downloads_even_when_hashes_match() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let local: HashMap<String, LocalFile> = [(
        "a".to_string(),
        LocalFile {
            exists: false,
            size: 0,
        },
    )]
    .into_iter()
    .collect();
    let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "art")];
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.downloads(), 1);
}

#[test]
fn absent_local_probe_treated_as_missing() {
    // A manifest clip with no probe entry is conservatively re-downloaded.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![desired("a", "a.flac", AudioFormat::Flac, "m", "art")];
    let plan = reconcile(&manifest, &d, &HashMap::new(), &mirror_ok());
    assert_eq!(plan.downloads(), 1);
}

#[test]
fn missing_file_download_wins_over_format_difference() {
    // A missing file is re-downloaded directly in the desired format rather
    // than reformatted from a file that is not there.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let local: HashMap<String, LocalFile> = [(
        "a".to_string(),
        LocalFile {
            exists: false,
            size: 0,
        },
    )]
    .into_iter()
    .collect();
    let d = vec![desired("a", "a.mp3", AudioFormat::Mp3, "m", "art")];
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.downloads(), 1);
    assert_eq!(plan.reformats(), 0);
}

#[test]
fn duplicate_desired_unions_modes() {
    // Mirror and copy entries for one id aggregate to a copy-held clip.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut mirror_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    mirror_entry.modes = vec![SourceMode::Mirror];
    mirror_entry.trashed = true;
    let mut copy_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    copy_entry.modes = vec![SourceMode::Copy];
    let plan = reconcile(
        &manifest,
        &[mirror_entry, copy_entry],
        &local_present("a"),
        &mirror_ok(),
    );
    // Copy-held wins over the trashed mirror entry, so no delete.
    assert_eq!(plan.deletes(), 0);
}

// ── Item 6: private is deletion-exempt only ─────────────────────

#[test]
fn private_new_clip_downloads() {
    // Private no longer short-circuits to Skip: a missing private clip is
    // downloaded like any other.
    let manifest = Manifest::new();
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.private = true;
    let plan = reconcile(&manifest, &[d], &HashMap::new(), &mirror_ok());
    assert_eq!(plan.downloads(), 1);
}

#[test]
fn private_zero_length_file_redownloads() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let local: HashMap<String, LocalFile> = [(
        "a".to_string(),
        LocalFile {
            exists: true,
            size: 0,
        },
    )]
    .into_iter()
    .collect();
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.private = true;
    let plan = reconcile(&manifest, &[d], &local, &mirror_ok());
    assert_eq!(plan.downloads(), 1);
}

#[test]
fn private_meta_change_retags() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "old", "art"));
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "new", "art");
    d.private = true;
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
    assert_eq!(plan.retags(), 1);
    assert_eq!(plan.deletes(), 0);
}

// ── Determinism and robustness ──────────────────────────────────

#[test]
fn output_is_deterministic_regardless_of_input_order() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    manifest.insert("b", entry("b.flac", AudioFormat::Flac, "old", "art"));
    manifest.insert("z", entry("z.flac", AudioFormat::Flac, "m", "art"));
    let local: HashMap<String, LocalFile> = ["a", "b", "z"]
        .iter()
        .map(|id| (id.to_string(), present(100)))
        .collect();

    let forward = vec![
        desired("a", "a.flac", AudioFormat::Flac, "m", "art"),
        desired("b", "b.flac", AudioFormat::Flac, "new", "art"),
        desired("c", "c.flac", AudioFormat::Flac, "m", "art"),
    ];
    let mut reversed = forward.clone();
    reversed.reverse();

    let p1 = reconcile(&manifest, &forward, &local, &mirror_ok());
    let p2 = reconcile(&manifest, &reversed, &local, &mirror_ok());
    assert_eq!(p1.actions, p2.actions);

    // And the order is clip-id sorted: a (skip), b (retag), c (download),
    // then absent z (delete).
    let ids: Vec<&str> = p1
        .actions
        .iter()
        .map(|a| match a {
            Action::Skip { clip_id } => clip_id.as_str(),
            Action::Retag { clip, .. } => clip.id.as_str(),
            Action::Download { clip, .. } => clip.id.as_str(),
            Action::Delete { clip_id, .. } => clip_id.as_str(),
            Action::Reformat { clip, .. } => clip.id.as_str(),
            Action::Rename { to, .. } => to.as_str(),
            Action::WriteArtifact { owner_id, .. }
            | Action::DeleteArtifact { owner_id, .. }
            | Action::MoveArtifact { owner_id, .. } => owner_id.as_str(),
            Action::WriteStem { clip_id, .. }
            | Action::DeleteStem { clip_id, .. }
            | Action::MoveStem { clip_id, .. } => clip_id.as_str(),
        })
        .collect();
    assert_eq!(ids, ["a", "b", "c", "z"]);
}

#[test]
fn empty_inputs_do_not_panic() {
    let plan = reconcile(&Manifest::new(), &[], &HashMap::new(), &[]);
    assert!(plan.is_empty());
    assert_eq!(plan.len(), 0);
}

#[test]
fn empty_desired_with_full_manifest_deletes_all() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    manifest.insert("b", entry("b.flac", AudioFormat::Flac, "m", "art"));
    let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
    assert_eq!(plan.deletes(), 2);
}

#[test]
fn full_desired_with_empty_manifest_downloads_all() {
    let d = vec![
        desired("a", "a.flac", AudioFormat::Flac, "m", "art"),
        desired("b", "b.flac", AudioFormat::Flac, "m", "art"),
    ];
    let plan = reconcile(&Manifest::new(), &d, &HashMap::new(), &mirror_ok());
    assert_eq!(plan.downloads(), 2);
}

#[test]
fn plan_counts_sum_to_len() {
    let mut manifest = Manifest::new();
    manifest.insert("skip", entry("skip.flac", AudioFormat::Flac, "m", "art"));
    manifest.insert(
        "retag",
        entry("retag.flac", AudioFormat::Flac, "old", "art"),
    );
    manifest.insert(
        "reformat",
        entry("reformat.flac", AudioFormat::Flac, "m", "art"),
    );
    manifest.insert(
        "rename",
        entry("old/rename.flac", AudioFormat::Flac, "m", "art"),
    );
    manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
    let local: HashMap<String, LocalFile> = ["skip", "retag", "reformat", "rename", "gone"]
        .iter()
        .map(|id| (id.to_string(), present(100)))
        .collect();
    let d = vec![
        desired("skip", "skip.flac", AudioFormat::Flac, "m", "art"),
        desired("retag", "retag.flac", AudioFormat::Flac, "new", "art"),
        desired("reformat", "reformat.mp3", AudioFormat::Mp3, "m", "art"),
        desired("rename", "new/rename.flac", AudioFormat::Flac, "m", "art"),
        desired("download", "download.flac", AudioFormat::Flac, "m", "art"),
    ];
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    let summed = plan.downloads()
        + plan.reformats()
        + plan.retags()
        + plan.renames()
        + plan.deletes()
        + plan.skips();
    assert_eq!(summed, plan.len());
    assert_eq!(plan.downloads(), 1);
    assert_eq!(plan.reformats(), 1);
    assert_eq!(plan.retags(), 1);
    assert_eq!(plan.renames(), 1);
    assert_eq!(plan.deletes(), 1);
    assert_eq!(plan.skips(), 1);
}
