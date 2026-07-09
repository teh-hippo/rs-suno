use super::*;

#[test]
fn stem_path_drift_emits_move() {
    // #141: a stem whose path drifts at an equal hash is relocated with a
    // rename rather than re-rendered or re-fetched.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        entry_with_stems("a", &[("voc", "old.stems/voc.mp3", "h1")]),
    );
    let d = vec![stem_desired(
        "a",
        Some(vec![dstem("voc", "new.stems/voc.mp3", "h1")]),
    )];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        ("old.stems/voc.mp3".to_string(), present(50)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.stem_moves(), 1);
    assert_eq!(plan.stem_writes(), 0);
    assert!(plan.actions.contains(&Action::MoveStem {
        clip_id: "a".to_string(),
        key: "voc".to_string(),
        stem_id: "voc".to_string(),
        from: "old.stems/voc.mp3".to_string(),
        to: "new.stems/voc.mp3".to_string(),
        source_url: "https://cdn1.suno.ai/voc.mp3".to_string(),
        format: StemFormat::Mp3,
        hash: "h1".to_string(),
    }));
}

fn dstem(key: &str, path: &str, hash: &str) -> DesiredStem {
    DesiredStem {
        key: key.to_string(),
        stem_id: key.to_string(),
        path: path.to_string(),
        source_url: format!("https://cdn1.suno.ai/{key}.mp3"),
        format: StemFormat::Mp3,
        hash: hash.to_string(),
    }
}

/// A kept FLAC clip that desires the given (possibly `None`) stem set.
fn stem_desired(id: &str, stems: Option<Vec<DesiredStem>>) -> Desired {
    Desired {
        stems,
        ..desired(id, &format!("{id}.flac"), AudioFormat::Flac, "m", "art")
    }
}

/// A manifest entry for a kept clip carrying the given tracked stems.
fn entry_with_stems(id: &str, stems: &[(&str, &str, &str)]) -> ManifestEntry {
    let mut e = entry(&format!("{id}.flac"), AudioFormat::Flac, "m", "art");
    for (key, path, hash) in stems {
        e.stems.insert(
            key.to_string(),
            ArtifactState {
                path: path.to_string(),
                hash: hash.to_string(),
            },
        );
    }
    e
}

fn stem_writes(plan: &Plan) -> Vec<(&str, &str)> {
    plan.actions
        .iter()
        .filter_map(|a| match a {
            Action::WriteStem { key, path, .. } => Some((key.as_str(), path.as_str())),
            _ => None,
        })
        .collect()
}

fn stem_deletes(plan: &Plan) -> Vec<(&str, &str)> {
    plan.actions
        .iter()
        .filter_map(|a| match a {
            Action::DeleteStem { key, path, .. } => Some((key.as_str(), path.as_str())),
            _ => None,
        })
        .collect()
}

#[test]
fn stems_none_keeps_every_existing_stem() {
    // An indeterminate listing (feature off, has_stem false, or a
    // paged-error) surfaces as `None`: no stem is written or deleted.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        entry_with_stems(
            "a",
            &[
                ("voc", "a.stems/voc.mp3", "h1"),
                ("drm", "a.stems/drm.mp3", "h2"),
            ],
        ),
    );
    let d = vec![stem_desired("a", None)];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.stem_writes(), 0);
    assert_eq!(plan.stem_deletes(), 0);
}

#[test]
fn stems_authoritative_writes_missing_stems() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![stem_desired(
        "a",
        Some(vec![
            dstem("voc", "a.stems/voc.mp3", "h1"),
            dstem("drm", "a.stems/drm.mp3", "h2"),
        ]),
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(
        stem_writes(&plan),
        vec![("voc", "a.stems/voc.mp3"), ("drm", "a.stems/drm.mp3")]
    );
    assert_eq!(plan.stem_deletes(), 0);
}

#[test]
fn stems_authoritative_rewrites_only_on_hash_or_path_drift() {
    let mut manifest = Manifest::new();
    // voc unchanged, drm hash drift, bas path drift (song moved).
    manifest.insert(
        "a",
        entry_with_stems(
            "a",
            &[
                ("voc", "a.stems/voc.mp3", "h1"),
                ("drm", "a.stems/drm.mp3", "h2"),
                ("bas", "old.stems/bas.mp3", "h3"),
            ],
        ),
    );
    let d = vec![stem_desired(
        "a",
        Some(vec![
            dstem("voc", "a.stems/voc.mp3", "h1"),     // unchanged
            dstem("drm", "a.stems/drm.mp3", "h2-new"), // hash drift
            dstem("bas", "a.stems/bas.mp3", "h3"),     // path drift
        ]),
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(
        stem_writes(&plan),
        vec![("drm", "a.stems/drm.mp3"), ("bas", "a.stems/bas.mp3")]
    );
    assert_eq!(plan.stem_deletes(), 0);
}

#[test]
fn stems_authoritative_removes_a_stem_absent_from_the_set() {
    // drm is gone from the authoritative listing, so it is delete-reconciled
    // through the shared gate; voc (still present) is untouched.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        entry_with_stems(
            "a",
            &[
                ("voc", "a.stems/voc.mp3", "h1"),
                ("drm", "a.stems/drm.mp3", "h2"),
            ],
        ),
    );
    let d = vec![stem_desired(
        "a",
        Some(vec![dstem("voc", "a.stems/voc.mp3", "h1")]),
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.stem_writes(), 0);
    assert_eq!(stem_deletes(&plan), vec![("drm", "a.stems/drm.mp3")]);
}

#[test]
fn stems_removal_needs_deletion_allowed() {
    // The same authoritative-omission case, but deletion is not allowed this
    // run (no fully-enumerated mirror). The stem is KEPT, never deleted.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        entry_with_stems(
            "a",
            &[
                ("voc", "a.stems/voc.mp3", "h1"),
                ("drm", "a.stems/drm.mp3", "h2"),
            ],
        ),
    );
    let d = vec![stem_desired(
        "a",
        Some(vec![dstem("voc", "a.stems/voc.mp3", "h1")]),
    )];

    let incomplete = vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }];
    assert_eq!(
        reconcile(&manifest, &d, &local_present("a"), &incomplete).stem_deletes(),
        0
    );

    let copy_only = vec![SourceStatus {
        mode: SourceMode::Copy,
        fully_enumerated: true,
    }];
    assert_eq!(
        reconcile(&manifest, &d, &local_present("a"), &copy_only).stem_deletes(),
        0
    );
}

#[test]
fn stems_removal_skipped_for_preserved_or_protected_clip() {
    let mut manifest = Manifest::new();
    let mut e = entry_with_stems(
        "a",
        &[
            ("voc", "a.stems/voc.mp3", "h1"),
            ("drm", "a.stems/drm.mp3", "h2"),
        ],
    );
    e.preserve = true;
    manifest.insert("a", e);
    let authoritative = Some(vec![dstem("voc", "a.stems/voc.mp3", "h1")]);

    // preserve marker wins: no stem delete.
    let d = vec![stem_desired("a", authoritative.clone())];
    assert_eq!(
        reconcile(&manifest, &d, &local_present("a"), &mirror_ok()).stem_deletes(),
        0
    );

    // A copy-held clip this run also keeps all stems (protected_now).
    let mut manifest2 = Manifest::new();
    manifest2.insert(
        "a",
        entry_with_stems(
            "a",
            &[
                ("voc", "a.stems/voc.mp3", "h1"),
                ("drm", "a.stems/drm.mp3", "h2"),
            ],
        ),
    );
    let held = Desired {
        modes: vec![SourceMode::Mirror, SourceMode::Copy],
        stems: authoritative,
        ..desired("a", "a.flac", AudioFormat::Flac, "m", "art")
    };
    assert_eq!(
        reconcile(&manifest2, &[held], &local_present("a"), &mirror_ok()).stem_deletes(),
        0
    );
}

#[test]
fn stems_are_co_deleted_when_the_song_is_trashed() {
    // A trashed clip's audio is deleted; its stems must be co-deleted so the
    // `.stems` folder is not orphaned (no stranding).
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        entry_with_stems(
            "a",
            &[
                ("voc", "a.stems/voc.mp3", "h1"),
                ("drm", "a.stems/drm.mp3", "h2"),
            ],
        ),
    );
    let trashed = Desired {
        trashed: true,
        ..desired("a", "a.flac", AudioFormat::Flac, "m", "art")
    };
    let plan = reconcile(&manifest, &[trashed], &local_present("a"), &mirror_ok());
    assert_eq!(plan.deletes(), 1, "the trashed audio is deleted");
    let mut deleted: Vec<&str> = stem_deletes(&plan).into_iter().map(|(k, _)| k).collect();
    deleted.sort_unstable();
    assert_eq!(deleted, vec!["drm", "voc"], "both stems co-deleted");
}

#[test]
fn stems_are_co_deleted_for_an_absent_clip() {
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        entry_with_stems("a", &[("voc", "a.stems/voc.mp3", "h1")]),
    );
    // Desired is empty: clip "a" left every source and is deleted.
    let plan = reconcile(&manifest, &[], &local_present("a"), &mirror_ok());
    assert_eq!(plan.deletes(), 1);
    assert_eq!(stem_deletes(&plan), vec![("voc", "a.stems/voc.mp3")]);
}

#[test]
fn stems_are_kept_when_absent_clip_listing_is_incomplete() {
    // SYNC-9: an unreliable listing deletes nothing, stems included.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        entry_with_stems("a", &[("voc", "a.stems/voc.mp3", "h1")]),
    );
    let incomplete = vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }];
    let plan = reconcile(&manifest, &[], &HashMap::new(), &incomplete);
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.stem_deletes(), 0);
}

#[test]
fn stem_delete_is_suppressed_when_it_aliases_a_stem_write() {
    // A prior stem at a path is being removed, while a different stem is
    // written to that same path this run (a re-key at a stable path). The
    // delete must be downgraded so it can never clobber the fresh write.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        entry_with_stems("a", &[("old", "a.stems/mix.mp3", "h1")]),
    );
    let d = vec![stem_desired(
        "a",
        Some(vec![dstem("new", "a.stems/mix.mp3", "h2")]),
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    // The new stem is written to the shared path; the old key's delete of the
    // same path is suppressed (no DeleteStem survives for that path).
    assert_eq!(stem_writes(&plan), vec![("new", "a.stems/mix.mp3")]);
    assert!(
        !plan.actions.iter().any(|a| matches!(
            a,
            Action::DeleteStem { path, .. } if path == "a.stems/mix.mp3"
        )),
        "a stem delete must never alias a stem write target"
    );
}

// #355: with no authoritative stem listing (`d.stems == None`), a retitle (or a
// pre-fix strand) relocates every tracked stem to the current `.stems` folder
// with a folder-only reparent, emitting only MoveStem and never a delete.

/// A kept FLAC entry at `audio` carrying the given tracked stems.
fn stems_entry_at(audio: &str, stems: &[(&str, &str, &str)]) -> ManifestEntry {
    let mut e = entry(audio, AudioFormat::Flac, "m", "art");
    for (key, path, hash) in stems {
        e.stems.insert(
            key.to_string(),
            ArtifactState {
                path: path.to_string(),
                hash: hash.to_string(),
            },
        );
    }
    e
}

/// A kept clip whose audio drifts to `new_path`, with no authoritative stems.
fn stems_none_at(id: &str, new_path: &str) -> Desired {
    Desired {
        stems: None,
        ..desired(id, new_path, AudioFormat::Flac, "m", "art")
    }
}

#[test]
fn stems_none_relocates_stranded_stems_on_retitle() {
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        stems_entry_at(
            "Old.flac",
            &[
                ("voc", "Old.stems/voc.mp3", "h1"),
                ("drm", "Old.stems/drm.mp3", "h2"),
            ],
        ),
    );
    let d = vec![stems_none_at("a", "New.flac")];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        ("Old.stems/voc.mp3".to_string(), present(50)),
        ("Old.stems/drm.mp3".to_string(), present(50)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.stem_moves(), 2);
    assert_eq!(plan.stem_writes(), 0);
    assert_eq!(plan.stem_deletes(), 0);
    assert!(plan.actions.contains(&Action::MoveStem {
        clip_id: "a".to_string(),
        key: "voc".to_string(),
        stem_id: String::new(),
        from: "Old.stems/voc.mp3".to_string(),
        to: "New.stems/voc.mp3".to_string(),
        source_url: String::new(),
        format: StemFormat::Mp3,
        hash: "h1".to_string(),
    }));
    assert!(plan.actions.contains(&Action::MoveStem {
        clip_id: "a".to_string(),
        key: "drm".to_string(),
        stem_id: String::new(),
        from: "Old.stems/drm.mp3".to_string(),
        to: "New.stems/drm.mp3".to_string(),
        source_url: String::new(),
        format: StemFormat::Mp3,
        hash: "h2".to_string(),
    }));
}

#[test]
fn stems_none_relocation_skipped_when_old_stem_absent() {
    // No source_url or stem_id is known, so a vanished stem cannot be
    // re-rendered: skip cleanly, never write, never delete.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        stems_entry_at("Old.flac", &[("voc", "Old.stems/voc.mp3", "h1")]),
    );
    let d = vec![stems_none_at("a", "New.flac")];
    // Only the audio is present on disk.
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.stem_moves(), 0);
    assert_eq!(plan.stem_writes(), 0);
    assert_eq!(plan.stem_deletes(), 0);
}

#[test]
fn stems_none_no_relocation_without_retitle() {
    // Stems already sit at the current base and the audio is stable: idempotent,
    // no move. Complements `stems_none_keeps_every_existing_stem`.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        stems_entry_at("a.flac", &[("voc", "a.stems/voc.mp3", "h1")]),
    );
    let d = vec![stem_desired("a", None)];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        ("a.stems/voc.mp3".to_string(), present(50)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.stem_moves(), 0);
    assert_eq!(plan.stem_writes(), 0);
    assert_eq!(plan.stem_deletes(), 0);
}

#[test]
fn stems_none_not_relocated_for_trashed_clip() {
    // A trashed clip kept in place (delete gate refuses) with a drifted d.path
    // must NOT have its stems relocated to the new base (strand inversion).
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        stems_entry_at("Old.flac", &[("voc", "Old.stems/voc.mp3", "h1")]),
    );
    let d = vec![Desired {
        trashed: true,
        ..stems_none_at("a", "New.flac")
    }];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        ("Old.stems/voc.mp3".to_string(), present(50)),
    ]
    .into_iter()
    .collect();
    let not_enumerated = vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }];
    let plan = reconcile(&manifest, &d, &local, &not_enumerated);
    assert_eq!(plan.stem_moves(), 0, "no strand inversion");
    assert_eq!(plan.stem_deletes(), 0);
    assert_eq!(plan.deletes(), 0);
}

#[test]
fn two_clip_title_swap_with_stems_is_clobber_safe() {
    // Clips a and b swap titles, each with a tracked stem. a's stem reparents
    // onto b's held stem path and vice versa; the clobber backstop suppresses
    // both, so no `.stems` file is targeted onto another live clip's path and
    // none is lost.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        stems_entry_at("A.flac", &[("voc", "A.stems/voc.mp3", "h1")]),
    );
    manifest.insert(
        "b",
        stems_entry_at("B.flac", &[("voc", "B.stems/voc.mp3", "h2")]),
    );
    // a takes B's base, b takes A's base.
    let d = vec![stems_none_at("a", "B.flac"), stems_none_at("b", "A.flac")];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        ("b".to_string(), present(100)),
        ("A.stems/voc.mp3".to_string(), present(50)),
        ("B.stems/voc.mp3".to_string(), present(50)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(
        plan.stem_moves(),
        0,
        "both colliding stem moves are suppressed to Skip"
    );
    assert_eq!(plan.stem_deletes(), 0, "no stem is lost");
}
