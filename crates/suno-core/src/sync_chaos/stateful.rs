//! Layer 2: a property-based state machine over random multi-run sequences.
//!
//! This is the layer single-shot tests miss. It keeps a small in-memory library
//! (one persistent disk and manifest) and applies a random sequence of *runs*,
//! each preceded by random remote mutations (add, remove, retitle, retag, change
//! creator or art, toggle copy-hold, private, trashed, or format) and using a
//! random listing reliability (a clean full listing, an unreliable partial
//! listing, or a failed empty listing). After every run it asserts the
//! library-integrity invariants hold:
//!
//! - **I-a** a clip that is genuinely protected and present before a run is
//!   still present after it; protection is never lost to a delete. "Genuinely
//!   protected" means copy-held or private while still listed, or an orphan kept
//!   by its preserve marker. That last case is intentional and permanent: under
//!   the archive-always-wins design (Option A, see `reconcile.rs`), a clip that
//!   was ever copy-held or private is kept forever once deselected, even after
//!   it also loses that protection. `full_sync.rs` pins this immortality with a
//!   model-truth test rather than letting it hide inside the manifest's flag.
//! - **I-b** a run whose listing is not fully enumerated performs no deletes.
//! - **I-c** a failed empty listing never shrinks the library.
//! - **I-d** clean, fully-enumerated runs converge to a *manifest* fixed point:
//!   the preserve latch (SYNC-8) may defer a delete by one run, so within two
//!   passes a re-plan is a pure no-op and the on-disk set is exactly the tracked
//!   set. This is convergence to what the manifest should hold, not to "source
//!   truth": preserved orphans are deliberately retained and never deleted to
//!   match the feed, so I-d makes no claim that the library mirrors the source.
//! - **I-e** every manifest entry references a file that exists on disk with the
//!   recorded size (and a clean run never leaves a failed action behind).
//!
//! No network, disk, or transcode faults are injected here; Layer 3 owns those.
//! Keeping this layer fault-free is what lets the invariants be exact.

use std::collections::{BTreeMap, BTreeSet};

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::harness::{
    ClipSpec, desired_set, fast_opts, mutating_actions, probe_local, run_sync, sources_for, world,
};
use crate::fs::Filesystem;
use crate::manifest::Manifest;
use crate::reconcile::{SourceStatus, reconcile};
use crate::testutil::MemFs;
use crate::vocab::{AudioFormat, SourceMode};

/// The shared id space; small, so adds and removes overlap and every action gets
/// exercised. Fixed-width ids never substring-collide in the origin's routes.
const IDS: u8 = 6;

fn id_of(n: u8) -> String {
    format!("c{n:03}")
}

/// The model's record of one remote clip. The `*_rev` counters stand in for
/// edits: bumping one changes the corresponding rendered field, which the engine
/// detects as a rename or a retag.
#[derive(Clone, Debug)]
struct ModelClip {
    title_rev: u32,
    creator_rev: u32,
    tags_rev: u32,
    art_rev: u32,
    copy: bool,
    private: bool,
    trashed: bool,
    format: AudioFormat,
}

impl ModelClip {
    fn fresh() -> Self {
        Self {
            title_rev: 0,
            creator_rev: 0,
            tags_rev: 0,
            art_rev: 0,
            copy: false,
            private: false,
            trashed: false,
            format: AudioFormat::Mp3,
        }
    }
}

type Model = BTreeMap<String, ModelClip>;

/// Render one model clip into the harness spec the engine consumes.
fn spec_of(id: &str, clip: &ModelClip) -> ClipSpec {
    let mut modes = vec![SourceMode::Mirror];
    if clip.copy {
        modes.push(SourceMode::Copy);
    }
    ClipSpec {
        id: id.to_owned(),
        title: format!("title-{}", clip.title_rev),
        creator: format!("creator-{}", clip.creator_rev),
        tags: format!("tags-{}", clip.tags_rev),
        art: format!("https://cdn1.suno.ai/{id}-art{}.jpeg", clip.art_rev),
        format: clip.format,
        modes,
        trashed: clip.trashed,
        private: clip.private,
    }
}

fn specs_of(model: &Model) -> Vec<ClipSpec> {
    model.iter().map(|(id, c)| spec_of(id, c)).collect()
}

/// One random edit to the remote between runs.
#[derive(Clone, Debug)]
enum Mutation {
    Add(u8),
    Remove(u8),
    BumpTags(u8),
    BumpTitle(u8),
    BumpCreator(u8),
    BumpArt(u8),
    ToggleCopy(u8),
    TogglePrivate(u8),
    ToggleTrashed(u8),
    ToggleFormat(u8),
}

fn apply(model: &mut Model, mutation: &Mutation) {
    match mutation {
        Mutation::Add(n) => {
            model.entry(id_of(*n)).or_insert_with(ModelClip::fresh);
        }
        Mutation::Remove(n) => {
            model.remove(&id_of(*n));
        }
        Mutation::BumpTags(n) => with_clip(model, *n, |c| c.tags_rev += 1),
        Mutation::BumpTitle(n) => with_clip(model, *n, |c| c.title_rev += 1),
        Mutation::BumpCreator(n) => with_clip(model, *n, |c| c.creator_rev += 1),
        Mutation::BumpArt(n) => with_clip(model, *n, |c| c.art_rev += 1),
        Mutation::ToggleCopy(n) => with_clip(model, *n, |c| c.copy = !c.copy),
        Mutation::TogglePrivate(n) => with_clip(model, *n, |c| c.private = !c.private),
        Mutation::ToggleTrashed(n) => with_clip(model, *n, |c| c.trashed = !c.trashed),
        Mutation::ToggleFormat(n) => with_clip(model, *n, |c| {
            c.format = match c.format {
                AudioFormat::Mp3 => AudioFormat::Flac,
                _ => AudioFormat::Mp3,
            }
        }),
    }
}

fn with_clip(model: &mut Model, n: u8, edit: impl FnOnce(&mut ModelClip)) {
    if let Some(clip) = model.get_mut(&id_of(n)) {
        edit(clip);
    }
}

/// The listing reliability for one run.
#[derive(Clone, Debug)]
enum RunMode {
    /// A full, fully-enumerated mirror: deletes allowed, must converge.
    Clean,
    /// The current selection, but the mirror could not be fully enumerated.
    PartialListing,
    /// An empty, failed listing: nothing returned and not fully enumerated.
    FailedEmptyListing,
}

/// One step: edits to apply, then a run in the chosen mode.
#[derive(Clone, Debug)]
struct Step {
    mutations: Vec<Mutation>,
    mode: RunMode,
}

fn mutation() -> impl Strategy<Value = Mutation> {
    let id = || 0u8..IDS;
    prop_oneof![
        id().prop_map(Mutation::Add),
        id().prop_map(Mutation::Remove),
        id().prop_map(Mutation::BumpTags),
        id().prop_map(Mutation::BumpTitle),
        id().prop_map(Mutation::BumpCreator),
        id().prop_map(Mutation::BumpArt),
        id().prop_map(Mutation::ToggleCopy),
        id().prop_map(Mutation::TogglePrivate),
        id().prop_map(Mutation::ToggleTrashed),
        id().prop_map(Mutation::ToggleFormat),
    ]
}

fn run_mode() -> impl Strategy<Value = RunMode> {
    // Clean runs dominate so the library makes progress and converges between
    // the occasional unreliable listing.
    prop_oneof![
        3 => Just(RunMode::Clean),
        1 => Just(RunMode::PartialListing),
        1 => Just(RunMode::FailedEmptyListing),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    (vec(mutation(), 0..4), run_mode()).prop_map(|(mutations, mode)| Step { mutations, mode })
}

fn script() -> impl Strategy<Value = Vec<Step>> {
    vec(step(), 1..12)
}

/// Is the clip currently tracked and backed by a real file on disk?
fn present(manifest: &Manifest, fs: &MemFs, id: &str) -> bool {
    manifest
        .get(id)
        .is_some_and(|entry| fs.metadata(&entry.path).is_some())
}

/// The ids that are *genuinely* protected and currently present on disk: these
/// must survive the run. A still-listed clip is protected only by its live
/// copy-held or private status; a deselected orphan is protected by its
/// persisted preserve marker (SYNC-8). A stale preserve marker on a still-listed
/// clip that has *lost* protection is deliberately not counted, because the
/// engine is allowed to (eventually) delete such a clip.
fn protected_present(manifest: &Manifest, fs: &MemFs, specs: &[ClipSpec]) -> BTreeSet<String> {
    let desired_ids: BTreeSet<&str> = specs.iter().map(|s| s.id.as_str()).collect();
    let live_protected: BTreeSet<&str> = specs
        .iter()
        .filter(|s| s.private || s.modes.contains(&SourceMode::Copy))
        .map(|s| s.id.as_str())
        .collect();
    manifest
        .iter()
        .filter(|(id, entry)| {
            let protected = if desired_ids.contains(id.as_str()) {
                live_protected.contains(id.as_str())
            } else {
                entry.preserve
            };
            protected && fs.metadata(&entry.path).is_some()
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// I-a: every protected, present-before clip is still present after the run.
fn assert_protected_survive(
    manifest: &Manifest,
    fs: &MemFs,
    protected: &BTreeSet<String>,
) -> Result<(), TestCaseError> {
    for id in protected {
        prop_assert!(
            present(manifest, fs, id),
            "protected clip {id} lost its file or manifest entry"
        );
    }
    Ok(())
}

/// I-e: every manifest entry references an existing file of the recorded size.
fn assert_manifest_disk_consistent(manifest: &Manifest, fs: &MemFs) -> Result<(), TestCaseError> {
    for (id, entry) in manifest.iter() {
        let stat = fs.metadata(&entry.path);
        prop_assert!(
            stat.is_some(),
            "manifest entry {id} points at a missing file {}",
            entry.path
        );
        prop_assert_eq!(
            stat.unwrap().size,
            entry.size,
            "manifest size disagrees with disk for {}",
            id
        );
    }
    Ok(())
}

/// Drive one whole script against a single fresh library and assert every
/// invariant after each run. Factored out of the proptest so the random search
/// and the deterministic high-value script below share identical checks: the
/// crafted test then guarantees the delete, protect, and unprotect-then-orphan
/// transitions are exercised, answering "could the random run pass vacuously?".
fn check_script(script: &[Step]) -> Result<(), TestCaseError> {
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    let mut model: Model = BTreeMap::new();

    for step in script {
        for mutation in &step.mutations {
            apply(&mut model, mutation);
        }
        let specs = specs_of(&model);
        let protected = protected_present(&manifest, &fs, &specs);
        let disk_before = fs.paths();

        match step.mode {
            RunMode::Clean => {
                // Sources are derived from the specs' modes, so a copy-held set
                // presents a fully-enumerated copy source alongside the mirror,
                // exactly as the CLI would build it.
                let sources = sources_for(&specs);
                let http = world(&specs);
                let (_plan, outcome) =
                    run_sync(&specs, &sources, &fs, &mut manifest, &http, &fast_opts());
                // A clean origin never fails an action.
                prop_assert_eq!(outcome.failed(), 0, "clean run had failures");
                // The preserve latch (SYNC-8) can defer a delete by one run
                // when a still-listed clip loses copy/private protection, so
                // the engine is allowed up to two clean passes to settle. A
                // second pass must then introduce no new failures.
                let (_p2, o2) = run_sync(&specs, &sources, &fs, &mut manifest, &http, &fast_opts());
                prop_assert_eq!(o2.failed(), 0, "second clean run had failures");
                // I-d: the engine has converged to a manifest fixed point. A
                // further re-plan is inert, and the disk holds exactly the
                // tracked files (with I-e this is a disk/manifest bijection).
                // Preserved orphans stay in both sets on purpose (Option A).
                let local = probe_local(&manifest, &fs);
                let replan = reconcile(&manifest, &desired_set(&specs), &local, &sources);
                prop_assert_eq!(
                    mutating_actions(&replan),
                    0,
                    "clean runs did not converge within two passes: {:?}",
                    replan.actions
                );
                let mut tracked: Vec<String> =
                    manifest.iter().map(|(_, e)| e.path.clone()).collect();
                tracked.sort();
                prop_assert_eq!(
                    fs.paths(),
                    tracked,
                    "converged disk is not exactly the tracked set"
                );
            }
            RunMode::PartialListing => {
                let sources = [SourceStatus {
                    mode: SourceMode::Mirror,
                    fully_enumerated: false,
                }];
                let http = world(&specs);
                let (plan, outcome) =
                    run_sync(&specs, &sources, &fs, &mut manifest, &http, &fast_opts());
                // I-b: an unreliable listing deletes nothing.
                prop_assert_eq!(plan.deletes(), 0, "partial listing planned a delete");
                prop_assert_eq!(outcome.deleted, 0, "partial listing executed a delete");
            }
            RunMode::FailedEmptyListing => {
                let sources = [SourceStatus {
                    mode: SourceMode::Mirror,
                    fully_enumerated: false,
                }];
                let (plan, outcome) =
                    run_sync(&[], &sources, &fs, &mut manifest, &world(&[]), &fast_opts());
                // I-c: a failed empty listing never shrinks the library.
                prop_assert_eq!(plan.deletes(), 0, "failed empty listing planned a delete");
                prop_assert_eq!(outcome.deleted, 0, "failed empty listing executed a delete");
                prop_assert_eq!(
                    fs.paths(),
                    disk_before,
                    "failed empty listing changed the disk"
                );
            }
        }

        // I-a and I-e hold after every run, whatever its mode.
        assert_protected_survive(&manifest, &fs, &protected)?;
        assert_manifest_disk_consistent(&manifest, &fs)?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn library_integrity_holds_across_random_runs(script in script()) {
        check_script(&script)?;
    }
}

/// A deterministic script that is guaranteed to drive the three highest-value
/// transitions, so the suite can never pass them vacuously: (1) a plain mirror
/// clip is deleted once trashed; (2) a copy-held and a private clip are kept
/// while protected; (3) both then lose protection *and* leave all sources in the
/// same step and are still kept forever as preserved orphans (Option A). All of
/// I-a..I-e are asserted after every run by the shared [`check_script`].
#[test]
fn library_integrity_holds_for_a_crafted_high_value_script() {
    let clean = |mutations: Vec<Mutation>| Step {
        mutations,
        mode: RunMode::Clean,
    };
    let script = vec![
        // Populate: a plain mirror clip, a copy-held clip, and a private clip.
        clean(vec![
            Mutation::Add(0),
            Mutation::Add(1),
            Mutation::Add(2),
            Mutation::ToggleCopy(1),
            Mutation::TogglePrivate(2),
        ]),
        // Trash the plain clip: it loses its only claim and is deleted (the
        // delete transition).
        clean(vec![Mutation::ToggleTrashed(0)]),
        // In one transition, drop the copy hold and privacy *and* remove both
        // from every source. They became preserved orphans, so they are
        // intentionally immortal and must survive (the unprotect+orphan path).
        clean(vec![
            Mutation::ToggleCopy(1),
            Mutation::TogglePrivate(2),
            Mutation::Remove(1),
            Mutation::Remove(2),
        ]),
        // A further clean run still keeps the immortal orphans.
        clean(vec![]),
    ];
    check_script(&script).expect("crafted high-value script holds every invariant");
}
