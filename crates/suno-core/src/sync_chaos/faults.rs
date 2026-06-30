//! Layer 3: fault injection across the network, disk, and transcode ports.
//!
//! The whole point of a download tool is that a failure is *safe*: a dropped
//! connection, a rate limit, a lying disk, or a refused delete must never lose
//! or corrupt a file that was already good, and must never advance the manifest
//! past work that did not actually land. These tests drive the real pipeline
//! with a [`ChaosHttp`] origin that fails on a schedule and a [`MemFs`] that can
//! refuse or corrupt specific writes and removes, and assert the engine holds
//! the line: failures are recorded and skipped, the run continues, an auth
//! failure aborts cleanly, and transient errors back off and recover.

use std::time::Duration;

use super::harness::{
    ClipSpec, clean_mirror, desired_set, fast_opts, path_of, probe_local, run_clean, run_sync,
    world,
};
use crate::auth::ClerkAuth;
use crate::client::SunoClient;
use crate::config::AudioFormat;
use crate::executor::{ExecOptions, ExecOutcome, Ports, RunStatus, execute};
use crate::manifest::Manifest;
use crate::reconcile::{Desired, Plan, reconcile};
use crate::testutil::{ChaosHttp, MemFs, Outcome, RecordingClock, StubFfmpeg};

/// An MP3 origin that serves `spec`'s cover art cleanly but runs `audio` as the
/// programmed outcome sequence for the audio GET, so the only injected fault is
/// on the audio download itself. The MP3 path needs no auth or render routes.
fn mp3_origin(spec: &ClipSpec, audio: Vec<Outcome>) -> ChaosHttp {
    let mut http = ChaosHttp::new().program(&format!("/{}.mp3", spec.id), audio);
    if !spec.art.is_empty() {
        http = http.serve(&spec.art, format!("cover-{}", spec.id).into_bytes());
    }
    http
}

/// Drive a plan like the harness does, but also return the backoff/poll delays
/// the recording clock observed, so a test can prove a retry actually waited.
fn drive_capturing(
    plan: &Plan,
    manifest: &mut Manifest,
    desired: &[Desired],
    http: &ChaosHttp,
    fs: &MemFs,
    opts: &ExecOptions,
) -> (ExecOutcome, Vec<Duration>) {
    let mut client = SunoClient::new(ClerkAuth::new("eyJtoken"));
    let clock = RecordingClock::new();
    let ffmpeg = StubFfmpeg::flac();
    let outcome = pollster::block_on(execute(
        plan,
        manifest,
        desired,
        Ports {
            client: &mut client,
            http,
            fs,
            ffmpeg: &ffmpeg,
            clock: &clock,
        },
        opts,
    ));
    (outcome, clock.sleeps())
}

/// Run a full clean-source sync, returning the plan, outcome, and clock delays.
fn sync_capturing(
    specs: &[ClipSpec],
    fs: &MemFs,
    manifest: &mut Manifest,
    http: &ChaosHttp,
) -> (Plan, ExecOutcome, Vec<Duration>) {
    let desired = desired_set(specs);
    let local = probe_local(manifest, fs);
    let plan = reconcile(manifest, &desired, &local, &clean_mirror());
    let (outcome, sleeps) = drive_capturing(&plan, manifest, &desired, http, fs, &fast_opts());
    (plan, outcome, sleeps)
}

#[test]
fn a_permanent_download_failure_never_advances_the_manifest() {
    let spec = ClipSpec::mirror("c100", "Lost Signal");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    // A 404 on the audio is a permanent fetch failure.
    let http = mp3_origin(&spec, vec![Outcome::status(404)]);

    let (_plan, outcome) = run_sync(
        std::slice::from_ref(&spec),
        &clean_mirror(),
        &fs,
        &mut manifest,
        &http,
        &fast_opts(),
    );

    assert_eq!(outcome.downloaded, 0);
    assert_eq!(outcome.failed(), 1);
    assert!(
        manifest.get("c100").is_none(),
        "manifest must not record a failed download"
    );
    assert!(
        !fs.exists(&path_of(&spec)),
        "no partial file must be left behind"
    );
}

#[test]
fn a_truncated_download_is_rejected_and_never_advances_the_manifest() {
    let spec = ClipSpec::mirror("c101", "Half A Song");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    // The body is shorter than its advertised length on every attempt.
    let http = mp3_origin(&spec, vec![Outcome::truncated(b"short".to_vec(), 9_999)]);

    let (_plan, outcome) = run_sync(
        std::slice::from_ref(&spec),
        &clean_mirror(),
        &fs,
        &mut manifest,
        &http,
        &fast_opts(),
    );

    assert_eq!(outcome.downloaded, 0);
    assert_eq!(outcome.failed(), 1);
    assert!(
        manifest.get("c101").is_none(),
        "a truncated download must not be recorded"
    );
    assert!(
        !fs.exists(&path_of(&spec)),
        "a truncated body must never be written"
    );
}

#[test]
fn a_failed_write_leaves_the_existing_good_file_untouched() {
    let mut spec = ClipSpec::mirror("c102", "Steady");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);
    let path = path_of(&spec);
    let good_bytes = fs.read_file(&path).expect("first sync wrote the file");
    let good_hash = manifest.get("c102").unwrap().meta_hash.clone();

    // A metadata change asks for a retag, but the disk refuses the write.
    spec = spec.with_tags("a brand new mood");
    fs.arm_fail_write(&path);
    let (plan, outcome) = run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);

    assert_eq!(
        plan.retags(),
        1,
        "the change should still be planned as a retag"
    );
    assert_eq!(outcome.retagged, 0);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(
        fs.read_file(&path).unwrap(),
        good_bytes,
        "the good file must be byte-for-byte intact"
    );
    assert_eq!(
        &manifest.get("c102").unwrap().meta_hash,
        &good_hash,
        "a failed retag must not advance the stored hash",
    );

    // With the disk healed, the next run completes the retag.
    fs.disarm_fail_write(&path);
    let (_plan, outcome) = run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);
    assert_eq!(
        outcome.retagged, 1,
        "the retag recovers once the disk works"
    );
    assert_ne!(
        fs.read_file(&path).unwrap(),
        good_bytes,
        "the recovered run re-tags the file"
    );
}

#[test]
fn a_corrupt_write_is_caught_and_never_advances_the_manifest() {
    let spec = ClipSpec::mirror("c103", "Bit Rot");
    // The disk silently stores the wrong number of bytes for this path.
    let fs = MemFs::new().corrupt_write(&path_of(&spec));
    let mut manifest = Manifest::new();

    let (_plan, outcome) = run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);

    assert_eq!(
        outcome.downloaded, 0,
        "the size check must reject a corrupt write"
    );
    assert_eq!(outcome.failed(), 1);
    assert!(
        manifest.get("c103").is_none(),
        "a download whose size verify failed must not be recorded",
    );
}

#[test]
fn a_failed_delete_keeps_the_manifest_entry_and_the_file() {
    let mut spec = ClipSpec::mirror("c104", "Keep Me");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);
    let path = path_of(&spec);

    // The clip is trashed, so a clean run wants to delete it, but the disk
    // refuses the remove.
    spec = spec.trashed();
    fs.arm_fail_remove(&path);
    let (plan, outcome) = run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);

    assert_eq!(
        plan.deletes(),
        1,
        "a trashed clip on a clean mirror is a delete"
    );
    assert_eq!(outcome.deleted, 0);
    assert_eq!(outcome.failed(), 1);
    assert!(
        fs.exists(&path),
        "a refused delete must leave the file in place"
    );
    assert!(
        manifest.get("c104").is_some(),
        "a refused delete must keep the manifest entry so the next run retries",
    );

    // Healed, the next run completes the delete.
    fs.disarm_fail_remove(&path);
    let (_plan, outcome) = run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);
    assert_eq!(
        outcome.deleted, 1,
        "the delete recovers once the disk works"
    );
    assert!(!fs.exists(&path));
    assert!(manifest.get("c104").is_none());
}

#[test]
fn one_clips_failure_never_aborts_the_others() {
    // The first clip in the plan fails permanently; the second must still land.
    let bad = ClipSpec::mirror("c200", "Broken");
    let good = ClipSpec::mirror("c201", "Whole");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let http = ChaosHttp::new()
        .program("/c200.mp3", vec![Outcome::status(404)])
        .serve("/c201.mp3", b"good-audio".to_vec())
        .serve(&good.art, b"good-art".to_vec());

    let specs = [bad.clone(), good.clone()];
    let (_plan, outcome) = run_sync(
        &specs,
        &clean_mirror(),
        &fs,
        &mut manifest,
        &http,
        &fast_opts(),
    );

    assert_eq!(
        outcome.downloaded, 1,
        "the healthy clip downloads despite the failure"
    );
    assert_eq!(outcome.failed(), 1);
    assert!(
        manifest.get("c200").is_none(),
        "the failed clip is not recorded"
    );
    assert!(
        manifest.get("c201").is_some(),
        "the healthy clip is recorded"
    );
    assert!(fs.exists(&path_of(&good)));
    assert!(!fs.exists(&path_of(&bad)));
}

#[test]
fn an_auth_failure_aborts_the_run_cleanly_and_stops_further_work() {
    // The first clip's download is rejected for auth; the run must abort before
    // the second clip is even requested.
    let first = ClipSpec::mirror("c300", "Gatekeeper");
    let second = ClipSpec::mirror("c301", "Never Reached");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();

    let http = ChaosHttp::new()
        .program("/c300.mp3", vec![Outcome::status(401)])
        .serve("/c301.mp3", b"audio".to_vec());

    let specs = [first.clone(), second.clone()];
    let (_plan, outcome) = run_sync(
        &specs,
        &clean_mirror(),
        &fs,
        &mut manifest,
        &http,
        &fast_opts(),
    );

    assert_eq!(outcome.status, RunStatus::AuthAborted);
    assert_eq!(outcome.downloaded, 0);
    assert_eq!(outcome.failed(), 1);
    assert_eq!(
        http.count("/c301.mp3"),
        0,
        "an auth abort must stop the run before later clips are touched",
    );
    assert!(manifest.is_empty(), "an aborted run records nothing");
}

#[test]
fn a_transient_download_error_backs_off_then_recovers() {
    let spec = ClipSpec::mirror("c400", "Flaky CDN");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    // Two transient failures (a reset, then a 500) before the body arrives.
    let http = mp3_origin(
        &spec,
        vec![
            Outcome::transport("connection reset"),
            Outcome::status(500),
            Outcome::ok(b"recovered-audio".to_vec()),
        ],
    );

    let (_plan, outcome, sleeps) =
        sync_capturing(std::slice::from_ref(&spec), &fs, &mut manifest, &http);

    assert_eq!(
        outcome.downloaded, 1,
        "the download recovers after transient errors"
    );
    assert_eq!(outcome.failed(), 0);
    assert!(fs.exists(&path_of(&spec)));
    assert_eq!(
        sleeps,
        vec![Duration::from_secs(1), Duration::from_secs(2)],
        "each retry must back off with doubling delays",
    );
}

#[test]
fn a_rate_limit_is_transient_and_recovers() {
    let spec = ClipSpec::mirror("c401", "Throttled");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    let http = mp3_origin(
        &spec,
        vec![Outcome::status(429), Outcome::ok(b"audio".to_vec())],
    );

    let (_plan, outcome, sleeps) =
        sync_capturing(std::slice::from_ref(&spec), &fs, &mut manifest, &http);

    assert_eq!(outcome.downloaded, 1, "a 429 is retried, not fatal");
    assert_eq!(outcome.failed(), 0);
    assert_eq!(
        sleeps,
        vec![Duration::from_secs(1)],
        "one backoff before the retry"
    );
}

#[test]
fn a_failed_reformat_write_keeps_the_old_file_and_format() {
    let mut spec = ClipSpec::mirror("c500", "Reshape");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);
    let old_path = path_of(&spec);
    let old_bytes = fs.read_file(&old_path).expect("first sync wrote the mp3");

    // Switch to FLAC, but the write of the new rendering is refused.
    spec = spec.with_format(AudioFormat::Flac);
    let new_path = path_of(&spec);
    fs.arm_fail_write(&new_path);
    let http = world(std::slice::from_ref(&spec));
    let (plan, outcome) = run_sync(
        std::slice::from_ref(&spec),
        &clean_mirror(),
        &fs,
        &mut manifest,
        &http,
        &fast_opts(),
    );

    assert_eq!(plan.reformats(), 1);
    assert_eq!(outcome.reformatted, 0);
    assert_eq!(outcome.failed(), 1);
    assert!(
        fs.exists(&old_path),
        "the old file must survive a failed reformat"
    );
    assert_eq!(
        fs.read_file(&old_path).unwrap(),
        old_bytes,
        "the old file is untouched"
    );
    assert!(
        !fs.exists(&new_path),
        "no new file must be left after a failed write"
    );
    let entry = manifest.get("c500").expect("the entry survives");
    assert_eq!(
        entry.format,
        AudioFormat::Mp3,
        "the manifest still tracks the old format"
    );
    assert_eq!(
        entry.path, old_path,
        "the manifest still points at the old file"
    );
}

#[test]
fn a_failed_reformat_remove_keeps_the_manifest_on_the_intact_old_file() {
    let mut spec = ClipSpec::mirror("c501", "Lingering");
    let fs = MemFs::new();
    let mut manifest = Manifest::new();
    run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);
    let old_path = path_of(&spec);
    let old_bytes = fs.read_file(&old_path).expect("first sync wrote the mp3");

    // Switch to FLAC; the new file writes, but removing the old one is refused.
    spec = spec.with_format(AudioFormat::Flac);
    let new_path = path_of(&spec);
    fs.arm_fail_remove(&old_path);
    let http = world(std::slice::from_ref(&spec));
    let (_plan, outcome) = run_sync(
        std::slice::from_ref(&spec),
        &clean_mirror(),
        &fs,
        &mut manifest,
        &http,
        &fast_opts(),
    );

    assert_eq!(outcome.reformatted, 0);
    assert_eq!(outcome.failed(), 1);
    // The manifest must not have advanced past the un-removed old file: it still
    // tracks the intact MP3, which is the only safe state to retry from.
    let entry = manifest.get("c501").expect("the entry survives");
    assert_eq!(
        entry.format,
        AudioFormat::Mp3,
        "the manifest stays on the old format"
    );
    assert_eq!(entry.path, old_path);
    assert!(fs.exists(&old_path), "the old, tracked file is intact");
    assert_eq!(fs.read_file(&old_path).unwrap(), old_bytes);

    // Healed, a re-sync completes the reformat and converges to FLAC only.
    fs.disarm_fail_remove(&old_path);
    let (_plan, outcome) = run_clean(std::slice::from_ref(&spec), &fs, &mut manifest);
    assert_eq!(
        outcome.reformatted, 1,
        "the reformat completes once the remove works"
    );
    assert!(!fs.exists(&old_path), "the old file is finally gone");
    assert!(fs.exists(&new_path), "the new FLAC is in place");
    assert_eq!(manifest.get("c501").unwrap().format, AudioFormat::Flac);
}
