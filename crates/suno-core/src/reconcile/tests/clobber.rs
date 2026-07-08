//! Write-clobber suppression: a rename, download, or move must never overwrite
//! a path another clip's file still holds, so a rename can never become a delete
//! of a protected clip by the back door. These lock the P1 deletion-safety fix.

use super::*;

fn mirror_narrowed() -> Vec<SourceStatus> {
    vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: false,
    }]
}

#[test]
fn rename_onto_a_preserved_clip_is_suppressed_not_a_clobber() {
    // Clip `a` renders onto the path copy-held clip `b` still occupies (b is
    // narrowed out of this selection). The old code emitted Rename a->b.mp3,
    // destroying b's file with zero deletes in the plan. The rename must be
    // suppressed to a Skip; b's file is untouched.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.mp3", AudioFormat::Mp3, "m", "art"));
    manifest.insert("b", preserved_entry("b.mp3", AudioFormat::Mp3, "m", "art"));

    let d = desired("a", "b.mp3", AudioFormat::Mp3, "m", "art");
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());

    assert_eq!(
        plan.renames(),
        0,
        "the clobbering rename must be suppressed"
    );
    assert!(
        !plan
            .actions
            .iter()
            .any(|a| matches!(a, Action::Delete { .. })),
        "no delete either: b is preserved",
    );
    assert_eq!(
        plan.actions,
        vec![
            Action::Skip {
                clip_id: "a".to_string()
            },
            Action::Skip {
                clip_id: "b".to_string()
            },
        ],
    );
}

#[test]
fn rename_onto_an_occupied_path_is_suppressed_on_a_narrowed_listing() {
    // The exact res-1 condition: a partial (not fully enumerated) listing, where
    // the deletion gate is disarmed. The clobber guard is independent of the
    // gate, so the rename is still refused.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("a.mp3", AudioFormat::Mp3, "m", "art"));
    manifest.insert("b", entry("b.mp3", AudioFormat::Mp3, "m", "art"));

    let d = desired("a", "b.mp3", AudioFormat::Mp3, "m", "art");
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_narrowed());

    assert_eq!(plan.renames(), 0);
    assert!(
        plan.actions
            .iter()
            .any(|a| matches!(a, Action::Skip { clip_id } if clip_id == "a")),
        "a's rename onto b's path is downgraded to a Skip",
    );
}

#[test]
fn swapping_renames_are_both_suppressed() {
    // Two clips exchanging names: each targets the other's occupied path. A
    // serial replacing rename would collapse both files into one; both must be
    // suppressed, leaving the files intact (safe-stable, not convergent).
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("song1.mp3", AudioFormat::Mp3, "m", "art"));
    manifest.insert("b", entry("song2.mp3", AudioFormat::Mp3, "m", "art"));

    let da = desired("a", "song2.mp3", AudioFormat::Mp3, "m", "art");
    let db = desired("b", "song1.mp3", AudioFormat::Mp3, "m", "art");
    let local = [
        ("a".to_string(), present(100)),
        ("b".to_string(), present(100)),
    ]
    .into_iter()
    .collect();
    let plan = reconcile(&manifest, &[da, db], &local, &mirror_ok());

    assert_eq!(plan.renames(), 0, "a true swap stays safely unmoved");
    assert_eq!(
        plan.actions,
        vec![
            Action::Skip {
                clip_id: "a".to_string()
            },
            Action::Skip {
                clip_id: "b".to_string()
            },
        ],
    );
}

#[test]
fn a_legitimate_retitle_rename_is_not_suppressed() {
    // The backstop must not break an ordinary rename to a free path: clip `a`
    // retitled from old.mp3 to new.mp3, which no other clip holds.
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("old.mp3", AudioFormat::Mp3, "m", "art"));

    let d = desired("a", "new.mp3", AudioFormat::Mp3, "m", "art");
    let plan = reconcile(&manifest, &[d], &local_present("a"), &mirror_ok());

    assert_eq!(
        plan.actions,
        vec![Action::Rename {
            from: "old.mp3".to_string(),
            to: "new.mp3".to_string(),
        }],
    );
}

#[test]
fn a_rename_chain_converges_over_two_runs() {
    // a wants b's path while b vacates to a free third path. Run one moves b and
    // safely defers a; run two, with b already moved, completes a. This proves
    // the conservative skip converges for a chain (only a true cycle stalls).
    let mut manifest = Manifest::new();
    manifest.insert("a", entry("p1.mp3", AudioFormat::Mp3, "m", "art"));
    manifest.insert("b", entry("p2.mp3", AudioFormat::Mp3, "m", "art"));
    let local = [
        ("a".to_string(), present(100)),
        ("b".to_string(), present(100)),
    ]
    .into_iter()
    .collect::<HashMap<_, _>>();

    let da = desired("a", "p2.mp3", AudioFormat::Mp3, "m", "art");
    let db = desired("b", "p3.mp3", AudioFormat::Mp3, "m", "art");
    let plan = reconcile(&manifest, &[da, db], &local, &mirror_ok());
    assert_eq!(
        plan.actions,
        vec![
            Action::Skip {
                clip_id: "a".to_string()
            },
            Action::Rename {
                from: "p2.mp3".to_string(),
                to: "p3.mp3".to_string(),
            },
        ],
        "run one: b moves to the free path, a is safely deferred",
    );

    // Run two: b has landed at p3, so a's target p2 is now free.
    let mut moved = Manifest::new();
    moved.insert("a", entry("p1.mp3", AudioFormat::Mp3, "m", "art"));
    moved.insert("b", entry("p3.mp3", AudioFormat::Mp3, "m", "art"));
    let da = desired("a", "p2.mp3", AudioFormat::Mp3, "m", "art");
    let db = desired("b", "p3.mp3", AudioFormat::Mp3, "m", "art");
    let plan = reconcile(&moved, &[da, db], &local, &mirror_ok());
    assert!(
        plan.actions.contains(&Action::Rename {
            from: "p1.mp3".to_string(),
            to: "p2.mp3".to_string(),
        }),
        "run two: a completes its rename to the now-free path",
    );
}
