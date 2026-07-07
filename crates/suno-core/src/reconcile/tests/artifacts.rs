use super::*;

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
