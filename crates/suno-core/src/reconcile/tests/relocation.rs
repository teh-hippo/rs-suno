//! #355 headline: an audio retitle relocates every stranded, still-wanted
//! sidecar and stem to the current audio base, emitting only moves and never a
//! delete. The strand is healed whether it comes from this run's rename or a
//! pre-fix run that advanced the audio path without moving the sidecars.

use super::*;

fn state(path: &str, hash: &str) -> ArtifactState {
    ArtifactState {
        path: path.to_string(),
        hash: hash.to_string(),
    }
}

/// A kept FLAC clip with a stranded cover and one stranded stem, desiring only
/// the audio (empty artifacts, no authoritative stem listing).
fn stranded_entry(audio: &str, cover: &str, stem_path: &str) -> ManifestEntry {
    let mut e = entry(audio, AudioFormat::Flac, "m", "art");
    e.cover_jpg = Some(state(cover, "arthash"));
    e.stems
        .insert("voc".to_string(), state(stem_path, "stemhash"));
    e
}

fn local(entries: &[(&str, u64)]) -> HashMap<String, LocalFile> {
    entries
        .iter()
        .map(|(path, size)| (path.to_string(), present(*size)))
        .collect()
}

#[test]
fn retitle_relocates_stranded_cover_and_stems_with_no_deletes() {
    // The rendered audio drifted Old.flac -> New.flac; the manifest still tracks
    // the cover and stem at the OLD base. Both must reparent to New, with zero
    // deletes. The stem's inner filename intentionally keeps the old title
    // (folder-only reparent).
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        stranded_entry("Old.flac", "Old.jpg", "Old.stems/Old - Vocals [voc].wav"),
    );
    let d = vec![desired("a", "New.flac", AudioFormat::Flac, "m", "art")];
    let local = local(&[
        ("a", 100),
        ("Old.jpg", 50),
        ("Old.stems/Old - Vocals [voc].wav", 50),
    ]);

    let plan = reconcile(&manifest, &d, &local, &mirror_ok());

    assert_eq!(plan.renames(), 1, "the audio itself is renamed");
    assert_eq!(plan.artifact_moves(), 1);
    assert_eq!(plan.stem_moves(), 1);
    assert_eq!(plan.deletes(), 0);
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(plan.stem_deletes(), 0);
    assert_eq!(plan.artifact_writes(), 0);
    assert_eq!(plan.stem_writes(), 0);

    assert!(
        plan.actions.contains(&Action::MoveArtifact {
            kind: ArtifactKind::CoverJpg,
            from: "Old.jpg".to_string(),
            to: "New.jpg".to_string(),
            source_url: String::new(),
            hash: "arthash".to_string(),
            owner_id: "a".to_string(),
        }),
        "the cover reparents Old.jpg -> New.jpg with no source_url"
    );
    assert!(
        plan.actions.contains(&Action::MoveStem {
            clip_id: "a".to_string(),
            key: "voc".to_string(),
            stem_id: String::new(),
            from: "Old.stems/Old - Vocals [voc].wav".to_string(),
            to: "New.stems/Old - Vocals [voc].wav".to_string(),
            source_url: String::new(),
            format: StemFormat::Mp3,
            hash: "stemhash".to_string(),
        }),
        "the stem folder reparents while its inner filename keeps the old title"
    );
}

#[test]
fn retitle_relocation_skipped_when_old_files_absent() {
    // The manifest tracks a cover and stem at the old base, but neither is on
    // disk: no move can rename a vanished file, and no source_url exists to
    // fetch. Skip cleanly, never fetch, never delete.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        stranded_entry("Old.flac", "Old.jpg", "Old.stems/voc.wav"),
    );
    let d = vec![desired("a", "New.flac", AudioFormat::Flac, "m", "art")];
    // Only the audio is present on disk; the old sidecar and stem are gone.
    let local = local(&[("a", 100)]);

    let plan = reconcile(&manifest, &d, &local, &mirror_ok());

    assert_eq!(plan.renames(), 1);
    assert_eq!(plan.artifact_moves(), 0);
    assert_eq!(plan.stem_moves(), 0);
    assert_eq!(plan.artifact_writes(), 0);
    assert_eq!(plan.stem_writes(), 0);
    assert_eq!(plan.artifact_deletes(), 0);
    assert_eq!(plan.stem_deletes(), 0);
}

#[test]
fn stable_healthy_library_relocates_nothing() {
    // The cover and stem already sit at the current base and the audio is
    // stable: a correctly placed file equals its expected path, so the pass is
    // idempotent and emits no move.
    let mut manifest = Manifest::new();
    manifest.insert(
        "a",
        stranded_entry("New.flac", "New.jpg", "New.stems/voc.wav"),
    );
    let d = vec![desired("a", "New.flac", AudioFormat::Flac, "m", "art")];
    let local = local(&[("a", 100), ("New.jpg", 50), ("New.stems/voc.wav", 50)]);

    let plan = reconcile(&manifest, &d, &local, &mirror_ok());

    assert_eq!(plan.renames(), 0);
    assert_eq!(plan.artifact_moves(), 0);
    assert_eq!(plan.stem_moves(), 0);
    assert_eq!(plan.deletes(), 0);
}
