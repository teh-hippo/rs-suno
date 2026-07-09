//! Property-based tests that lock the delete guard against random inputs.
//!
//! These complement the deterministic unit tests above. The generators are
//! bounded (a small clip-id space, short paths and hashes) so the cases stay
//! cheap and CI stays stable, and failure persistence is disabled so a run
//! never leaves regression files behind.
//!
//! The generators are fully random: `trashed`, `private`, source `modes`, and
//! the persisted `preserve` marker are all exercised, and the desired list may
//! hold duplicate ids so aggregation is covered too. The invariants below are
//! written to hold for every such input, so the trashed delete path is no
//! longer a special case hidden from the property tests.

use super::*;
use proptest::collection::{btree_map, hash_map, vec};
use proptest::prelude::*;
use std::collections::BTreeSet;

type DesiredFields = (
    String,
    AudioFormat,
    String,
    String,
    Vec<SourceMode>,
    bool,
    bool,
);

fn audio_format() -> impl Strategy<Value = AudioFormat> {
    prop_oneof![
        Just(AudioFormat::Mp3),
        Just(AudioFormat::Flac),
        Just(AudioFormat::Wav),
    ]
}

fn source_mode() -> impl Strategy<Value = SourceMode> {
    prop_oneof![Just(SourceMode::Mirror), Just(SourceMode::Copy)]
}

// A small id space forces overlap between the manifest and the desired set,
// so deletes, renames, retags, and downloads all get exercised.
fn clip_id() -> impl Strategy<Value = String> {
    (0u8..8).prop_map(|n| format!("c{n}"))
}

// Paths drawn from a tiny shared space that deliberately includes case-only and
// NFC/NFD aliases, so the canonical-key deletion guard (`suppress_path_aliasing`,
// keyed on NFC + lowercase) is actually exercised: `path1`/`Path1` name one file
// on a case-insensitive filesystem, and the NFC and NFD encodings of "café" name
// one file on an NFC-normalising one. INV10 compares canonically to match it.
fn small_path() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("path0".to_string()),
        Just("path1".to_string()),
        Just("Path1".to_string()),
        Just("caf\u{00e9}".to_string()),
        Just("cafe\u{0301}".to_string()),
        Just("path5".to_string()),
    ]
}

// The manifest entry path is the source of every `Delete.path`, so it must
// occasionally be empty for INV9 to actually exercise the empty-path guard.
fn manifest_path() -> impl Strategy<Value = String> {
    prop_oneof![
        1 => Just(String::new()),
        6 => small_path(),
    ]
}

fn small_hash() -> impl Strategy<Value = String> {
    (0u8..4).prop_map(|n| format!("h{n}"))
}

// Tiny, shared path/key spaces for sidecars and stems, so a manifest slot and a
// desired artifact/stem frequently agree or drift on path and hash, exercising
// the write, rewrite, relocate (#141), and remove branches of the per-clip
// artifact and stem planners.
fn artifact_path() -> impl Strategy<Value = String> {
    (0u8..4).prop_map(|n| format!("art{n}"))
}

fn stem_key() -> impl Strategy<Value = String> {
    (0u8..4).prop_map(|n| format!("k{n}"))
}

fn stem_path() -> impl Strategy<Value = String> {
    (0u8..4).prop_map(|n| format!("stem{n}"))
}

fn per_clip_kind() -> impl Strategy<Value = ArtifactKind> {
    prop_oneof![
        Just(ArtifactKind::CoverJpg),
        Just(ArtifactKind::CoverWebp),
        Just(ArtifactKind::DetailsTxt),
        Just(ArtifactKind::LyricsTxt),
        Just(ArtifactKind::Lrc),
        Just(ArtifactKind::VideoMp4),
    ]
}

fn stem_format() -> impl Strategy<Value = StemFormat> {
    prop_oneof![Just(StemFormat::Wav), Just(StemFormat::Mp3)]
}

fn artifact_state() -> impl Strategy<Value = ArtifactState> {
    (artifact_path(), small_hash()).prop_map(|(path, hash)| ArtifactState { path, hash })
}

// Weighted so both the present (rewrite/relocate/remove) and absent (fresh
// write) branches of the per-clip artifact planner are reached.
fn opt_artifact_state() -> impl Strategy<Value = Option<ArtifactState>> {
    prop_oneof![
        2 => Just(None),
        3 => artifact_state().prop_map(Some),
    ]
}

fn manifest_stems() -> impl Strategy<Value = BTreeMap<String, ArtifactState>> {
    btree_map(
        stem_key(),
        (stem_path(), small_hash()).prop_map(|(path, hash)| ArtifactState { path, hash }),
        0..4,
    )
}

fn manifest_entry() -> impl Strategy<Value = ManifestEntry> {
    let base = (
        manifest_path(),
        audio_format(),
        small_hash(),
        small_hash(),
        0u64..4,
        any::<bool>(),
    );
    let sidecars = (
        opt_artifact_state(),
        opt_artifact_state(),
        opt_artifact_state(),
        opt_artifact_state(),
        opt_artifact_state(),
        opt_artifact_state(),
    );
    (base, sidecars, manifest_stems()).prop_map(
        |(
            (path, format, meta_hash, art_hash, size, preserve),
            (cover_jpg, cover_webp, details_txt, lyrics_txt, lrc, video_mp4),
            stems,
        )| ManifestEntry {
            path,
            format,
            meta_hash,
            art_hash,
            size,
            preserve,
            cover_jpg,
            cover_webp,
            details_txt,
            lyrics_txt,
            lrc,
            video_mp4,
            stems,
            ..Default::default()
        },
    )
}

fn manifest_strategy() -> impl Strategy<Value = Manifest> {
    btree_map(clip_id(), manifest_entry(), 0..8).prop_map(|entries| Manifest { entries })
}

fn local_file() -> impl Strategy<Value = LocalFile> {
    (any::<bool>(), 0u64..4).prop_map(|(exists, size)| LocalFile { exists, size })
}

// Probe map keyed like the CLI's `probe_local`: clip ids for audio, plus the
// sidecar and stem path spaces, so a tracked artifact or stem present on disk is
// observed and can be relocated with a Move when only its path drifts (#141),
// rather than the planner never seeing the old file.
fn local_strategy() -> impl Strategy<Value = HashMap<String, LocalFile>> {
    (
        hash_map(clip_id(), local_file(), 0..8),
        hash_map(artifact_path(), local_file(), 0..4),
        hash_map(stem_path(), local_file(), 0..4),
    )
        .prop_map(|(mut audio, arts, stems)| {
            audio.extend(arts);
            audio.extend(stems);
            audio
        })
}

fn source_status() -> impl Strategy<Value = SourceStatus> {
    (source_mode(), any::<bool>()).prop_map(|(mode, fully_enumerated)| SourceStatus {
        mode,
        fully_enumerated,
    })
}

fn sources_strategy() -> impl Strategy<Value = Vec<SourceStatus>> {
    vec(source_status(), 0..5)
}

fn copy_sources_strategy() -> impl Strategy<Value = Vec<SourceStatus>> {
    vec(
        any::<bool>().prop_map(|fully_enumerated| SourceStatus {
            mode: SourceMode::Copy,
            fully_enumerated,
        }),
        1..5,
    )
}

fn desired_fields() -> impl Strategy<Value = DesiredFields> {
    (
        small_path(),
        audio_format(),
        small_hash(),
        small_hash(),
        vec(source_mode(), 1..3),
        any::<bool>(),
        any::<bool>(),
    )
}

// One desired sidecar. `content: Some` models a generated (inline) artifact
// (text sidecars); `None` a fetched one (covers, video).
fn desired_artifact() -> impl Strategy<Value = DesiredArtifact> {
    (
        per_clip_kind(),
        artifact_path(),
        small_hash(),
        any::<bool>(),
    )
        .prop_map(|(kind, path, hash, inline)| DesiredArtifact {
            kind,
            path,
            source_url: if inline {
                String::new()
            } else {
                format!("u{hash}")
            },
            hash,
            content: inline.then(|| "body".to_string()),
        })
}

fn desired_artifacts() -> impl Strategy<Value = Vec<DesiredArtifact>> {
    vec(desired_artifact(), 0..4)
}

fn desired_stem() -> impl Strategy<Value = DesiredStem> {
    (
        stem_key(),
        small_hash(),
        stem_path(),
        stem_format(),
        small_hash(),
    )
        .prop_map(|(key, stem_id, path, format, hash)| DesiredStem {
            key,
            stem_id: format!("s{stem_id}"),
            path,
            source_url: format!("u{hash}"),
            format,
            hash,
        })
}

// Tri-state, encoding stem deletion safety: `None` models a non-authoritative
// listing (feature off, `has_stem` false, or a failed/partial/paged listing),
// where every tracked stem must be KEPT and none deleted; `Some(set)` is an
// authoritative, fully enumerated set that may remove tracked stems.
fn desired_stems() -> impl Strategy<Value = Option<Vec<DesiredStem>>> {
    prop_oneof![
        1 => Just(None),
        3 => vec(desired_stem(), 0..4).prop_map(Some),
    ]
}

fn build_desired(
    id: String,
    fields: DesiredFields,
    artifacts: Vec<DesiredArtifact>,
    stems: Option<Vec<DesiredStem>>,
) -> Desired {
    let (path, format, meta_hash, art_hash, modes, trashed, private) = fields;
    let clip = Clip {
        id,
        title: "t".to_string(),
        ..Default::default()
    };
    Desired {
        lineage: LineageContext::own_root(&clip),
        clip,
        path,
        format,
        meta_hash,
        art_hash,
        embedded_lyrics_hash: String::new(),
        modes,
        trashed,
        private,
        artifacts,
        stems,
    }
}

// A desired list over the shared id space that may hold duplicate ids, so
// aggregation and the trashed/private/copy folds are all exercised. Artifacts
// and stems are assigned PER ID (every duplicate of an id shares them), because
// `aggregate_desired` copies the non-safety fields from a `rep_key`-chosen
// representative and, on an exact rep_key tie, keeps the first-seen entry: were
// two same-id duplicates to carry different artifacts/stems the merged result
// would depend on input order and INV4 (order-independence) would break. In
// production every duplicate of one clip renders identical sidecars, so this is
// faithful while preserving the fold coverage duplicates provide.
fn desired_strategy() -> impl Strategy<Value = Vec<Desired>> {
    let id_arts = btree_map(clip_id(), (desired_artifacts(), desired_stems()), 0..8);
    (vec((clip_id(), desired_fields()), 0..10), id_arts).prop_map(|(items, id_arts)| {
        items
            .into_iter()
            .map(|(id, fields)| {
                let (artifacts, stems) = id_arts.get(&id).cloned().unwrap_or_default();
                build_desired(id, fields, artifacts, stems)
            })
            .collect()
    })
}

fn desired_ids(desired: &[Desired]) -> BTreeSet<&str> {
    desired.iter().map(|d| d.clip.id.as_str()).collect()
}

// Ids protected from deletion: any duplicate that is private or copy-held
// protects the whole id, mirroring the aggregation's union semantics.
fn protected_ids(desired: &[Desired]) -> BTreeSet<&str> {
    desired
        .iter()
        .filter(|d| d.private || d.modes.contains(&SourceMode::Copy))
        .map(|d| d.clip.id.as_str())
        .collect()
}

// Ids with at least one non-trashed duplicate: the trashed fold is an
// intersection, so one live duplicate keeps the clip.
fn non_trashed_ids(desired: &[Desired]) -> BTreeSet<&str> {
    desired
        .iter()
        .filter(|d| !d.trashed)
        .map(|d| d.clip.id.as_str())
        .collect()
}

fn delete_clip_ids(plan: &Plan) -> Vec<&str> {
    plan.actions
        .iter()
        .filter_map(|a| match a {
            Action::Delete { clip_id, .. } => Some(clip_id.as_str()),
            _ => None,
        })
        .collect()
}

// Canonical keys of every write or move DESTINATION, exactly the set
// `suppress_path_aliasing` protects across its seven write/move classes: any
// delete whose canonical key collides with one of these is downgraded to a
// Skip, so no write ever clobbers a file another action (re)creates this run.
fn write_target_keys(plan: &Plan) -> BTreeSet<String> {
    plan.actions
        .iter()
        .filter_map(|a| match a {
            Action::Download { path, .. }
            | Action::Reformat { path, .. }
            | Action::WriteArtifact { path, .. }
            | Action::WriteStem { path, .. } => Some(path.as_str()),
            Action::Rename { to, .. }
            | Action::MoveArtifact { to, .. }
            | Action::MoveStem { to, .. } => Some(to.as_str()),
            _ => None,
        })
        .map(canonical_path_key)
        .collect()
}

// Every delete DESTINATION path across the three delete classes (audio,
// sidecar, stem).
fn delete_paths(plan: &Plan) -> Vec<&str> {
    plan.actions
        .iter()
        .filter_map(|a| match a {
            Action::Delete { path, .. }
            | Action::DeleteArtifact { path, .. }
            | Action::DeleteStem { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect()
}

// The owning clip id of every sidecar or stem delete. `delete_artifact_action`
// and `delete_stem_action` share the audio `can_delete` gate and the owning
// entry's `preserve` marker, so a protected clip must never have one removed.
fn sidecar_delete_owner_ids(plan: &Plan) -> Vec<&str> {
    plan.actions
        .iter()
        .filter_map(|a| match a {
            Action::DeleteArtifact { owner_id, .. } => Some(owner_id.as_str()),
            Action::DeleteStem { clip_id, .. } => Some(clip_id.as_str()),
            _ => None,
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    // INVARIANT 1: a desired clip is deleted only when every one of its
    // duplicates is trashed; one live (non-trashed) duplicate keeps it.
    #[test]
    fn inv1_desired_clip_deleted_only_when_fully_trashed(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        sources in sources_strategy(),
    ) {
        let plan = reconcile(&manifest, &desired, &local, &sources);
        let present = desired_ids(&desired);
        let live = non_trashed_ids(&desired);
        for id in delete_clip_ids(&plan) {
            prop_assert!(
                !(present.contains(id) && live.contains(id)),
                "deleted a desired clip with a non-trashed duplicate: {id}"
            );
        }
    }

    // INVARIANT 2: a single not-fully-enumerated mirror source (truncated,
    // partial, empty, or failed listing) suppresses every deletion, trashed
    // clips included.
    #[test]
    fn inv2_no_delete_when_any_mirror_unenumerated(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        mut sources in sources_strategy(),
    ) {
        sources.push(SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        });
        let plan = reconcile(&manifest, &desired, &local, &sources);
        prop_assert_eq!(plan.deletes(), 0);
        prop_assert_eq!(plan.artifact_deletes(), 0);
        prop_assert_eq!(plan.stem_deletes(), 0);
    }

    // INVARIANT 3: a copy-only run is additive and never deletes.
    #[test]
    fn inv3_all_copy_sources_means_no_deletes(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        sources in copy_sources_strategy(),
    ) {
        let plan = reconcile(&manifest, &desired, &local, &sources);
        prop_assert_eq!(plan.deletes(), 0);
        prop_assert_eq!(plan.artifact_deletes(), 0);
        prop_assert_eq!(plan.stem_deletes(), 0);
    }

    // INVARIANT 4: identical inputs always yield an identical plan, and the
    // plan does not depend on the order of the desired or source lists.
    #[test]
    fn inv4_plan_is_deterministic(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        sources in sources_strategy(),
    ) {
        let plan = reconcile(&manifest, &desired, &local, &sources);

        let again = reconcile(&manifest, &desired, &local, &sources);
        prop_assert_eq!(&plan, &again);

        let mut desired_rev = desired.clone();
        desired_rev.reverse();
        let mut sources_rev = sources.clone();
        sources_rev.reverse();
        let shuffled = reconcile(&manifest, &desired_rev, &local, &sources_rev);
        prop_assert_eq!(&plan, &shuffled);
    }

    // INVARIANT 5: every Delete names a clip that exists in the manifest.
    #[test]
    fn inv5_every_delete_is_in_the_manifest(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        sources in sources_strategy(),
    ) {
        let plan = reconcile(&manifest, &desired, &local, &sources);
        for id in delete_clip_ids(&plan) {
            prop_assert!(manifest.contains(id), "deleted a clip absent from the manifest: {id}");
        }
    }

    // INVARIANT 6: never delete a copy-held or private clip, whether that
    // protection is in the current selection or persisted on the manifest.
    #[test]
    fn inv6_never_deletes_protected_clip(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        sources in sources_strategy(),
    ) {
        let plan = reconcile(&manifest, &desired, &local, &sources);
        let protected = protected_ids(&desired);
        for id in delete_clip_ids(&plan) {
            prop_assert!(!protected.contains(id), "deleted a copy-held or private clip: {id}");
            let preserved = manifest.get(id).map(|e| e.preserve).unwrap_or(false);
            prop_assert!(!preserved, "deleted a preserve-marked clip: {id}");
        }
        // The sidecar and stem delete gates share the audio protection, so a
        // removed artifact or stem must never name a protected or preserved id.
        for id in sidecar_delete_owner_ids(&plan) {
            prop_assert!(
                !protected.contains(id),
                "deleted a sidecar or stem of a copy-held or private clip: {id}"
            );
            let preserved = manifest.get(id).map(|e| e.preserve).unwrap_or(false);
            prop_assert!(!preserved, "deleted a sidecar or stem of a preserve-marked clip: {id}");
        }
    }

    // INVARIANT 7: every Delete requires deletion to be allowed for the run,
    // so the trashed path is no longer an exception to the enumeration guard.
    #[test]
    fn inv7_no_delete_unless_deletion_allowed(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        sources in sources_strategy(),
    ) {
        let plan = reconcile(&manifest, &desired, &local, &sources);
        if !deletion_allowed(&sources) {
            prop_assert_eq!(plan.deletes(), 0);
            prop_assert_eq!(plan.artifact_deletes(), 0);
            prop_assert_eq!(plan.stem_deletes(), 0);
        }
    }

    // INVARIANT 8: at most one Delete per clip id.
    #[test]
    fn inv8_at_most_one_delete_per_clip(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        sources in sources_strategy(),
    ) {
        let plan = reconcile(&manifest, &desired, &local, &sources);
        let ids = delete_clip_ids(&plan);
        let unique: BTreeSet<&str> = ids.iter().copied().collect();
        prop_assert_eq!(ids.len(), unique.len());
    }

    // INVARIANT 9: no delete of any class (audio, sidecar, or stem) carries an
    // empty path.
    #[test]
    fn inv9_no_delete_with_empty_path(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        sources in sources_strategy(),
    ) {
        let plan = reconcile(&manifest, &desired, &local, &sources);
        for path in delete_paths(&plan) {
            prop_assert!(!path.is_empty(), "delete with an empty path");
        }
    }

    // INVARIANT 10: no delete of any class targets a file another action writes
    // or moves onto this run, comparing CANONICAL keys (case- and
    // normalisation-folded), so a deletion can never clobber a just-written
    // file even when the two paths differ only by case or Unicode form. The
    // canonical comparison is a strict superset of the byte-exact one.
    #[test]
    fn inv10_no_delete_aliases_a_write_target(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        sources in sources_strategy(),
    ) {
        let plan = reconcile(&manifest, &desired, &local, &sources);
        let targets = write_target_keys(&plan);
        for path in delete_paths(&plan) {
            prop_assert!(
                !targets.contains(&canonical_path_key(path)),
                "delete path {path} aliases a write target"
            );
        }
    }

    // INVARIANT 11 (LIVENESS): a fully unprotected orphan on a clean, fully
    // enumerated mirror IS deleted, and its cover and stem are co-deleted with
    // it. This is the anti-vacuous complement to the safety invariants above,
    // which an empty plan satisfies: here we craft a desired state whose only
    // outcome CAN be a positive delete, and assert the delete plus both
    // co-deletes actually fire. The orphan lives in a private `orphan-live/`
    // path namespace disjoint from `small_path`, so aliasing suppression can
    // never turn its deletes into skips, and its id is disjoint from the shared
    // clip-id space so no generated duplicate can protect or re-list it.
    #[test]
    fn inv11_unprotected_orphan_is_deleted_with_its_sidecars(
        mut manifest in manifest_strategy(),
        mut desired in desired_strategy(),
    ) {
        // Strip any generated duplicate of the crafted id (cannot occur given
        // disjoint id spaces, but keep the property total).
        desired.retain(|d| d.clip.id != "orphan-live");

        let mut entry = ManifestEntry {
            path: "orphan-live/song.flac".to_string(),
            format: AudioFormat::Flac,
            size: 7,
            preserve: false,
            cover_jpg: Some(ArtifactState {
                path: "orphan-live/cover.jpg".to_string(),
                hash: "c".to_string(),
            }),
            ..Default::default()
        };
        entry.stems.insert(
            "vocals".to_string(),
            ArtifactState {
                path: "orphan-live/song.vocals.wav".to_string(),
                hash: "v".to_string(),
            },
        );
        manifest.insert("orphan-live", entry);

        // A single clean, fully enumerated mirror: deletion is allowed and the
        // orphan is absent from it, so the audio delete gate opens.
        let sources = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: true,
        }];
        let local = HashMap::new();

        let plan = reconcile(&manifest, &desired, &local, &sources);

        prop_assert!(
            delete_clip_ids(&plan).contains(&"orphan-live"),
            "unprotected orphan on a clean mirror was not deleted"
        );
        let deleted_cover = plan.actions.iter().any(|a| matches!(
            a,
            Action::DeleteArtifact { owner_id, kind: ArtifactKind::CoverJpg, .. }
                if owner_id == "orphan-live"
        ));
        prop_assert!(deleted_cover, "orphan's cover was not co-deleted");
        let deleted_stem = plan.actions.iter().any(|a| matches!(
            a,
            Action::DeleteStem { clip_id, key, .. }
                if clip_id == "orphan-live" && key == "vocals"
        ));
        prop_assert!(deleted_stem, "orphan's stem was not co-deleted");
    }
}
