//! The reconcile unit-test suite: deterministic scenarios over crafted
//! desired/manifest/local inputs asserting the plan, and above all the
//! deletion-safety gates (mirror enumeration, copy/archive wins, private and
//! trashed handling, path-alias suppression) that are the engine's #1 invariant.

use super::*;
use crate::hash::content_hash;

fn clip(id: &str) -> Clip {
    Clip {
        id: id.to_string(),
        title: "Song".to_string(),
        ..Default::default()
    }
}

fn lineage(id: &str) -> LineageContext {
    LineageContext::own_root(&clip(id))
}

fn entry(path: &str, format: AudioFormat, meta: &str, art: &str) -> ManifestEntry {
    ManifestEntry {
        path: path.to_string(),
        format,
        meta_hash: meta.to_string(),
        art_hash: art.to_string(),
        size: 100,
        preserve: false,
        ..Default::default()
    }
}

fn preserved_entry(path: &str, format: AudioFormat, meta: &str, art: &str) -> ManifestEntry {
    ManifestEntry {
        preserve: true,
        ..entry(path, format, meta, art)
    }
}

fn desired(id: &str, path: &str, format: AudioFormat, meta: &str, art: &str) -> Desired {
    Desired {
        clip: clip(id),
        lineage: lineage(id),
        path: path.to_string(),
        format,
        meta_hash: meta.to_string(),
        art_hash: art.to_string(),
        modes: vec![SourceMode::Mirror],
        trashed: false,
        private: false,
        artifacts: Vec::new(),
        stems: None,
    }
}

fn present(size: u64) -> LocalFile {
    LocalFile { exists: true, size }
}

fn local_present(id: &str) -> HashMap<String, LocalFile> {
    [(id.to_string(), present(100))].into_iter().collect()
}

fn mirror_ok() -> Vec<SourceStatus> {
    vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: true,
    }]
}

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

// ── Phase 6: artifact reconcile ─────────────────────────────────

fn cover(path: &str, hash: &str) -> ArtifactState {
    ArtifactState {
        path: path.to_string(),
        hash: hash.to_string(),
    }
}

fn art(kind: ArtifactKind, path: &str, url: &str, hash: &str) -> DesiredArtifact {
    DesiredArtifact {
        kind,
        path: path.to_string(),
        source_url: url.to_string(),
        hash: hash.to_string(),
        content: None,
    }
}

/// A generated text sidecar desired artifact carrying its body inline.
fn text_art(kind: ArtifactKind, path: &str, body: &str) -> DesiredArtifact {
    DesiredArtifact {
        kind,
        path: path.to_string(),
        source_url: String::new(),
        hash: content_hash(body),
        content: Some(body.to_string()),
    }
}

// An unchanged FLAC clip (Skip audio) that desires the given artifacts.
fn desired_arts(id: &str, arts: Vec<DesiredArtifact>) -> Desired {
    Desired {
        artifacts: arts,
        ..desired(id, &format!("{id}.flac"), AudioFormat::Flac, "m", "art")
    }
}

// A manifest entry for an unchanged FLAC clip carrying a cover.jpg sidecar.
fn entry_with_cover_jpg(id: &str, cover_path: &str, cover_hash: &str) -> ManifestEntry {
    ManifestEntry {
        cover_jpg: Some(cover(cover_path, cover_hash)),
        ..entry(&format!("{id}.flac"), AudioFormat::Flac, "m", "art")
    }
}

fn write_artifacts(plan: &Plan) -> Vec<&Action> {
    plan.actions
        .iter()
        .filter(|a| matches!(a, Action::WriteArtifact { .. }))
        .collect()
}

#[test]
fn write_artifact_emitted_when_manifest_lacks_it() {
    // The clip's audio is unchanged (Skip), but the manifest has no cover.jpg
    // slot, so the desired sidecar is written.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::CoverJpg,
            "a/cover.jpg",
            "https://art/a",
            "h1",
        )],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 1);
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(plan.skips(), 1);
    assert_eq!(
        write_artifacts(&plan)[0],
        &Action::WriteArtifact {
            kind: ArtifactKind::CoverJpg,
            path: "a/cover.jpg".to_string(),
            source_url: "https://art/a".to_string(),
            hash: "h1".to_string(),
            owner_id: "a".to_string(),
            content: None,
        }
    );
}

#[test]
fn write_artifact_emitted_when_hash_differs() {
    // The manifest already tracks a cover.jpg, but its stored hash differs
    // from the desired one, so it is rewritten (and never delete-reconciled).
    let mut manifest = Manifest::new();
    manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "old"));
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::CoverJpg,
            "a/cover.jpg",
            "https://art/a",
            "new",
        )],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 1);
    assert_eq!(plan.artifact_deletes(), 0);
    if let Action::WriteArtifact { hash, .. } = write_artifacts(&plan)[0] {
        assert_eq!(hash, "new");
    } else {
        panic!("expected a WriteArtifact");
    }
}

#[test]
fn write_artifact_skipped_when_hash_matches() {
    // Present with a matching hash: no write, no delete.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::CoverJpg,
            "a/cover.jpg",
            "https://art/a",
            "h1",
        )],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 0);
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(
        plan.actions,
        vec![Action::Skip {
            clip_id: "a".to_string()
        }]
    );
}

#[test]
fn removed_kind_cover_is_kept_not_deleted() {
    // The clip is kept but no longer desires a cover.jpg (an empty/transient
    // art URL this run). Covers opt out of removed-kind deletion, so the
    // existing sidecar is KEPT: no DeleteArtifact, no write, just a Skip.
    // This is the empty-art-URL keep the P6 review deferred to P7.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
    let d = vec![desired_arts("a", vec![])];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(plan.artifact_writes(), 0);
    // The audio is untouched and the cover is preserved on disk.
    assert_eq!(plan.deletes(), 0);
    assert_eq!(
        plan.actions,
        vec![Action::Skip {
            clip_id: "a".to_string()
        }]
    );
    assert!(!plan.actions.iter().any(|a| matches!(
        a,
        Action::DeleteArtifact {
            kind: ArtifactKind::CoverJpg,
            ..
        }
    )));
}

#[test]
fn delete_artifact_never_on_incomplete_listing() {
    // Kept clips no longer desiring their covers keep them: covers opt out of
    // removed-kind deletion. An incomplete mirror is a further backstop that
    // forbids every delete (the B2 gate on the co-delete path). Either way, a
    // large manifest of stale sidecars is safe.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
    manifest.insert("b", entry_with_cover_jpg("b", "b/cover.jpg", "h1"));
    let d = vec![desired_arts("a", vec![]), desired_arts("b", vec![])];
    let sources = vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        ("b".to_string(), present(100)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &d, &local, &sources);
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(plan.deletes(), 0);
}

#[test]
fn delete_artifact_never_when_entry_preserved() {
    // A kept clip that stops desiring its cover keeps it (covers opt out of
    // removed-kind deletion); the preserve marker is a further backstop.
    let mut manifest = Manifest::new();
    let preserved = ManifestEntry {
        preserve: true,
        ..entry_with_cover_jpg("a", "a/cover.jpg", "h1")
    };
    manifest.insert("a", preserved);
    let d = vec![desired_arts("a", vec![])];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_deletes(), 0);
}

#[test]
fn co_delete_never_when_path_empty() {
    // The empty-path guard now matters on the co-delete path (covers opt out
    // of removed-kind deletion). An absent clip's audio is deleted, but its
    // sidecar with an empty path must never become a delete of the root.
    let mut manifest = Manifest::new();
    manifest.insert("gone", entry_with_cover_jpg("gone", "", "h1"));
    let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
    assert_eq!(plan.deletes(), 1);
    assert_eq!(plan.artifact_deletes(), 0);
}

#[test]
fn co_delete_absent_clip_deletes_audio_and_cover() {
    // A clip absent from desired is deleted; its cover.jpg is co-deleted
    // under the same gate.
    let mut manifest = Manifest::new();
    manifest.insert("gone", entry_with_cover_jpg("gone", "gone/cover.jpg", "h1"));
    let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
    assert_eq!(plan.deletes(), 1);
    assert_eq!(plan.artifact_deletes(), 1);
    assert!(plan.actions.contains(&Action::Delete {
        path: "gone.flac".to_string(),
        clip_id: "gone".to_string(),
    }));
    assert!(plan.actions.contains(&Action::DeleteArtifact {
        kind: ArtifactKind::CoverJpg,
        path: "gone/cover.jpg".to_string(),
        owner_id: "gone".to_string(),
    }));
}

#[test]
fn co_delete_absent_clip_suppressed_when_not_enumerated() {
    // Neither audio nor sidecar is removed on an incomplete listing.
    let mut manifest = Manifest::new();
    manifest.insert("gone", entry_with_cover_jpg("gone", "gone/cover.jpg", "h1"));
    let sources = vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }];
    let plan = reconcile(&manifest, &[], &HashMap::new(), &sources);
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.artifact_deletes(), 0);
}

#[test]
fn co_delete_trashed_desired_clip_removes_audio_and_cover() {
    // A trashed clip present in desired: audio Delete plus cover co-delete.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
    let mut d = desired_arts("a", vec![]);
    d.trashed = true;
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
    assert_eq!(plan.deletes(), 1);
    assert_eq!(plan.artifact_deletes(), 1);
}

#[test]
fn co_delete_trashed_suppressed_when_not_enumerated() {
    // The trashed co-delete obeys the same enumeration gate as the audio.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
    let mut d = desired_arts("a", vec![]);
    d.trashed = true;
    let sources = vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }];
    let plan = reconcile(&manifest, &[d], &local_present("a"), &sources);
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(plan.skips(), 1);
}

#[test]
fn co_delete_trashed_suppressed_when_preserved() {
    // A preserved, trashed clip keeps both audio and sidecar.
    let mut manifest = Manifest::new();
    let preserved = ManifestEntry {
        preserve: true,
        ..entry_with_cover_jpg("a", "a/cover.jpg", "h1")
    };
    manifest.insert("a", preserved);
    let mut d = desired_arts("a", vec![]);
    d.trashed = true;
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.artifact_deletes(), 0);
}

// ── Issue #15: per-song text sidecars ───────────────────────────

#[test]
fn details_sidecar_written_with_inline_content_when_slot_absent() {
    // The audio is unchanged (Skip) but no details slot exists, so the
    // generated sidecar is written and carries its body inline.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![desired_arts(
        "a",
        vec![text_art(
            ArtifactKind::DetailsTxt,
            "a.details.txt",
            "Title: A\n",
        )],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 1);
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(
        write_artifacts(&plan)[0],
        &Action::WriteArtifact {
            kind: ArtifactKind::DetailsTxt,
            path: "a.details.txt".to_string(),
            source_url: String::new(),
            hash: content_hash("Title: A\n"),
            owner_id: "a".to_string(),
            content: Some("Title: A\n".to_string()),
        }
    );
}

#[test]
fn lrc_sidecar_written_with_inline_content_when_slot_absent() {
    // The audio is unchanged (Skip) but no lrc slot exists, so the generated
    // sidecar is written and carries its body inline. This is the guard that
    // the type system cannot provide: dropping Lrc from is_per_clip_kind
    // would silently never write the file, and only this test would catch it.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let body = "[re:rs-suno]\nla la\n";
    let d = vec![desired_arts(
        "a",
        vec![text_art(ArtifactKind::Lrc, "a.lrc", body)],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 1);
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(
        write_artifacts(&plan)[0],
        &Action::WriteArtifact {
            kind: ArtifactKind::Lrc,
            path: "a.lrc".to_string(),
            source_url: String::new(),
            hash: content_hash(body),
            owner_id: "a".to_string(),
            content: Some(body.to_string()),
        }
    );
}

#[test]
fn text_sidecars_skipped_when_hash_and_path_match() {
    // Present with a matching content hash and path: no write, no delete.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.details_txt = Some(cover("a.details.txt", &content_hash("Title: A\n")));
    e.lyrics_txt = Some(cover("a.lyrics.txt", &content_hash("la la\n")));
    manifest.insert("a", e);
    let d = vec![desired_arts(
        "a",
        vec![
            text_art(ArtifactKind::DetailsTxt, "a.details.txt", "Title: A\n"),
            text_art(ArtifactKind::LyricsTxt, "a.lyrics.txt", "la la\n"),
        ],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 0);
    assert_eq!(plan.artifact_deletes(), 0);
}

#[test]
fn details_rewritten_when_content_hash_differs() {
    // A title change alters the details body, so its content hash drifts and
    // the sidecar is rewritten even though the audio is otherwise unchanged.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.details_txt = Some(cover("a.details.txt", &content_hash("Title: Old\n")));
    manifest.insert("a", e);
    let d = vec![desired_arts(
        "a",
        vec![text_art(
            ArtifactKind::DetailsTxt,
            "a.details.txt",
            "Title: New\n",
        )],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 1);
    assert_eq!(plan.artifact_deletes(), 0);
}

#[test]
fn lyrics_rewritten_when_content_hash_differs_though_meta_unchanged() {
    // The per-sidecar content hash keys on the rendered lyrics independently
    // of the audio's stored meta_hash, so editing the sidecar body rewrites
    // the file with no audio retag even when the meta_hash slot is unchanged.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.lyrics_txt = Some(cover("a.lyrics.txt", &content_hash("old words\n")));
    manifest.insert("a", e);
    let d = vec![desired_arts(
        "a",
        vec![text_art(
            ArtifactKind::LyricsTxt,
            "a.lyrics.txt",
            "new words\n",
        )],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    // The audio meta_hash matches ("m"), so only the sidecar rewrites.
    assert_eq!(plan.artifact_writes(), 1);
    assert_eq!(plan.retags(), 0);
}

#[test]
fn text_sidecar_relocated_when_path_differs() {
    // The audio moved (rename), so the tracked details path drifts and the
    // sidecar is rewritten at the new path even though the content matches.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.details_txt = Some(cover("old/a.details.txt", &content_hash("Title: A\n")));
    manifest.insert("a", e);
    let d = vec![desired_arts(
        "a",
        vec![text_art(
            ArtifactKind::DetailsTxt,
            "new/a.details.txt",
            "Title: A\n",
        )],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 1);
    if let Action::WriteArtifact { path, .. } = write_artifacts(&plan)[0] {
        assert_eq!(path, "new/a.details.txt");
    } else {
        panic!("expected a WriteArtifact");
    }
}

#[test]
fn fetched_sidecar_path_drift_emits_move() {
    // #141: a fetched cover whose bytes are unchanged but whose path drifted
    // (a retitle) is relocated with a rename rather than re-fetched.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.cover_jpg = Some(cover("old/cover.jpg", "arthash"));
    manifest.insert("a", e);
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::CoverJpg,
            "new/cover.jpg",
            "https://art/large.jpg",
            "arthash",
        )],
    )];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        ("old/cover.jpg".to_string(), present(50)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.artifact_moves(), 1);
    assert_eq!(plan.artifact_writes(), 0);
    assert!(plan.actions.contains(&Action::MoveArtifact {
        kind: ArtifactKind::CoverJpg,
        from: "old/cover.jpg".to_string(),
        to: "new/cover.jpg".to_string(),
        source_url: "https://art/large.jpg".to_string(),
        hash: "arthash".to_string(),
        owner_id: "a".to_string(),
    }));
}

#[test]
fn sidecar_hash_drift_emits_write_not_move() {
    // Different bytes must re-fetch, even when the path also drifted.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.cover_jpg = Some(cover("old/cover.jpg", "oldhash"));
    manifest.insert("a", e);
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::CoverJpg,
            "new/cover.jpg",
            "https://art/large.jpg",
            "newhash",
        )],
    )];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        ("old/cover.jpg".to_string(), present(50)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.artifact_moves(), 0);
    assert_eq!(plan.artifact_writes(), 1);
}

#[test]
fn inline_sidecar_path_drift_stays_a_write() {
    // Inline-content kinds (text) rewrite from the in-hand bytes, so a move
    // buys nothing: a path drift stays a WriteArtifact even at an equal hash.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.lyrics_txt = Some(cover("old/a.lyrics.txt", &content_hash("words\n")));
    manifest.insert("a", e);
    let d = vec![desired_arts(
        "a",
        vec![text_art(
            ArtifactKind::LyricsTxt,
            "new/a.lyrics.txt",
            "words\n",
        )],
    )];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        ("old/a.lyrics.txt".to_string(), present(50)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.artifact_moves(), 0);
    assert_eq!(plan.artifact_writes(), 1);
}

#[test]
fn sidecar_move_downgrades_to_write_when_old_file_absent() {
    // Same bytes and a path drift, but the old file is gone: fetch fresh at
    // the new path (a self-heal), never emit a move that cannot rename.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.cover_jpg = Some(cover("old/cover.jpg", "arthash"));
    manifest.insert("a", e);
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::CoverJpg,
            "new/cover.jpg",
            "https://art/large.jpg",
            "arthash",
        )],
    )];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        (
            "old/cover.jpg".to_string(),
            LocalFile {
                exists: false,
                size: 0,
            },
        ),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.artifact_moves(), 0);
    assert_eq!(plan.artifact_writes(), 1);
}

#[test]
fn move_target_suppresses_a_colliding_delete() {
    // A MoveArtifact to a path another manifest entry is having deleted must
    // downgrade that delete, so relocation never clobbers the relocated file.
    let mut manifest = Manifest::new();
    let mut a = entry("a.flac", AudioFormat::Flac, "m", "art");
    a.cover_jpg = Some(cover("old/cover.jpg", "arthash"));
    manifest.insert("a", a);
    // b holds a cover at the path a is moving TO; b's cover is a removed kind
    // this run (feature toggled), so it would be delete-reconciled.
    let mut b = entry("b.flac", AudioFormat::Flac, "m", "art");
    b.details_txt = Some(cover("new/cover.jpg", "bh"));
    manifest.insert("b", b);
    let d = vec![
        desired_arts(
            "a",
            vec![art(
                ArtifactKind::CoverJpg,
                "new/cover.jpg",
                "https://art/large.jpg",
                "arthash",
            )],
        ),
        desired_arts("b", vec![]),
    ];
    let local: HashMap<String, LocalFile> = [
        ("a".to_string(), present(100)),
        ("b".to_string(), present(100)),
        ("old/cover.jpg".to_string(), present(50)),
        ("new/cover.jpg".to_string(), present(50)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.artifact_moves(), 1);
    // The colliding delete of new/cover.jpg is suppressed.
    assert!(!plan.actions.iter().any(|a| matches!(
        a,
        Action::DeleteArtifact { path, .. } if path == "new/cover.jpg"
    )));
}

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

#[test]
fn details_removed_kind_is_deleted_when_feature_off() {
    // DetailsTxt is total, so an absent desired can only mean the feature is
    // off: the stale sidecar is delete-reconciled through the shared gate.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.details_txt = Some(cover("a.details.txt", &content_hash("Title: A\n")));
    manifest.insert("a", e);
    let d = vec![desired_arts("a", vec![])];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_deletes(), 1);
    assert!(plan.actions.contains(&Action::DeleteArtifact {
        kind: ArtifactKind::DetailsTxt,
        path: "a.details.txt".to_string(),
        owner_id: "a".to_string(),
    }));
}

#[test]
fn cover_webp_retired_sidecar_is_deleted_through_the_gate() {
    // The `<track>.webp` sidecar is retired: the animated cover is embedded
    // now, so a desired set never contains CoverWebp. A `.webp` left by an
    // older version is delete-reconciled through the shared gate (unlike
    // CoverJpg, which opts out because its absence can be a transient URL).
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.cover_webp = Some(cover("a.webp", "v1"));
    manifest.insert("a", e);
    let d = vec![desired_arts("a", vec![])];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_deletes(), 1);
    assert!(plan.actions.contains(&Action::DeleteArtifact {
        kind: ArtifactKind::CoverWebp,
        path: "a.webp".to_string(),
        owner_id: "a".to_string(),
    }));
}

#[test]
fn cover_webp_retired_cleanup_respects_the_deletion_gate() {
    // The retirement cleanup is still fully gated: on a run where deletion is
    // disarmed (a partial, non-authoritative listing), the stale `.webp` is
    // KEPT, never dropped on an unsafe listing.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.cover_webp = Some(cover("a.webp", "v1"));
    manifest.insert("a", e);
    let d = vec![desired_arts("a", vec![])];
    let not_enumerated = vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }];
    let plan = reconcile(&manifest, &d, &local_present("a"), &not_enumerated);
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(plan.deletes(), 0);
}

#[test]
fn lyrics_removed_kind_is_kept_not_deleted() {
    // LyricsTxt is partial (absent could be feature-off OR a transient empty
    // lyrics read), so it opts out of removed-kind deletion cover-style: the
    // existing file is KEPT when no lyrics sidecar is desired this run.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.lyrics_txt = Some(cover("a.lyrics.txt", &content_hash("words\n")));
    manifest.insert("a", e);
    let d = vec![desired_arts("a", vec![])];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(plan.deletes(), 0);
}

#[test]
fn lrc_removed_kind_is_kept_not_deleted() {
    // Lrc is partial like LyricsTxt, so it opts out of removed-kind deletion:
    // an existing `.lrc` is KEPT when no lrc sidecar is desired this run.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.lrc = Some(cover("a.lrc", &content_hash("[re:rs-suno]\nwords\n")));
    manifest.insert("a", e);
    let d = vec![desired_arts("a", vec![])];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(plan.deletes(), 0);
}

#[test]
fn video_mp4_removed_kind_is_kept_not_deleted() {
    // VideoMp4 opts out of removed-kind deletion like a cover: a large binary
    // is never deleted merely because the video feature is off this run (or
    // the URL was transiently absent). Only a co-delete removes it.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.video_mp4 = Some(cover("a.mp4", "vid-hash"));
    manifest.insert("a", e);
    let d = vec![desired_arts("a", vec![])];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(plan.deletes(), 0);
}

#[test]
fn video_mp4_written_when_manifest_lacks_it() {
    // A desired VideoMp4 with no manifest slot is written as a fetched binary
    // (no inline content), proving the new kind flows through per-clip planning.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::VideoMp4,
            "a/song.mp4",
            "https://cdn/a/video.mp4",
            "vid-hash",
        )],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 1);
    assert_eq!(
        write_artifacts(&plan)[0],
        &Action::WriteArtifact {
            kind: ArtifactKind::VideoMp4,
            path: "a/song.mp4".to_string(),
            source_url: "https://cdn/a/video.mp4".to_string(),
            hash: "vid-hash".to_string(),
            owner_id: "a".to_string(),
            content: None,
        }
    );
}

#[test]
fn details_removed_kind_not_deleted_on_incomplete_listing() {
    // The removed-kind delete still obeys the enumeration gate: an incomplete
    // mirror forbids removing the stale details sidecar.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.details_txt = Some(cover("a.details.txt", &content_hash("Title: A\n")));
    manifest.insert("a", e);
    let d = vec![desired_arts("a", vec![])];
    let sources = vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }];
    let plan = reconcile(&manifest, &d, &local_present("a"), &sources);
    assert_eq!(plan.artifact_deletes(), 0);
}

#[test]
fn details_removed_kind_not_deleted_when_preserved() {
    // A preserved (private/copy-held) clip keeps its stale details sidecar
    // even when the feature is off this run.
    let mut manifest = Manifest::new();
    let mut e = ManifestEntry {
        preserve: true,
        ..entry("a.flac", AudioFormat::Flac, "m", "art")
    };
    e.details_txt = Some(cover("a.details.txt", &content_hash("Title: A\n")));
    manifest.insert("a", e);
    let d = vec![desired_arts("a", vec![])];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_deletes(), 0);
}

#[test]
fn co_delete_orphan_removes_every_text_sidecar() {
    // An orphaned clip's audio is deleted; ALL its per-clip sidecars must be
    // co-deleted. This fails if `manifest_artifacts` misses a kind, which
    // would strand the file. Guards the single most important #15 wiring.
    let mut manifest = Manifest::new();
    let mut e = entry("gone.flac", AudioFormat::Flac, "m", "art");
    e.cover_jpg = Some(cover("gone/cover.jpg", "h1"));
    e.details_txt = Some(cover("gone.details.txt", &content_hash("Title: G\n")));
    e.lyrics_txt = Some(cover("gone.lyrics.txt", &content_hash("words\n")));
    e.lrc = Some(cover("gone.lrc", &content_hash("[re:rs-suno]\nwords\n")));
    e.video_mp4 = Some(cover("gone/song.mp4", "vid-hash"));
    manifest.insert("gone", e);
    let plan = reconcile(&manifest, &[], &HashMap::new(), &mirror_ok());
    assert_eq!(plan.deletes(), 1);
    assert_eq!(plan.artifact_deletes(), 5);
    for (kind, path) in [
        (ArtifactKind::CoverJpg, "gone/cover.jpg"),
        (ArtifactKind::DetailsTxt, "gone.details.txt"),
        (ArtifactKind::LyricsTxt, "gone.lyrics.txt"),
        (ArtifactKind::Lrc, "gone.lrc"),
        (ArtifactKind::VideoMp4, "gone/song.mp4"),
    ] {
        assert!(
            plan.actions.contains(&Action::DeleteArtifact {
                kind,
                path: path.to_string(),
                owner_id: "gone".to_string(),
            }),
            "missing co-delete for {kind:?}"
        );
    }
}

#[test]
fn co_delete_trashed_removes_every_text_sidecar() {
    // The same co-delete completeness holds on the trashed path.
    let mut manifest = Manifest::new();
    let mut e = entry("a.flac", AudioFormat::Flac, "m", "art");
    e.details_txt = Some(cover("a.details.txt", &content_hash("Title: A\n")));
    e.lyrics_txt = Some(cover("a.lyrics.txt", &content_hash("words\n")));
    manifest.insert("a", e);
    let mut d = desired_arts("a", vec![]);
    d.trashed = true;
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());
    assert_eq!(plan.deletes(), 1);
    assert_eq!(plan.artifact_deletes(), 2);
}

#[test]
fn suppress_downgrades_delete_artifact_colliding_with_write_artifact() {
    // Clip "a" writes a cover to the very path clip "b"'s stale cover holds;
    // deleting it would clobber the freshly written file, so it is dropped.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    manifest.insert("b", entry_with_cover_jpg("b", "shared/cover.jpg", "h1"));
    // "a" writes a new CoverJpg to the shared path; "b" is absent (its cover
    // would be co-deleted from the same path).
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::CoverJpg,
            "shared/cover.jpg",
            "https://art/a",
            "h2",
        )],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 1);
    // The colliding DeleteArtifact is suppressed.
    assert!(
        !plan.actions.iter().any(
            |a| matches!(a, Action::DeleteArtifact { path, .. } if path == "shared/cover.jpg")
        )
    );
    // The audio for "b" is still deleted (different path), just not its cover.
    assert!(plan.actions.contains(&Action::Delete {
        path: "b.flac".to_string(),
        clip_id: "b".to_string(),
    }));
}

#[test]
fn suppress_downgrades_delete_artifact_colliding_with_download() {
    // A fresh clip downloads audio to the path an absent clip's cover holds.
    let mut manifest = Manifest::new();
    manifest.insert("b", entry_with_cover_jpg("b", "shared/x", "h1"));
    let d = vec![desired("a", "shared/x", AudioFormat::Flac, "m", "art")];
    let plan = reconcile(&manifest, &d, &HashMap::new(), &mirror_ok());
    assert_eq!(plan.downloads(), 1);
    assert!(
        !plan
            .actions
            .iter()
            .any(|a| matches!(a, Action::DeleteArtifact { path, .. } if path == "shared/x"))
    );
}

#[test]
fn adding_artifacts_leaves_the_audio_plan_unchanged() {
    // SYNC-8/9/10/12 matrix invariance: the audio actions and plan.deletes()
    // are identical with and without artifacts attached. One absent clip is
    // deleted, one desired clip is kept (Skip), one trashed clip is deleted.
    let build = |with_art: bool| {
        let mut manifest = Manifest::new();
        manifest.insert("keep", entry_with_cover_jpg("keep", "keep/cover.jpg", "h1"));
        manifest.insert("gone", entry_with_cover_jpg("gone", "gone/cover.jpg", "h1"));
        manifest.insert(
            "trash",
            entry_with_cover_jpg("trash", "trash/cover.jpg", "h1"),
        );
        let keep = if with_art {
            desired_arts(
                "keep",
                vec![art(
                    ArtifactKind::CoverJpg,
                    "keep/cover.jpg",
                    "https://art/keep",
                    "h1",
                )],
            )
        } else {
            desired_arts("keep", vec![])
        };
        let mut trash = desired_arts("trash", vec![]);
        trash.trashed = true;
        let local: HashMap<String, LocalFile> = ["keep", "gone", "trash"]
            .iter()
            .map(|id| (id.to_string(), present(100)))
            .collect();
        reconcile(&manifest, &[keep, trash], &local, &mirror_ok())
    };

    let with = build(true);
    let without = build(false);

    // The audio decisions are identical regardless of artifacts.
    let audio = |plan: &Plan| -> Vec<Action> {
        plan.actions
            .iter()
            .filter(|a| {
                !matches!(
                    a,
                    Action::WriteArtifact { .. } | Action::DeleteArtifact { .. }
                )
            })
            .cloned()
            .collect()
    };
    assert_eq!(audio(&with), audio(&without));
    assert_eq!(with.deletes(), without.deletes());
    // gone + trash audio deletes, unaffected by the artifacts.
    assert_eq!(with.deletes(), 2);
    // The `with` run additionally reconciles sidecars: gone + trash covers
    // co-deleted, and keep's cover matches so it is neither written nor
    // deleted.
    assert_eq!(with.artifact_deletes(), 2);
    assert_eq!(with.artifact_writes(), 0);
}

// ── Phase 6 review fixes: protection, path-drift, kind guard ─────

#[test]
fn removed_kind_sidecar_kept_when_clip_is_protected_this_run() {
    // Covers opt out of removed-kind deletion, so a kept clip keeps its cover
    // regardless of protection. This case additionally proves protection is
    // honoured: a private clip and a copy-held clip each keep a removed-kind
    // cover even though the persisted entry is NOT preserve-marked and the
    // mirror is fully enumerated.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
    assert!(!manifest.get("a").unwrap().preserve);

    // Private this run.
    let private = Desired {
        private: true,
        ..desired_arts("a", vec![])
    };
    let plan = reconcile(&manifest, &[private], &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_deletes(), 0);

    // Copy-held this run (modes contains Copy).
    let copy_held = Desired {
        modes: vec![SourceMode::Copy],
        ..desired_arts("a", vec![])
    };
    let plan = reconcile(&manifest, &[copy_held], &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_deletes(), 0);
}

#[test]
fn write_artifact_emitted_when_path_differs_even_if_hash_matches() {
    // The audio moved (new album/name) so the sidecar belongs at a new path;
    // the bytes are unchanged (same hash) but a rewrite at the new path is
    // still required. Reconcile emits no DeleteArtifact for the old path: the
    // executor's WriteArtifact relocates the sidecar (writes new, removes the
    // old copy), so the plan stays a single write.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry_with_cover_jpg("a", "old/cover.jpg", "h1"));
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::CoverJpg,
            "new/cover.jpg",
            "https://art/a",
            "h1",
        )],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 1);
    assert_eq!(plan.artifact_deletes(), 0);
    if let Action::WriteArtifact { path, .. } = write_artifacts(&plan)[0] {
        assert_eq!(path, "new/cover.jpg");
    } else {
        panic!("expected a WriteArtifact");
    }
}

#[test]
fn needs_write_drift_applies_hash_path_and_probe_rules() {
    let local: HashMap<String, LocalFile> = [
        ("ok".to_string(), present(10)),
        ("missing".to_string(), LocalFile::default()),
        ("empty".to_string(), present(0)),
    ]
    .into_iter()
    .collect();

    assert!(needs_write_drift(None, "h1", "ok", &local));
    assert!(!needs_write_drift(Some(("h1", "ok")), "h1", "ok", &local));
    assert!(needs_write_drift(Some(("h0", "ok")), "h1", "ok", &local));
    assert!(needs_write_drift(
        Some(("h1", "missing")),
        "h1",
        "missing",
        &local
    ));
    assert!(needs_write_drift(
        Some(("h1", "empty")),
        "h1",
        "empty",
        &local
    ));
    assert!(!needs_write_drift(
        Some(("h1", "unprobed")),
        "h1",
        "unprobed",
        &local
    ));
}

#[test]
fn per_clip_reconcile_ignores_album_and_library_kinds() {
    // Album/library kinds must never be written per clip (they have no
    // per-song manifest slot, so they would be rewritten every run). A
    // CoverJpg alongside them is still handled.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![desired_arts(
        "a",
        vec![
            art(
                ArtifactKind::FolderJpg,
                "a/folder.jpg",
                "https://art/folder",
                "hf",
            ),
            art(
                ArtifactKind::Playlist,
                "a/list.m3u",
                "https://art/list",
                "hp",
            ),
            art(ArtifactKind::CoverJpg, "a/cover.jpg", "https://art/a", "h1"),
        ],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 1);
    let paths: Vec<&str> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::WriteArtifact { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(paths, vec!["a/cover.jpg"]);
}

#[test]
fn per_clip_reconcile_emits_nothing_for_album_only_artifacts() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.flac", AudioFormat::Flac, "m", "art"));
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::FolderWebp,
            "a/folder.webp",
            "https://art/folder",
            "hf",
        )],
    )];
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(plan.artifact_writes(), 0);
    assert_eq!(plan.artifact_deletes(), 0);
}

// ── Self-heal: missing-on-disk sidecar / folder-art / playlist ──

/// A local probe map that marks `path` as missing (exists=false).
fn local_with_missing(audio_id: &str, missing_path: &str) -> HashMap<String, LocalFile> {
    let mut m = local_present(audio_id);
    m.insert(missing_path.to_owned(), LocalFile::default());
    m
}

/// A local probe map that marks `path` as present (exists=true, size>0).
fn local_with_present_artifact(audio_id: &str, artifact_path: &str) -> HashMap<String, LocalFile> {
    let mut m = local_present(audio_id);
    m.insert(artifact_path.to_owned(), present(50));
    m
}

#[test]
fn sidecar_missing_on_disk_forces_rewrite() {
    // Manifest and desired agree on hash+path, but the file is absent on
    // disk: the probe forces needs_write = true and a WriteArtifact is
    // emitted to self-heal it.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::CoverJpg,
            "a/cover.jpg",
            "https://art/a",
            "h1",
        )],
    )];
    let local = local_with_missing("a", "a/cover.jpg");
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(
        plan.artifact_writes(),
        1,
        "missing sidecar must be rewritten"
    );
    assert_eq!(plan.artifact_deletes(), 0);
}

#[test]
fn sidecar_present_on_disk_with_matching_hash_no_churn() {
    // Same manifest / desired / hash — but the file IS present. No write.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::CoverJpg,
            "a/cover.jpg",
            "https://art/a",
            "h1",
        )],
    )];
    let local = local_with_present_artifact("a", "a/cover.jpg");
    let plan = reconcile(&manifest, &d, &local, &mirror_ok());
    assert_eq!(plan.artifact_writes(), 0, "present sidecar must not churn");
    assert_eq!(plan.artifact_deletes(), 0);
}

#[test]
fn sidecar_probe_absent_falls_back_to_hash_comparison_no_write() {
    // When the artifact path is not in the local map (probe unavailable),
    // the engine falls back to hash/path comparison only. A matching entry
    // must NOT trigger a write, and must NOT trigger a delete.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry_with_cover_jpg("a", "a/cover.jpg", "h1"));
    let d = vec![desired_arts(
        "a",
        vec![art(
            ArtifactKind::CoverJpg,
            "a/cover.jpg",
            "https://art/a",
            "h1",
        )],
    )];
    // local only has the audio entry; cover path is unprobeable.
    let plan = reconcile(&manifest, &d, &local_present("a"), &mirror_ok());
    assert_eq!(
        plan.artifact_writes(),
        0,
        "no write when probe unavailable and hash matches"
    );
    assert_eq!(
        plan.artifact_deletes(),
        0,
        "missing probe must never trigger a delete"
    );
}

#[test]
fn folder_art_missing_on_disk_forces_rewrite() {
    // The album store records a matching folder.jpg, but the file is absent:
    // the probe must force a WriteArtifact.
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", ""),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    let mut local: HashMap<String, LocalFile> = HashMap::new();
    local.insert("c/al/folder.jpg".to_owned(), LocalFile::default());
    let actions = plan_album_artifacts(&desired, &albums, true, &local);
    assert_eq!(actions.len(), 1, "missing folder art must be rewritten");
    assert!(matches!(
        &actions[0],
        Action::WriteArtifact {
            kind: ArtifactKind::FolderJpg,
            ..
        }
    ));
}

#[test]
fn folder_art_present_on_disk_no_churn() {
    // Matching hash+path and the file is present: no write.
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", ""),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    let mut local: HashMap<String, LocalFile> = HashMap::new();
    local.insert("c/al/folder.jpg".to_owned(), present(5000));
    let actions = plan_album_artifacts(&desired, &albums, true, &local);
    assert!(
        actions.is_empty(),
        "present folder art with matching hash must not churn"
    );
}

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

// ── Phase 8: folder art (album-scoped) ──────────────────────────

fn album_clip(id: &str, play_count: u64, created_at: &str, image: &str, video: &str) -> Clip {
    Clip {
        id: id.to_string(),
        title: "Song".to_string(),
        image_large_url: image.to_string(),
        video_cover_url: video.to_string(),
        play_count,
        created_at: created_at.to_string(),
        ..Default::default()
    }
}

fn album_member(clip: Clip, root_id: &str, path: &str) -> Desired {
    let mut lineage = LineageContext::own_root(&clip);
    lineage.root_id = root_id.to_string();
    Desired {
        clip,
        lineage,
        path: path.to_string(),
        format: AudioFormat::Flac,
        meta_hash: "m".to_string(),
        art_hash: "a".to_string(),
        modes: vec![SourceMode::Mirror],
        trashed: false,
        private: false,
        artifacts: Vec::new(),
        stems: None,
    }
}

fn stored(path: &str, hash: &str) -> ArtifactState {
    ArtifactState {
        path: path.to_string(),
        hash: hash.to_string(),
    }
}

#[test]
fn folder_jpg_source_is_most_played() {
    let members = vec![
        album_member(album_clip("a", 5, "t0", "art-a", ""), "root", "c/al/a.flac"),
        album_member(album_clip("b", 9, "t1", "art-b", ""), "root", "c/al/b.flac"),
        album_member(album_clip("c", 2, "t2", "art-c", ""), "root", "c/al/c.flac"),
    ];
    let albums = album_desired(&members, false, false, WebpEncodeSettings::default());
    assert_eq!(albums.len(), 1);
    let jpg = albums[0].folder_jpg.as_ref().unwrap();
    // "b" has the highest play_count, so its art content hash wins.
    assert_eq!(jpg.hash, art_url_hash("art-b"));
    assert_eq!(jpg.source_url, "art-b");
    assert_eq!(jpg.path, "c/al/folder.jpg");
    assert_eq!(jpg.kind, ArtifactKind::FolderJpg);
}

#[test]
fn folder_jpg_tie_breaks_earliest_then_lex_id() {
    // Equal play_count: earliest created_at wins.
    let by_time = vec![
        album_member(album_clip("z", 4, "t2", "art-z", ""), "root", "c/al/z.flac"),
        album_member(album_clip("y", 4, "t0", "art-y", ""), "root", "c/al/y.flac"),
        album_member(album_clip("x", 4, "t1", "art-x", ""), "root", "c/al/x.flac"),
    ];
    let jpg = album_desired(&by_time, false, false, WebpEncodeSettings::default())[0]
        .folder_jpg
        .clone()
        .unwrap();
    assert_eq!(jpg.source_url, "art-y");

    // Equal play_count and created_at: lexicographically smallest id wins.
    let by_id = vec![
        album_member(album_clip("m", 4, "t0", "art-m", ""), "root", "c/al/m.flac"),
        album_member(album_clip("g", 4, "t0", "art-g", ""), "root", "c/al/g.flac"),
    ];
    let jpg = album_desired(&by_id, false, false, WebpEncodeSettings::default())[0]
        .folder_jpg
        .clone()
        .unwrap();
    assert_eq!(jpg.source_url, "art-g");
}

#[test]
fn folder_webp_source_is_first_created_animated() {
    let members = vec![
        album_member(
            album_clip("a", 9, "t2", "art-a", "vid-a"),
            "root",
            "c/al/a.flac",
        ),
        album_member(
            album_clip("b", 1, "t0", "art-b", "vid-b"),
            "root",
            "c/al/b.flac",
        ),
        album_member(album_clip("c", 5, "t1", "art-c", ""), "root", "c/al/c.flac"),
    ];
    let webp = album_desired(&members, true, false, WebpEncodeSettings::default())[0]
        .folder_webp
        .clone()
        .unwrap();
    // "b" is earliest-created with an animated source, regardless of plays.
    assert_eq!(webp.source_url, "vid-b");
    assert_eq!(
        webp.hash,
        webp_art_hash("vid-b", &WebpEncodeSettings::default())
    );
    assert_eq!(webp.path, "c/al/cover.webp");
    assert_eq!(webp.kind, ArtifactKind::FolderWebp);

    // The cover.webp hash folds in the encode settings, so raising quality
    // (or any encode knob) re-transcodes an existing album cover.
    let hi = WebpEncodeSettings {
        quality: 40,
        ..WebpEncodeSettings::default()
    };
    let rehashed = album_desired(&members, true, false, hi)[0]
        .folder_webp
        .clone()
        .unwrap();
    assert_ne!(rehashed.hash, webp.hash);
}

#[test]
fn animated_covers_off_yields_no_folder_webp() {
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", "vid-a"),
        "root",
        "c/al/a.flac",
    )];
    let off = album_desired(&members, false, false, WebpEncodeSettings::default());
    assert!(off[0].folder_webp.is_none());
    let on = album_desired(&members, true, false, WebpEncodeSettings::default());
    assert!(on[0].folder_webp.is_some());
}

#[test]
fn raw_cover_yields_folder_mp4_from_the_webp_source_verbatim() {
    let members = vec![
        album_member(
            album_clip("a", 9, "t2", "art-a", "vid-a"),
            "root",
            "c/al/a.flac",
        ),
        album_member(
            album_clip("b", 1, "t0", "art-b", "vid-b"),
            "root",
            "c/al/b.flac",
        ),
    ];
    // `both`: cover.webp (transcoded) and cover.mp4 (raw) come from the SAME
    // earliest-created animated variant, so they describe one animation. The
    // raw cover keeps the `video_cover_url` unchanged and hashes on the URL.
    let album = album_desired(&members, true, true, WebpEncodeSettings::default()).remove(0);
    let webp = album.folder_webp.unwrap();
    let mp4 = album.folder_mp4.unwrap();
    assert_eq!(mp4.kind, ArtifactKind::FolderMp4);
    assert_eq!(mp4.path, "c/al/cover.mp4");
    assert_eq!(mp4.source_url, "vid-b");
    assert_eq!(mp4.hash, art_url_hash("vid-b"));
    assert_eq!(mp4.source_url, webp.source_url, "same variant feeds both");
}

#[test]
fn raw_cover_and_webp_are_independent_toggles() {
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", "vid-a"),
        "root",
        "c/al/a.flac",
    )];
    // webp-only keeps the transcode but no raw mp4.
    let webp_only = album_desired(&members, true, false, WebpEncodeSettings::default()).remove(0);
    assert!(webp_only.folder_webp.is_some());
    assert!(webp_only.folder_mp4.is_none());
    // mp4-only keeps the raw source but no transcode.
    let mp4_only = album_desired(&members, false, true, WebpEncodeSettings::default()).remove(0);
    assert!(mp4_only.folder_webp.is_none());
    assert!(mp4_only.folder_mp4.is_some());
}

#[test]
fn raw_cover_needs_an_animated_source() {
    // No variant carries a video_cover_url, so there is nothing to keep.
    let members = vec![album_member(
        album_clip("a", 3, "t0", "art-a", ""),
        "root",
        "c/al/a.flac",
    )];
    let album = album_desired(&members, true, true, WebpEncodeSettings::default()).remove(0);
    assert!(album.folder_mp4.is_none());
    assert!(album.folder_webp.is_none());
}

#[test]
fn album_with_no_art_yields_no_folder_jpg() {
    let members = vec![album_member(
        album_clip("a", 3, "t0", "", ""),
        "root",
        "c/al/a.flac",
    )];
    let albums = album_desired(&members, true, false, WebpEncodeSettings::default());
    assert!(albums[0].folder_jpg.is_none());
    assert!(albums[0].folder_webp.is_none());
}

#[test]
fn album_desired_groups_by_root_id() {
    let members = vec![
        album_member(album_clip("a", 1, "t0", "art-a", ""), "r1", "c/al1/a.flac"),
        album_member(album_clip("b", 1, "t0", "art-b", ""), "r2", "c/al2/b.flac"),
        album_member(album_clip("c", 9, "t0", "art-c", ""), "r1", "c/al1/c.flac"),
    ];
    let albums = album_desired(&members, false, false, WebpEncodeSettings::default());
    assert_eq!(albums.len(), 2);
    assert_eq!(albums[0].root_id, "r1");
    assert_eq!(albums[0].folder_jpg.as_ref().unwrap().source_url, "art-c");
    assert_eq!(
        albums[0].folder_jpg.as_ref().unwrap().path,
        "c/al1/folder.jpg"
    );
    assert_eq!(albums[1].root_id, "r2");
    assert_eq!(albums[1].folder_jpg.as_ref().unwrap().source_url, "art-b");
    assert_eq!(
        albums[1].folder_jpg.as_ref().unwrap().path,
        "c/al2/folder.jpg"
    );
}

#[test]
fn plan_writes_folder_art_when_store_empty() {
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", "vid-a"),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, true, false, WebpEncodeSettings::default());
    let actions = plan_album_artifacts(&desired, &BTreeMap::new(), true, &HashMap::new());
    assert_eq!(
        actions,
        vec![
            Action::WriteArtifact {
                kind: ArtifactKind::FolderJpg,
                path: "c/al/folder.jpg".to_string(),
                source_url: "art-a".to_string(),
                hash: art_url_hash("art-a"),
                owner_id: "root".to_string(),
                content: None,
            },
            Action::WriteArtifact {
                kind: ArtifactKind::FolderWebp,
                path: "c/al/cover.webp".to_string(),
                source_url: "vid-a".to_string(),
                hash: webp_art_hash("vid-a", &WebpEncodeSettings::default()),
                owner_id: "root".to_string(),
                content: None,
            },
        ]
    );
}

#[test]
fn plan_skips_when_hash_and_path_match() {
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", ""),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    assert!(plan_album_artifacts(&desired, &albums, true, &HashMap::new()).is_empty());
}

#[test]
fn plan_rewrites_when_path_drifts_even_if_hash_matches() {
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", ""),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("old/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::WriteArtifact { path, .. } if path == "c/al/folder.jpg"
    ));
}

#[test]
fn h1_most_played_flip_to_same_art_writes_nothing() {
    // Two variants sharing identical art. Run 1: "a" is most-played.
    let run1 = vec![
        album_member(
            album_clip("a", 9, "t0", "same-art", ""),
            "root",
            "c/al/a.flac",
        ),
        album_member(
            album_clip("b", 1, "t1", "same-art", ""),
            "root",
            "c/al/b.flac",
        ),
    ];
    let desired1 = album_desired(&run1, false, false, WebpEncodeSettings::default());
    let write1 = plan_album_artifacts(&desired1, &BTreeMap::new(), true, &HashMap::new());
    assert_eq!(write1.len(), 1);

    // Persist the winner's state as the executor would.
    let mut albums = BTreeMap::new();
    if let Action::WriteArtifact {
        path,
        hash,
        owner_id,
        ..
    } = &write1[0]
    {
        albums.insert(
            owner_id.clone(),
            AlbumArt {
                folder_jpg: Some(stored(path, hash)),
                folder_webp: None,
                folder_mp4: None,
            },
        );
    }

    // Run 2: "b" overtakes "a" on plays, but the art content is identical.
    let run2 = vec![
        album_member(
            album_clip("a", 1, "t0", "same-art", ""),
            "root",
            "c/al/a.flac",
        ),
        album_member(
            album_clip("b", 9, "t1", "same-art", ""),
            "root",
            "c/al/b.flac",
        ),
    ];
    let desired2 = album_desired(&run2, false, false, WebpEncodeSettings::default());
    // The winner flipped, but the chosen art content hash did not: no churn.
    assert!(plan_album_artifacts(&desired2, &albums, true, &HashMap::new()).is_empty());
}

#[test]
fn h1_flip_to_different_art_writes_exactly_one() {
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("old-art"))),
            folder_webp: None,
            folder_mp4: None,
        },
    );
    // The new most-played variant carries genuinely different art.
    let members = vec![
        album_member(
            album_clip("a", 1, "t0", "old-art", ""),
            "root",
            "c/al/a.flac",
        ),
        album_member(
            album_clip("b", 9, "t1", "new-art", ""),
            "root",
            "c/al/b.flac",
        ),
    ];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());
    let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::WriteArtifact { hash, .. } if *hash == art_url_hash("new-art")
    ));
}

#[test]
fn one_write_per_album_regardless_of_clip_count() {
    let members: Vec<Desired> = (0..200)
        .map(|i| {
            album_member(
                album_clip(
                    &format!("clip-{i:03}"),
                    i as u64,
                    &format!("t{i:03}"),
                    &format!("art-{i:03}"),
                    &format!("vid-{i:03}"),
                ),
                "root",
                &format!("c/al/clip-{i:03}.flac"),
            )
        })
        .collect();
    let desired = album_desired(&members, true, false, WebpEncodeSettings::default());
    assert_eq!(desired.len(), 1);
    let actions = plan_album_artifacts(&desired, &BTreeMap::new(), true, &HashMap::new());
    // Exactly one folder.jpg and one cover.webp for the whole 200-clip album.
    assert_eq!(actions.len(), 2);
    assert_eq!(
        actions
            .iter()
            .filter(|a| matches!(a, Action::WriteArtifact { .. }))
            .count(),
        2
    );
}

#[test]
fn emptied_album_deletes_only_when_can_delete() {
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", "h")),
            folder_webp: Some(stored("c/al/cover.webp", "hw")),
            folder_mp4: Some(stored("c/al/cover.mp4", "hm")),
        },
    );
    // No album desires this root any more (it emptied out this run).
    let desired: Vec<AlbumDesired> = Vec::new();

    // Gated off: an incomplete/unsafe listing removes nothing.
    assert!(plan_album_artifacts(&desired, &albums, false, &HashMap::new()).is_empty());

    // Gated on: every stored kind is removed, sorted by kind.
    let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
    assert_eq!(
        actions,
        vec![
            Action::DeleteArtifact {
                kind: ArtifactKind::FolderJpg,
                path: "c/al/folder.jpg".to_string(),
                owner_id: "root".to_string(),
            },
            Action::DeleteArtifact {
                kind: ArtifactKind::FolderWebp,
                path: "c/al/cover.webp".to_string(),
                owner_id: "root".to_string(),
            },
            Action::DeleteArtifact {
                kind: ArtifactKind::FolderMp4,
                path: "c/al/cover.mp4".to_string(),
                owner_id: "root".to_string(),
            },
        ]
    );
}

#[test]
fn disappeared_webp_source_deletes_only_that_kind_when_gated() {
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: Some(stored("c/al/cover.webp", &art_url_hash("vid-a"))),
            folder_mp4: None,
        },
    );
    // The album is still present with the same folder.jpg, but animated
    // covers are now off, so the webp source has disappeared.
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", "vid-a"),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, false, false, WebpEncodeSettings::default());

    assert!(plan_album_artifacts(&desired, &albums, false, &HashMap::new()).is_empty());

    let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
    assert_eq!(
        actions,
        vec![Action::DeleteArtifact {
            kind: ArtifactKind::FolderWebp,
            path: "c/al/cover.webp".to_string(),
            owner_id: "root".to_string(),
        }]
    );
}

#[test]
fn disappeared_raw_cover_deletes_only_that_kind_when_gated() {
    let mut albums = BTreeMap::new();
    albums.insert(
        "root".to_string(),
        AlbumArt {
            folder_jpg: Some(stored("c/al/folder.jpg", &art_url_hash("art-a"))),
            folder_webp: Some(stored(
                "c/al/cover.webp",
                &webp_art_hash("vid-a", &WebpEncodeSettings::default()),
            )),
            folder_mp4: Some(stored("c/al/cover.mp4", &art_url_hash("vid-a"))),
        },
    );
    // The album stays and animated covers stay on, but raw cover retention
    // is now off, so only the raw `cover.mp4` is no longer desired.
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", "vid-a"),
        "root",
        "c/al/a.flac",
    )];
    let desired = album_desired(&members, true, false, WebpEncodeSettings::default());

    // Gated off: nothing removed on an unsafe listing.
    assert!(plan_album_artifacts(&desired, &albums, false, &HashMap::new()).is_empty());

    // Gated on: only the raw cover goes; folder.jpg and cover.webp stay.
    let actions = plan_album_artifacts(&desired, &albums, true, &HashMap::new());
    assert_eq!(
        actions,
        vec![Action::DeleteArtifact {
            kind: ArtifactKind::FolderMp4,
            path: "c/al/cover.mp4".to_string(),
            owner_id: "root".to_string(),
        }]
    );
}

#[test]
fn plan_album_artifacts_is_deterministically_ordered() {
    let members = vec![
        album_member(
            album_clip("a", 1, "t0", "art-a", "vid-a"),
            "r2",
            "c/al2/a.flac",
        ),
        album_member(
            album_clip("b", 1, "t0", "art-b", "vid-b"),
            "r1",
            "c/al1/b.flac",
        ),
    ];
    let desired = album_desired(&members, true, true, WebpEncodeSettings::default());
    let actions = plan_album_artifacts(&desired, &BTreeMap::new(), true, &HashMap::new());
    let keys: Vec<(&str, ArtifactKind)> = actions
        .iter()
        .map(|a| match a {
            Action::WriteArtifact { owner_id, kind, .. } => (owner_id.as_str(), *kind),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            ("r1", ArtifactKind::FolderJpg),
            ("r1", ArtifactKind::FolderWebp),
            ("r1", ArtifactKind::FolderMp4),
            ("r2", ArtifactKind::FolderJpg),
            ("r2", ArtifactKind::FolderWebp),
            ("r2", ArtifactKind::FolderMp4),
        ]
    );
}

// ── Phase 9: playlist artifacts ─────────────────────────────────

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

// ── Keyed stem reconcile ────────────────────────────────────────

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
