//! Tests for [`LineageStore::colliding_clip_ids`]: the whole-library set of
//! clip ids sharing an `{id8}` prefix, the file-name counterpart of
//! `colliding_root_titles` that keeps id8 disambiguation batch-independent
//! (#356).

use super::*;

/// An empty resolution, so [`LineageStore::update`] archives every clip as a
/// node without asserting anything about roots.
fn no_resolution() -> Resolution {
    Resolution {
        roots: HashMap::new(),
        gap_filled: Vec::new(),
        bridges: Vec::new(),
    }
}

fn clip_with_id(id: &str) -> Clip {
    Clip {
        id: id.to_owned(),
        title: "Song".to_owned(),
        clip_type: "gen".to_owned(),
        display_name: "alice".to_owned(),
        ..Default::default()
    }
}

fn store_with_ids(ids: &[&str]) -> LineageStore {
    let clips: Vec<Clip> = ids.iter().map(|id| clip_with_id(id)).collect();
    let mut store = LineageStore::new();
    store.update(&clips, &no_resolution(), "now");
    store
}

#[test]
fn colliding_clip_ids_flags_shared_id8_prefixes() {
    // Two clips share the first 8 id chars; a third is unique. Only the two
    // twins are returned, keyed on their full ids.
    let store = store_with_ids(&["abcd1234-aaaa", "abcd1234-bbbb", "zzzz9999-cccc"]);
    let colliding = store.colliding_clip_ids();

    assert!(colliding.contains("abcd1234-aaaa"));
    assert!(colliding.contains("abcd1234-bbbb"));
    assert!(!colliding.contains("zzzz9999-cccc"));
    assert_eq!(colliding.len(), 2);
}

#[test]
fn colliding_clip_ids_remembers_trashed_and_purged_twins() {
    // The stability contract: a twin still pins the suffix even when it is
    // trashed or absent from a later run. `nodes` is monotonic, so once two
    // ids share an id8 the pair is flagged forever, independent of the batch.
    let mut trashed = clip_with_id("abcd1234-gone");
    trashed.is_trashed = true;
    let live = clip_with_id("abcd1234-live");
    let mut store = LineageStore::new();
    store.update(&[live.clone(), trashed], &no_resolution(), "now");

    let colliding = store.colliding_clip_ids();
    assert!(colliding.contains("abcd1234-live"));
    assert!(
        colliding.contains("abcd1234-gone"),
        "a trashed twin must still pin the suffix"
    );

    // A later run that no longer lists the twin (Suno purged it) leaves the
    // node in place, so the kept clip is still flagged.
    store.update(&[live], &no_resolution(), "later");
    let after = store.colliding_clip_ids();
    assert!(after.contains("abcd1234-live"));
    assert!(
        after.contains("abcd1234-gone"),
        "a purged twin remembered by the store must still pin the suffix"
    );
}

#[test]
fn colliding_clip_ids_empty_for_unique_library() {
    // No two clips share an id8, so the set is empty and nothing is suffixed.
    let store = store_with_ids(&["aaaa1111-x", "bbbb2222-y", "cccc3333-z"]);
    assert!(store.colliding_clip_ids().is_empty());
}

#[test]
fn colliding_clip_ids_empty_for_empty_store() {
    // A fresh library has no nodes; the query must not panic and returns empty.
    let store = LineageStore::new();
    assert!(store.colliding_clip_ids().is_empty());
}

#[test]
fn colliding_clip_ids_uses_first_eight_chars() {
    // Ids differing only past char 8 collide; ids differing within the first 8
    // do not. This locks the `take(8)` key against `truncate_chars` in naming.
    let store = store_with_ids(&[
        "aaaaaaaa-one",
        "aaaaaaaa-two", // shares the 8-char prefix -> collides
        "bbbbbbbc-1",
        "bbbbbbbd-1", // differs at the 8th char -> distinct id8, no collision
    ]);
    let colliding = store.colliding_clip_ids();

    assert!(colliding.contains("aaaaaaaa-one"));
    assert!(colliding.contains("aaaaaaaa-two"));
    assert!(!colliding.contains("bbbbbbbc-1"));
    assert!(!colliding.contains("bbbbbbbd-1"));
    assert_eq!(colliding.len(), 2);
}

#[test]
fn colliding_clip_ids_key_case_matches_rendered_id8() {
    // Defensive (current UUIDs are lowercase): two ids equal on the first 8
    // chars up to ASCII case share a group key, so both are flagged. The
    // companion render confirms the path `{id8}`, ASCII-folded, is the same
    // prefix the store keyed on, so the group key can never diverge from the
    // rendered id8 and miss a real collision.
    let store = store_with_ids(&["ABCD1234-x", "abcd1234-y"]);
    let colliding = store.colliding_clip_ids();
    assert!(colliding.contains("ABCD1234-x"));
    assert!(colliding.contains("abcd1234-y"));
    assert_eq!(colliding.len(), 2);

    let upper = clip_with_id("ABCD1234-x");
    let lineage = LineageContext::own_root(&upper);
    let rendered = crate::naming::render_clip_name(
        crate::naming::NamingRequest {
            clip: &upper,
            lineage: &lineage,
        },
        &crate::naming::NamingConfig::default(),
    );
    assert!(
        rendered
            .base_name
            .to_ascii_lowercase()
            .contains("[abcd1234]"),
        "rendered id8 {:?} folds to the store key `abcd1234`",
        rendered.base_name
    );
}
