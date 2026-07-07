use super::*;

// ── SYNC-12: trashed and private ────────────────────────────────

#[test]
fn trashed_but_complete_clip_is_downloadable_yet_still_deletes() {
    // A trashed clip is complete and carries no excluded type or task, so it
    // passes `is_downloadable` (downloadability never screens on trashed).
    // A full run still schedules its deletion, proving the two concerns stay
    // decoupled: the download filter does not suppress the delete signal.
    let mut trashed = clip("a");
    trashed.status = "complete".to_string();
    trashed.is_trashed = true;
    assert!(crate::is_downloadable(&trashed));

    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.clip = trashed;
    d.trashed = true;
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Delete {
            path: "a.flac".to_string(),
            clip_id: "a".to_string(),
        }]
    );
}

#[test]
fn trashed_clip_deletes_local_file() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.trashed = true;
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Delete {
            path: "a.flac".to_string(),
            clip_id: "a".to_string(),
        }]
    );
}

#[test]
fn trashed_clip_not_in_manifest_skips() {
    // Nothing on disk to remove, so trashing is a no-op.
    let manifest = Manifest::new();
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.trashed = true;
    let plan = reconcile(&manifest, &[d], &HashMap::new(), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Skip {
            clip_id: "a".to_string()
        }]
    );
}

#[test]
fn private_clip_is_kept() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.private = true;
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Skip {
            clip_id: "a".to_string()
        }]
    );
}

#[test]
fn private_beats_trashed_never_deletes() {
    // Safety first: a clip that is both trashed and private is kept.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.trashed = true;
    d.private = true;
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.skips(), 1);
}

#[test]
fn copy_held_trashed_clip_is_not_deleted() {
    // SYNC-8: copy always wins, so a trashed clip still held by a copy
    // source is kept and synced rather than deleted.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.modes = vec![SourceMode::Copy];
    d.trashed = true;
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
    assert_eq!(plan.deletes(), 0);
    assert_eq!(
        plan.actions,
        vec![Action::Skip {
            clip_id: "a".to_string()
        }]
    );
}

// ── Deletion pass: absent manifest entries ──────────────────────

#[test]
fn absent_clip_deleted_when_all_mirrors_enumerated() {
    let mut manifest = Manifest::new();
    manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
    let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![Action::Delete {
            path: "gone.flac".to_string(),
            clip_id: "gone".to_string(),
        }]
    );
}

#[test]
fn absent_clip_kept_when_any_mirror_not_enumerated() {
    let mut manifest = Manifest::new();
    manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
    let sources = vec![
        SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: true,
        },
        SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        },
    ];
    let plan = reconcile(&manifest, &[], &HashMap::new(), &sources);
    assert_eq!(plan.deletes(), 0);
    assert_eq!(
        plan.actions,
        vec![Action::Skip {
            clip_id: "gone".to_string()
        }]
    );
}

#[test]
fn empty_listing_cannot_cause_deletion() {
    // A failed or truncated listing presents as a not-fully-enumerated
    // mirror source: absence must never delete in that case.
    let mut manifest = Manifest::new();
    manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
    let sources = vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }];
    let plan = reconcile(&manifest, &[], &HashMap::new(), &sources);
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.skips(), 1);
}

#[test]
fn no_mirror_sources_means_no_deletion() {
    // Copy-only or sourceless runs are additive: nothing is deleted.
    let mut manifest = Manifest::new();
    manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
    let copy_only = vec![SourceStatus {
        mode: SourceMode::Copy,
        fully_enumerated: true,
    }];
    assert_eq!(
        reconcile(&manifest, &[], &HashMap::new(), &copy_only).deletes(),
        0
    );
    assert_eq!(reconcile(&manifest, &[], &HashMap::new(), &[]).deletes(), 0);
}

#[test]
fn copy_source_with_unenumerated_mirror_still_suppresses_deletion() {
    let mut manifest = Manifest::new();
    manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
    let sources = vec![
        SourceStatus {
            mode: SourceMode::Copy,
            fully_enumerated: true,
        },
        SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        },
    ];
    assert_eq!(
        reconcile(&manifest, &[], &HashMap::new(), &sources).deletes(),
        0
    );
}

#[test]
fn area_authoritative_requires_all_conditions() {
    // All three conditions satisfied: authoritative.
    assert!(area_authoritative(true, false, false));
    // Incomplete page drain: not authoritative.
    assert!(!area_authoritative(false, false, false));
    // A member lost to the downloadable filter: not authoritative.
    assert!(!area_authoritative(true, true, false));
    // Narrowed with --limit/--since: not authoritative.
    assert!(!area_authoritative(true, false, true));
    // Multiple conditions: any failure disarms.
    assert!(!area_authoritative(false, true, true));
}

#[test]
fn area_fully_enumerated_applies_empty_mirror_guard() {
    // A non-empty Mirror that fully listed is authoritative.
    assert!(area_fully_enumerated(true, false, SourceMode::Mirror));
    // An empty Mirror is never authoritative (indistinguishable from a drop).
    assert!(!area_fully_enumerated(true, true, SourceMode::Mirror));
    // An empty Copy is still authoritative (it protects nothing).
    assert!(area_fully_enumerated(true, true, SourceMode::Copy));
    // A non-empty Copy is authoritative.
    assert!(area_fully_enumerated(true, false, SourceMode::Copy));
    // A non-authoritative (narrowed/incomplete) area is not enumerated regardless.
    assert!(!area_fully_enumerated(false, false, SourceMode::Mirror));
    assert!(!area_fully_enumerated(false, true, SourceMode::Copy));
}

#[test]
fn narrows_downloads_only_when_no_deletion_and_no_full_library() {
    // Neither deleting nor a full library: narrowing is allowed.
    assert!(narrows_downloads(false, false));
    // Armed deletion: narrowing must not occur (D2).
    assert!(!narrows_downloads(true, false));
    // Full library listed: narrowing regresses the index.
    assert!(!narrows_downloads(false, true));
    // Both: definitely no narrowing.
    assert!(!narrows_downloads(true, true));
}

#[test]
fn narrowing_never_coexists_with_deletion() {
    for can_delete in [false, true] {
        for lib_auth in [false, true] {
            assert!(
                !(narrows_downloads(can_delete, lib_auth) && can_delete),
                "truncate must imply !can_delete"
            );
        }
    }
}

#[test]
fn copy_held_clip_in_desired_is_never_a_deletion_candidate() {
    // SYNC-8 falls out naturally: a copy-held clip is in the desired set,
    // so it is classified there (Skip) and never reaches the delete pass,
    // even while a sibling clip is being deleted.
    let mut manifest = Manifest::new();
    manifest.insert("keep", entry("keep.flac", AudioFormat::Flac, "m", "art"));
    manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
    let mut held = desired("keep", "keep.flac", AudioFormat::Flac, "m", "art");
    held.modes = vec![SourceMode::Copy];
    let local: HashMap<String, LocalFile> = [
        ("keep".to_string(), present(100)),
        ("gone".to_string(), present(100)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &[held], &local, &mirror_ok());
    assert!(plan.actions.contains(&Action::Skip {
        clip_id: "keep".to_string()
    }));
    assert!(plan.actions.contains(&Action::Delete {
        path: "gone.flac".to_string(),
        clip_id: "gone".to_string(),
    }));
    // The copy-held clip is never deleted.
    assert!(
        !plan
            .actions
            .iter()
            .any(|a| matches!(a, Action::Delete { clip_id, .. } if clip_id == "keep"))
    );
}

// ── Item 1: persisted preserve marker ───────────────────────────

#[test]
fn orphan_with_preserve_marker_is_kept() {
    // A copy-held or private clip whose source was deselected is absent from
    // desired, but the persisted marker still protects it from deletion.
    let mut manifest = Manifest::new();
    manifest.insert(
        "gone",
        preserved_entry("gone.flac", AudioFormat::Flac, "m", "art"),
    );
    let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
    assert_eq!(plan.deletes(), 0);
    assert_eq!(
        plan.actions,
        vec![Action::Skip {
            clip_id: "gone".to_string()
        }]
    );
}

#[test]
fn trashed_clip_with_preserve_marker_is_kept() {
    // The marker also defends the trashed path: a preserved entry is never
    // deleted even when the clip is trashed and fully enumerated.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        preserved_entry("a.flac", AudioFormat::Flac, "m", "art"),
    );
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.trashed = true;
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.skips(), 1);
}

// ── Item 2: unified, enumeration-gated delete guard ─────────────

#[test]
fn trashed_clip_kept_when_a_mirror_is_not_enumerated() {
    // The trashed path now obeys the same enumeration guard as orphans.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.trashed = true;
    let sources = vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }];
    let plan = reconcile(&manifest, &[d], &local_present("a"), &sources);
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.skips(), 1);
}

#[test]
fn trashed_clip_kept_when_sources_empty() {
    // With no sources there is no authoritative listing, so even a trashed
    // clip is kept rather than deleted.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.trashed = true;
    let plan = reconcile(&manifest, &[d], &local_present("a"), &[]);
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.skips(), 1);
}

#[test]
fn failed_copy_listing_suppresses_orphan_deletion() {
    // A partial or failed copy listing is as unreliable as a mirror one and
    // must suppress deletes, even with a fully enumerated mirror present.
    let mut manifest = Manifest::new();
    manifest.insert("gone", entry("gone.flac", AudioFormat::Flac, "m", "art"));
    let sources = vec![
        SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: true,
        },
        SourceStatus {
            mode: SourceMode::Copy,
            fully_enumerated: false,
        },
    ];
    let plan = reconcile(&manifest, &[], &HashMap::new(), &sources);
    assert_eq!(plan.deletes(), 0);
}

#[test]
fn failed_copy_listing_suppresses_trashed_deletion() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut d = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    d.trashed = true;
    let sources = vec![
        SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: true,
        },
        SourceStatus {
            mode: SourceMode::Copy,
            fully_enumerated: false,
        },
    ];
    let plan = reconcile(&manifest, &[d], &local_present("a"), &sources);
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.skips(), 1);
}

#[test]
fn empty_path_entry_never_deletes() {
    // A default or partially written manifest entry can have an empty path;
    // that must never become a Delete of the account root.
    let mut manifest = Manifest::new();
    manifest.insert("gone", entry("", AudioFormat::Flac, "m", "art"));
    let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
    assert_eq!(plan.deletes(), 0);
    assert_eq!(
        plan.actions,
        vec![Action::Skip {
            clip_id: "gone".to_string()
        }]
    );
}

// ── Item 3: path aliasing suppression ───────────────────────────

#[test]
fn delete_suppressed_when_path_aliases_rename_target() {
    // Clip "a" renames into the path that absent clip "b" recorded; deleting
    // "b" would clobber the file "a" was just moved to, so it is suppressed.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("old/a.flac", AudioFormat::Flac, "m", "art"));
    manifest.insert("b", entry("new/a.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![desired("a", "new/a.flac", AudioFormat::Flac, "m", "art")];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        ("b".to_string(), present(100)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert!(plan.actions.contains(&Action::Rename {
        from: "old/a.flac".to_string(),
        to: "new/a.flac".to_string(),
    }));
    // No delete targets the renamed-to path.
    assert!(
        !plan
            .actions
            .iter()
            .any(|a| matches!(a, Action::Delete { path, .. } if path == "new/a.flac"))
    );
    assert!(plan.actions.contains(&Action::Skip {
        clip_id: "b".to_string()
    }));
}

#[test]
fn delete_suppressed_when_path_aliases_download_target() {
    // A new clip downloads to the path an absent clip recorded.
    let mut manifest = Manifest::new();
    manifest.insert("b", entry("shared.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![desired("a", "shared.flac", AudioFormat::Flac, "m", "art")];
    let plan = reconcile(&manifest, &d, &HashMap::new(), &mirror_ok());
    assert!(
        !plan
            .actions
            .iter()
            .any(|a| matches!(a, Action::Delete { .. }))
    );
    assert_eq!(plan.downloads(), 1);
}

#[test]
fn delete_suppressed_when_path_case_aliases_download_target() {
    // HIGH (#269): a departed clip's delete path and a kept clip's fresh
    // download target differ only by case. On a case-insensitive filesystem
    // they name one file, so executing the plan would delete the file the
    // same run just wrote. The canonical match must suppress the delete.
    let mut manifest = Manifest::new();
    manifest.insert(
        "b",
        entry("Creator/Song.flac", AudioFormat::Flac, "m", "art"),
    );
    let d = vec![desired(
        "a",
        "Creator/song.flac",
        AudioFormat::Flac,
        "m",
        "art",
    )];
    let plan = reconcile(&manifest, &d, &HashMap::new(), &mirror_ok());
    assert!(
        !plan
            .actions
            .iter()
            .any(|a| matches!(a, Action::Delete { .. })),
        "case-only alias was not suppressed: {:?}",
        plan.actions
    );
    assert_eq!(plan.downloads(), 1);
    assert!(plan.actions.contains(&Action::Skip {
        clip_id: "b".to_string()
    }));
}

#[test]
fn delete_suppressed_when_path_nfc_aliases_download_target() {
    // HIGH (#269): the same guard for NFC vs NFD encodings of one name,
    // which an NFC-folding filesystem (macOS APFS) treats as one file.
    let nfc = "Creator/\u{00e9}toile.flac"; // é as U+00E9
    let nfd = "Creator/e\u{0301}toile.flac"; // é as e + U+0301
    let mut manifest = Manifest::new();
    manifest.insert("b", entry(nfd, AudioFormat::Flac, "m", "art"));
    let d = vec![desired("a", nfc, AudioFormat::Flac, "m", "art")];
    let plan = reconcile(&manifest, &d, &HashMap::new(), &mirror_ok());
    assert!(
        !plan
            .actions
            .iter()
            .any(|a| matches!(a, Action::Delete { .. })),
        "NFC/NFD alias was not suppressed: {:?}",
        plan.actions
    );
    assert_eq!(plan.downloads(), 1);
}

#[test]
fn delete_artifact_suppressed_when_path_aliases_rename_target() {
    // A sidecar delete must never clobber a file a rename just produced this
    // run. A DeleteArtifact whose path equals a Rename's `to` is downgraded
    // to a Skip, exactly as an audio Delete is. Built directly so the
    // collision is explicit and independent of how reconcile derives it.
    let mut actions = vec![
        Action::Rename {
            from: "old/song.flac".to_string(),
            to: "new/cover.jpg".to_string(),
        },
        Action::DeleteArtifact {
            kind: ArtifactKind::CoverJpg,
            path: "new/cover.jpg".to_string(),
            owner_id: "a".to_string(),
        },
    ];
    suppress_path_aliasing(&mut actions);
    // The colliding delete is gone; only its Skip downgrade remains.
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::DeleteArtifact { .. })),
        "a sidecar delete must not alias a rename target"
    );
    assert!(actions.contains(&Action::Skip {
        clip_id: "a".to_string()
    }));
    // The rename target is untouched.
    assert!(actions.contains(&Action::Rename {
        from: "old/song.flac".to_string(),
        to: "new/cover.jpg".to_string(),
    }));
}

#[test]
fn delete_artifact_suppressed_when_path_aliases_write_artifact_target() {
    // The same guard covers every write class: a DeleteArtifact colliding
    // with another artifact's WriteArtifact path is downgraded too.
    let mut actions = vec![
        Action::WriteArtifact {
            kind: ArtifactKind::FolderJpg,
            path: "creator/album/folder.jpg".to_string(),
            source_url: "https://art/large.jpg".to_string(),
            hash: "h".to_string(),
            owner_id: "root".to_string(),
            content: None,
        },
        Action::DeleteArtifact {
            kind: ArtifactKind::FolderJpg,
            path: "creator/album/folder.jpg".to_string(),
            owner_id: "root-old".to_string(),
        },
    ];
    suppress_path_aliasing(&mut actions);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::DeleteArtifact { .. }))
    );
    assert!(actions.contains(&Action::Skip {
        clip_id: "root-old".to_string()
    }));
}

// ── Item 5: aggregation of duplicate desired ids ────────────────

#[test]
fn duplicate_trashed_does_not_defeat_copy_sibling() {
    // The same clip held by a copy source and reported trashed by a mirror:
    // copy wins, so it is kept, not deleted.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut copy_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    copy_entry.modes = vec![SourceMode::Copy];
    let mut trashed_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    trashed_entry.modes = vec![SourceMode::Mirror];
    trashed_entry.trashed = true;
    let plan = reconcile(
        &manifest,
        &[copy_entry, trashed_entry],
        &local_present("a"),
        &mirror_ok(),
    );
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.skips(), 1);
}

#[test]
fn duplicate_trashed_does_not_defeat_private_sibling() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut private_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    private_entry.private = true;
    let mut trashed_entry = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    trashed_entry.trashed = true;
    let plan = reconcile(
        &manifest,
        &[private_entry, trashed_entry],
        &local_present("a"),
        &mirror_ok(),
    );
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.skips(), 1);
}

#[test]
fn duplicate_trashed_deletes_only_when_all_trashed() {
    // Every duplicate trashed and unprotected: a single delete results.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let mut first = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    first.trashed = true;
    let mut second = desired("a", "a.flac", AudioFormat::Flac, "m", "art");
    second.trashed = true;
    let plan = reconcile(
        &manifest,
        &[first, second],
        &local_present("a"),
        &mirror_ok(),
    );
    assert_eq!(plan.deletes(), 1);
}

#[test]
fn absent_private_clip_protected_by_preserve_marker() {
    // Items 1 and 6 together: a private clip deselected from the run is
    // absent from desired, but its preserve marker keeps it across runs.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        preserved_entry("a.flac", AudioFormat::Flac, "m", "art"),
    );
    let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.skips(), 1);
}
