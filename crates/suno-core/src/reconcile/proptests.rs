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

fn small_path() -> impl Strategy<Value = String> {
    (0u8..6).prop_map(|n| format!("path{n}"))
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

fn manifest_entry() -> impl Strategy<Value = ManifestEntry> {
    (
        manifest_path(),
        audio_format(),
        small_hash(),
        small_hash(),
        0u64..4,
        any::<bool>(),
    )
        .prop_map(
            |(path, format, meta_hash, art_hash, size, preserve)| ManifestEntry {
                path,
                format,
                meta_hash,
                art_hash,
                size,
                preserve,
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

fn local_strategy() -> impl Strategy<Value = HashMap<String, LocalFile>> {
    hash_map(clip_id(), local_file(), 0..8)
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

fn build_desired(id: String, fields: DesiredFields) -> Desired {
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
        artifacts: Vec::new(),
        stems: None,
    }
}

// A desired list over the shared id space that may hold duplicate ids, so
// aggregation and the trashed/private/copy folds are all exercised.
fn desired_strategy() -> impl Strategy<Value = Vec<Desired>> {
    vec((clip_id(), desired_fields()), 0..10).prop_map(|items| {
        items
            .into_iter()
            .map(|(id, fields)| build_desired(id, fields))
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

fn write_target_paths(plan: &Plan) -> BTreeSet<&str> {
    plan.actions
        .iter()
        .filter_map(|a| match a {
            Action::Download { path, .. } | Action::Reformat { path, .. } => Some(path.as_str()),
            Action::Rename { to, .. } => Some(to.as_str()),
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

    // INVARIANT 9: no Delete carries an empty path.
    #[test]
    fn inv9_no_delete_with_empty_path(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        sources in sources_strategy(),
    ) {
        let plan = reconcile(&manifest, &desired, &local, &sources);
        for action in &plan.actions {
            if let Action::Delete { path, .. } = action {
                prop_assert!(!path.is_empty(), "delete with an empty path");
            }
        }
    }

    // INVARIANT 10: no Delete path equals a file another action writes this
    // run, so a deletion can never clobber a just-written file.
    #[test]
    fn inv10_no_delete_aliases_a_write_target(
        manifest in manifest_strategy(),
        desired in desired_strategy(),
        local in local_strategy(),
        sources in sources_strategy(),
    ) {
        let plan = reconcile(&manifest, &desired, &local, &sources);
        let targets = write_target_paths(&plan);
        for action in &plan.actions {
            if let Action::Delete { path, .. } = action {
                prop_assert!(
                    !targets.contains(path.as_str()),
                    "delete path {path} aliases a write target"
                );
            }
        }
    }
}
