//! Layer 1: deterministic, hand-built end-to-end sync scenarios.
//!
//! Each test drives the whole pipeline (probe, reconcile, execute) over the
//! in-memory doubles and asserts the exact resulting disk and manifest state.
//! These are the readable, golden-path-and-edges scenarios the strategy doc
//! calls for; the property-based layers generalise them.

use super::harness::{
    ClipSpec, clean_mirror, desired_set, ext, fast_opts, mutating_actions, path_of, probe_local,
    run_clean, run_sync, world,
};
use crate::config::AudioFormat;
use crate::fs::Filesystem;
use crate::manifest::Manifest;
use crate::reconcile::{SourceMode, SourceStatus, reconcile};
use crate::testutil::MemFs;

/// Assert the on-disk path set equals the given specs' paths, exactly.
fn assert_disk_is(fs: &MemFs, specs: &[ClipSpec]) {
    let mut want: Vec<String> = specs.iter().map(path_of).collect();
    want.sort();
    assert_eq!(
        fs.paths(),
        want,
        "on-disk set does not match the desired set"
    );
}

/// Assert every manifest entry points at a file that exists with the recorded
/// size: the manifest-to-disk consistency invariant (I-e).
fn assert_manifest_matches_disk(manifest: &Manifest, fs: &MemFs) {
    for (id, entry) in manifest.iter() {
        let stat = fs.metadata(&entry.path).unwrap_or_else(|| {
            panic!(
                "manifest entry {id} points at a missing file {}",
                entry.path
            )
        });
        assert_eq!(
            stat.size, entry.size,
            "manifest size disagrees with disk for {id}"
        );
    }
}

#[test]
fn first_sync_populates_an_empty_library() {
    let specs = [
        ClipSpec::mirror("c001", "Dawn"),
        ClipSpec::mirror("c002", "Dusk"),
    ];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let (plan, outcome) = run_clean(&specs, &fs, &mut manifest);

    assert_eq!(plan.downloads(), 2);
    assert_eq!(outcome.downloaded, 2);
    assert_eq!(outcome.failed(), 0);
    assert_disk_is(&fs, &specs);
    assert_eq!(manifest.len(), 2);
    assert_manifest_matches_disk(&manifest, &fs);
}

#[test]
fn resync_with_no_change_is_a_pure_noop() {
    let specs = [
        ClipSpec::mirror("c001", "Dawn"),
        ClipSpec::mirror("c002", "Dusk"),
    ];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);
    let disk_before = fs.paths();

    let (plan, outcome) = run_clean(&specs, &fs, &mut manifest);

    assert_eq!(mutating_actions(&plan), 0, "second run must be all skips");
    assert_eq!(plan.skips(), 2);
    assert_eq!(outcome.downloaded, 0);
    assert_eq!(outcome.deleted, 0);
    assert_eq!(outcome.retagged, 0);
    assert_eq!(outcome.skipped, 2);
    assert_eq!(
        fs.paths(),
        disk_before,
        "a no-op run must not touch the disk"
    );
}

#[test]
fn metadata_change_triggers_a_retag_not_a_download() {
    let mut spec = ClipSpec::mirror("c001", "Anthem");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);
    let old_hash = manifest.get("c001").unwrap().meta_hash.clone();
    let path = path_of(&spec);
    let bytes_before = fs.read_file(&path).unwrap();

    spec = spec.with_tags("a totally different mood");
    let (plan, outcome) = run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);

    assert_eq!(plan.retags(), 1);
    assert_eq!(plan.downloads(), 0);
    assert_eq!(outcome.retagged, 1);
    let new_hash = &manifest.get("c001").unwrap().meta_hash;
    assert_ne!(
        &old_hash, new_hash,
        "retag must refresh the stored meta hash"
    );
    assert!(fs.exists(&path), "the file stays at the same path");
    assert_ne!(
        fs.read_file(&path).unwrap(),
        bytes_before,
        "the file was re-tagged"
    );
    assert_manifest_matches_disk(&manifest, &fs);
}

#[test]
fn format_change_reformats_and_removes_the_old_file() {
    let spec_mp3 = ClipSpec::mirror("c001", "Crossfade");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(std::slice::from_ref(&spec_mp3), &fs, &mut manifest);
    let old_path = path_of(&spec_mp3);
    assert!(fs.exists(&old_path));

    let spec_flac = spec_mp3.clone().with_format(AudioFormat::Flac);
    let new_path = path_of(&spec_flac);
    let (plan, outcome) = run_clean(std::slice::from_ref(&spec_flac), &fs, &mut manifest);

    assert_eq!(plan.reformats(), 1);
    assert_eq!(outcome.reformatted, 1);
    assert!(!fs.exists(&old_path), "the old-format file must be removed");
    assert!(fs.exists(&new_path), "the new-format file must be written");
    assert_eq!(&fs.read_file(&new_path).unwrap()[..4], b"fLaC");
    let entry = manifest.get("c001").unwrap();
    assert_eq!(entry.format, AudioFormat::Flac);
    assert_eq!(entry.path, new_path);
    assert_eq!(fs.file_count(), 1, "no stale file is left behind");
}

#[test]
fn creator_change_renames_without_retagging() {
    let spec = ClipSpec::mirror("c001", "Wander");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);
    let old_path = path_of(&spec);

    let renamed = spec.with_creator("A New Name");
    let new_path = path_of(&renamed);
    assert_ne!(old_path, new_path);
    let (plan, outcome) = run_clean(std::slice::from_ref(&renamed), &fs, &mut manifest);

    assert_eq!(plan.renames(), 1);
    assert_eq!(plan.retags(), 0, "a path-only change must not retag");
    assert_eq!(outcome.renamed, 1);
    assert!(!fs.exists(&old_path));
    assert!(fs.exists(&new_path));
    assert_eq!(manifest.get("c001").unwrap().path, new_path);
    assert_eq!(fs.file_count(), 1);
}

#[test]
fn title_change_renames_and_retags() {
    let spec = ClipSpec::mirror("c001", "Working Title");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);
    let old_path = path_of(&spec);

    let retitled = spec.with_title("Final Title");
    let new_path = path_of(&retitled);
    let (plan, outcome) = run_clean(std::slice::from_ref(&retitled), &fs, &mut manifest);

    // The title feeds both the path and the metadata sentinel, so the engine
    // both moves the file and refreshes its tags.
    assert_eq!(plan.renames(), 1);
    assert_eq!(plan.retags(), 1);
    assert_eq!(outcome.renamed, 1);
    assert_eq!(outcome.retagged, 1);
    assert!(!fs.exists(&old_path));
    assert!(fs.exists(&new_path));
    assert_eq!(fs.file_count(), 1);
    assert_manifest_matches_disk(&manifest, &fs);
}

#[test]
fn clip_removed_from_a_full_mirror_is_deleted() {
    let specs = [
        ClipSpec::mirror("c001", "Keep"),
        ClipSpec::mirror("c002", "Drop"),
    ];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);
    let dropped_path = path_of(&specs[1]);
    assert!(fs.exists(&dropped_path));

    let remaining = [specs[0].clone()];
    let (plan, outcome) = run_clean(&remaining, &fs, &mut manifest);

    assert_eq!(plan.deletes(), 1);
    assert_eq!(outcome.deleted, 1);
    assert!(!fs.exists(&dropped_path), "the orphan file must be deleted");
    assert!(
        manifest.get("c002").is_none(),
        "the orphan entry must be dropped"
    );
    assert_disk_is(&fs, &remaining);
}

#[test]
fn copy_held_clip_is_never_deleted_when_it_leaves_the_mirror() {
    // The clip is held by both a mirror and a copy source, then leaves the
    // mirror entirely. The copy hold (and the persisted preserve marker) must
    // keep its file even though the mirror is fully enumerated.
    let copy = ClipSpec::mirror("c002", "Archived").copy_held();
    let specs = [ClipSpec::mirror("c001", "Plain"), copy.clone()];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);
    let copy_path = path_of(&copy);
    assert!(
        manifest.get("c002").unwrap().preserve,
        "copy hold must set preserve"
    );

    // Next run lists only the mirror clip; the copy clip is gone from the feed.
    let mirror_only = [ClipSpec::mirror("c001", "Plain")];
    let (plan, outcome) = run_clean(&mirror_only, &fs, &mut manifest);

    assert_eq!(
        plan.deletes(),
        0,
        "a preserved orphan must never be deleted"
    );
    assert_eq!(outcome.deleted, 0);
    assert!(fs.exists(&copy_path), "the copy-held file survives");
    assert!(
        manifest.get("c002").is_some(),
        "the copy-held entry survives"
    );
}

#[test]
fn private_clip_is_preserved_across_removal() {
    let private = ClipSpec::mirror("c002", "Secret").private();
    let specs = [ClipSpec::mirror("c001", "Public"), private.clone()];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);
    let private_path = path_of(&private);
    assert!(
        manifest.get("c002").unwrap().preserve,
        "private must set preserve"
    );

    let public_only = [ClipSpec::mirror("c001", "Public")];
    let (plan, outcome) = run_clean(&public_only, &fs, &mut manifest);

    assert_eq!(plan.deletes(), 0);
    assert_eq!(outcome.deleted, 0);
    assert!(fs.exists(&private_path), "a private file is never deleted");
    assert!(manifest.get("c002").is_some());
}

#[test]
fn trashed_clip_is_deleted_under_the_enumeration_guard() {
    let specs = [
        ClipSpec::mirror("c001", "Live"),
        ClipSpec::mirror("c002", "Doomed"),
    ];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);
    let doomed_path = path_of(&specs[1]);

    let with_trash = [specs[0].clone(), specs[1].clone().trashed()];
    let (plan, outcome) = run_clean(&with_trash, &fs, &mut manifest);

    assert_eq!(plan.deletes(), 1);
    assert_eq!(outcome.deleted, 1);
    assert!(!fs.exists(&doomed_path));
    assert!(manifest.get("c002").is_none());
}

#[test]
fn partial_listing_suppresses_every_delete() {
    let specs = [ClipSpec::mirror("c001", "A"), ClipSpec::mirror("c002", "B")];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);
    let disk_before = fs.paths();

    // The feed is removed entirely, but the mirror source reports it could not
    // be fully enumerated (a failed or truncated listing). No deletes may run.
    let http = world(&[]);
    let unreliable = [SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }];
    let (plan, outcome) = run_sync(&[], &unreliable, &fs, &mut manifest, &http, &fast_opts());

    assert_eq!(plan.deletes(), 0, "a partial listing must suppress deletes");
    assert_eq!(outcome.deleted, 0);
    assert_eq!(fs.paths(), disk_before, "the whole library survives");
    assert_eq!(manifest.len(), 2);
}

#[test]
fn missing_local_file_is_redownloaded_even_when_hashes_match() {
    let spec = ClipSpec::mirror("c001", "Restore");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);
    let path = path_of(&spec);

    // Simulate the file vanishing from disk (manual deletion, lost drive) while
    // the manifest still records it as present and current.
    fs.remove(&path).unwrap();
    assert!(!fs.exists(&path));

    let (plan, outcome) = run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);

    assert_eq!(plan.downloads(), 1, "a vanished file is re-downloaded");
    assert_eq!(outcome.downloaded, 1);
    assert!(fs.exists(&path));
    assert_manifest_matches_disk(&manifest, &fs);
}

#[test]
fn flac_first_sync_renders_transcodes_and_tags() {
    let spec = ClipSpec::mirror("c001", "Lossless").with_format(AudioFormat::Flac);
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let (plan, outcome) = run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);

    assert_eq!(plan.downloads(), 1);
    assert_eq!(outcome.downloaded, 1);
    let path = path_of(&spec);
    assert_eq!(ext(spec.format), "flac");
    assert_eq!(&fs.read_file(&path).unwrap()[..4], b"fLaC");
    assert_eq!(manifest.get("c001").unwrap().format, AudioFormat::Flac);
}

#[test]
fn empty_remote_with_full_mirror_clears_a_tracked_library() {
    // A genuinely empty account that fully enumerates is the one case a mirror
    // is allowed to delete everything; the safety guard lives above reconcile.
    let specs = [ClipSpec::mirror("c001", "A"), ClipSpec::mirror("c002", "B")];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);

    let (plan, outcome) = run_sync(
        &[],
        &clean_mirror(),
        &fs,
        &mut manifest,
        &world(&[]),
        &fast_opts(),
    );

    assert_eq!(plan.deletes(), 2);
    assert_eq!(outcome.deleted, 2);
    assert_eq!(fs.file_count(), 0);
    assert!(manifest.is_empty());
}

#[test]
fn reconcile_plan_is_stable_under_input_reordering() {
    // The end-to-end determinism guarantee the executor relies on: the same
    // selection in any order yields the same plan.
    let specs = [
        ClipSpec::mirror("c003", "Gamma"),
        ClipSpec::mirror("c001", "Alpha"),
        ClipSpec::mirror("c002", "Beta"),
    ];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);

    let forward = desired_set(&specs);
    let mut reversed = forward.clone();
    reversed.reverse();
    let local = probe_local(&manifest, &fs);
    let plan_a = reconcile(&manifest, &forward, &local, &clean_mirror());
    let plan_b = reconcile(&manifest, &reversed, &local, &clean_mirror());
    assert_eq!(plan_a, plan_b);
}

#[test]
fn a_preserved_copy_orphan_is_immortal_after_losing_protection_and_all_sources() {
    // Option A, archive-always-wins (reconcile.rs SYNC-8): a clip that was ever
    // copy-held or private is kept forever once it leaves every source, even if
    // it also loses that protection in the very same transition. This is the
    // intended "immortal preserved orphan", NOT a bug: do not "fix" it into a
    // delete. The keep is asserted by MODEL truth - the clip is gone from the
    // remote and no longer copy-held - never by reading the manifest preserve
    // flag, so the test pins the behaviour rather than restating the mechanism.
    let copy = ClipSpec::mirror("c002", "Archived").copy_held();
    let specs = [ClipSpec::mirror("c001", "Plain"), copy.clone()];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);
    let copy_path = path_of(&copy);
    assert!(
        fs.exists(&copy_path),
        "the copy-held file lands on first sync"
    );

    // ONE transition: the clip both drops its copy hold and leaves all sources.
    // By model truth it is now an ordinary, unprotected clip that no longer
    // exists remotely; only the permanent archive latch may keep it.
    let remaining = [ClipSpec::mirror("c001", "Plain")];

    // Two clean, fully-enumerated passes - exactly the runs that ARE allowed to
    // delete. Each must still keep the orphan: a delete is never even planned,
    // and the file and its manifest entry both remain.
    for pass in 1..=2 {
        let (plan, outcome) = run_clean(&remaining, &fs, &mut manifest);
        assert_eq!(
            plan.deletes(),
            0,
            "pass {pass}: a preserved orphan must never be planned for deletion"
        );
        assert_eq!(outcome.deleted, 0, "pass {pass}: nothing may be deleted");
        assert!(
            fs.exists(&copy_path),
            "pass {pass}: the preserved orphan file is intentionally immortal"
        );
        assert!(
            manifest.get("c002").is_some(),
            "pass {pass}: the preserved orphan manifest entry is retained forever"
        );
    }
}

#[test]
fn a_preserved_private_orphan_is_immortal_after_losing_protection_and_all_sources() {
    // The private twin of the copy-orphan immortality test (Option A). A clip
    // that was private is latched into the archive; dropping privacy AND leaving
    // every source in one transition still keeps it forever. Asserted by model
    // truth, not the manifest preserve flag.
    let private = ClipSpec::mirror("c002", "Secret").private();
    let specs = [ClipSpec::mirror("c001", "Public"), private.clone()];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);
    let private_path = path_of(&private);
    assert!(
        fs.exists(&private_path),
        "the private file lands on first sync"
    );

    let remaining = [ClipSpec::mirror("c001", "Public")];
    for pass in 1..=2 {
        let (plan, outcome) = run_clean(&remaining, &fs, &mut manifest);
        assert_eq!(
            plan.deletes(),
            0,
            "pass {pass}: a preserved private orphan is never planned for deletion"
        );
        assert_eq!(outcome.deleted, 0, "pass {pass}: nothing may be deleted");
        assert!(
            fs.exists(&private_path),
            "pass {pass}: the preserved private orphan is intentionally immortal"
        );
        assert!(
            manifest.get("c002").is_some(),
            "pass {pass}: the preserved private orphan entry is retained forever"
        );
    }
}

#[test]
fn partial_copy_source_suppresses_delete_end_to_end() {
    // End-to-end copy-vs-mirror suppression (SYNC-9). The mirror is fully
    // enumerated and would normally delete an orphan, but a second selected
    // source - a copy source - could not be fully enumerated this run. Deletion
    // is allowed only when EVERY selected source is reliable, so the whole run
    // must suppress deletes even though the mirror listing itself was complete.
    let specs = [
        ClipSpec::mirror("c001", "Keep"),
        ClipSpec::mirror("c002", "Maybe"),
    ];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);
    let maybe_path = path_of(&specs[1]);

    // The feed now lists only c001, but the copy source is unreliable, so c002
    // is an orphan that must NOT be deleted.
    let remaining = [ClipSpec::mirror("c001", "Keep")];
    let sources = [
        SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: true,
        },
        SourceStatus {
            mode: SourceMode::Copy,
            fully_enumerated: false,
        },
    ];
    let http = world(&remaining);
    let (plan, outcome) = run_sync(
        &remaining,
        &sources,
        &fs,
        &mut manifest,
        &http,
        &fast_opts(),
    );

    assert_eq!(
        plan.deletes(),
        0,
        "an unreliable copy source must suppress every delete"
    );
    assert_eq!(outcome.deleted, 0);
    assert!(
        fs.exists(&maybe_path),
        "the orphan survives the unreliable copy run"
    );
    assert!(manifest.get("c002").is_some());
}

#[test]
fn copy_held_trashed_clip_survives_end_to_end() {
    // SYNC-12 versus SYNC-8 across the whole pipeline: trashing a clip normally
    // deletes its file, but a copy hold outranks the trash. The copy-held source
    // is threaded end to end via the harness `sources_for`, so the trashed,
    // copy-held clip is kept while a trashed mirror-only clip beside it is
    // deleted in the same run.
    let keep = ClipSpec::mirror("c002", "Trashed but archived").copy_held();
    let specs = [ClipSpec::mirror("c001", "Doomed"), keep.clone()];
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(&specs, &fs, &mut manifest);
    let keep_path = path_of(&keep);
    let doomed_path = path_of(&specs[0]);

    // Both clips are trashed in the same run; only the unprotected one dies.
    let trashed = [
        ClipSpec::mirror("c001", "Doomed").trashed(),
        keep.clone().trashed(),
    ];
    let (plan, outcome) = run_clean(&trashed, &fs, &mut manifest);

    assert_eq!(
        plan.deletes(),
        1,
        "only the unprotected trashed clip is deleted"
    );
    assert_eq!(outcome.deleted, 1);
    assert!(
        !fs.exists(&doomed_path),
        "the mirror-only trashed clip is deleted"
    );
    assert!(
        fs.exists(&keep_path),
        "the copy-held trashed clip is kept end to end"
    );
    assert!(
        manifest.get("c002").is_some(),
        "the copy-held trashed clip's manifest entry is retained"
    );
}
