//! Direct truth-table tests for the two shared deletion-gate predicates
//! ([`delete_gate_open`] and [`clip_owned_delete_open`]) that the four per-kind
//! delete gates now route through. Deletion safety is the engine's #1 invariant,
//! so the shared floor is pinned here at the unit level as well as through the
//! per-gate scenarios and the property suite.

use super::*;

#[test]
fn delete_gate_open_requires_the_verdict_and_a_nonempty_path() {
    // The run verdict is mandatory: no deletion-enabled run, no delete.
    assert!(!delete_gate_open(false, "a/cover.jpg"));
    // An empty path can never delete the account root, even with the verdict.
    assert!(!delete_gate_open(true, ""));
    // Verdict armed and a real path: the shared floor is open.
    assert!(delete_gate_open(true, "a/cover.jpg"));
}

#[test]
fn clip_owned_delete_open_requires_a_live_unpreserved_owner() {
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a/song.flac", AudioFormat::Flac, "m", "art"));
    manifest.insert(
        "p",
        preserved_entry("p/song.flac", AudioFormat::Flac, "m", "art"),
    );

    // Verdict off: keep, regardless of the owning entry.
    assert!(!clip_owned_delete_open(
        "a",
        "a/cover.jpg",
        &manifest,
        false
    ));
    // Empty path: keep.
    assert!(!clip_owned_delete_open("a", "", &manifest, true));
    // No manifest entry for the owner: keep (an untracked owner is never
    // delete-reconciled).
    assert!(!clip_owned_delete_open(
        "missing",
        "missing/cover.jpg",
        &manifest,
        true
    ));
    // Preserved owner: keep (its sidecars and stems are preserved too).
    assert!(!clip_owned_delete_open("p", "p/cover.jpg", &manifest, true));
    // Live, unpreserved owner with a real path and the verdict: floor open.
    assert!(clip_owned_delete_open("a", "a/cover.jpg", &manifest, true));
}
