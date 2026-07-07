use super::*;

fn clip(id: &str, created_at: &str) -> Clip {
    Clip {
        id: id.to_owned(),
        created_at: created_at.to_owned(),
        ..Default::default()
    }
}

/// A context that folders `clip` under `root_id`; other fields are irrelevant to
/// track assignment.
fn ctx(root_id: &str) -> LineageContext {
    LineageContext {
        root_id: root_id.to_owned(),
        ..LineageContext::own_root(&Clip::default())
    }
}

fn contexts_all<'a>(
    clips: impl IntoIterator<Item = &'a Clip>,
    root: &str,
) -> HashMap<String, LineageContext> {
    clips
        .into_iter()
        .map(|c| (c.id.clone(), ctx(root)))
        .collect()
}

fn no_leads() -> BTreeSet<String> {
    BTreeSet::new()
}

fn leads(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn numbers_by_created_at_ascending() {
    let a = clip("a", "2026-01-03T00:00:00Z");
    let b = clip("b", "2026-01-01T00:00:00Z");
    let c = clip("c", "2026-01-02T00:00:00Z");
    let clips = [&a, &b, &c];
    let contexts = contexts_all(clips, "root");

    let out = assign_track_numbers(&clips, &contexts, &no_leads(), true);

    assert_eq!(out["b"], TrackAssignment { track: 1, total: 3 });
    assert_eq!(out["c"], TrackAssignment { track: 2, total: 3 });
    assert_eq!(out["a"], TrackAssignment { track: 3, total: 3 });
}

#[test]
fn ties_on_created_at_break_by_id() {
    let a = clip("bbb", "2026-01-01T00:00:00Z");
    let b = clip("aaa", "2026-01-01T00:00:00Z");
    let clips = [&a, &b];
    let contexts = contexts_all(clips, "root");

    let out = assign_track_numbers(&clips, &contexts, &no_leads(), true);

    assert_eq!(out["aaa"].track, 1);
    assert_eq!(out["bbb"].track, 2);
}

#[test]
fn lead_is_promoted_to_track_one_and_shifts_the_rest() {
    // The main version was made last (track 7 chronologically); flagging it as
    // lead pulls it to 1 while the others keep their relative order.
    let mut clips_owned = Vec::new();
    for (i, day) in (1..=7).enumerate() {
        clips_owned.push(clip(&format!("v{i}"), &format!("2026-01-0{day}T00:00:00Z")));
    }
    let clips: Vec<&Clip> = clips_owned.iter().collect();
    let contexts = contexts_all(clips.iter().copied(), "root");

    let out = assign_track_numbers(&clips, &contexts, &leads(&["v6"]), true);

    assert_eq!(out["v6"], TrackAssignment { track: 1, total: 7 });
    assert_eq!(out["v0"].track, 2, "the previous first shifts to 2");
    assert_eq!(out["v5"].track, 7, "the last non-lead stays last");
}

#[test]
fn lead_already_earliest_leaves_order_unchanged() {
    let a = clip("a", "2026-01-01T00:00:00Z");
    let b = clip("b", "2026-01-02T00:00:00Z");
    let clips = [&a, &b];
    let contexts = contexts_all(clips, "root");

    let out = assign_track_numbers(&clips, &contexts, &leads(&["a"]), true);

    assert_eq!(out["a"].track, 1);
    assert_eq!(out["b"].track, 2);
}

#[test]
fn albums_are_grouped_by_root_and_numbered_independently() {
    let a1 = clip("a1", "2026-01-02T00:00:00Z");
    let a2 = clip("a2", "2026-01-01T00:00:00Z");
    let b1 = clip("b1", "2026-01-05T00:00:00Z");
    let clips = [&a1, &a2, &b1];
    let mut contexts = HashMap::new();
    contexts.insert("a1".to_owned(), ctx("rootA"));
    contexts.insert("a2".to_owned(), ctx("rootA"));
    contexts.insert("b1".to_owned(), ctx("rootB"));

    let out = assign_track_numbers(&clips, &contexts, &no_leads(), true);

    assert_eq!(out["a2"], TrackAssignment { track: 1, total: 2 });
    assert_eq!(out["a1"], TrackAssignment { track: 2, total: 2 });
    assert_eq!(out["b1"], TrackAssignment { track: 1, total: 1 });
}

#[test]
fn singleton_numbered_when_enabled() {
    let a = clip("a", "2026-01-01T00:00:00Z");
    let clips = [&a];
    let contexts = contexts_all(clips, "root");

    let out = assign_track_numbers(&clips, &contexts, &no_leads(), true);

    assert_eq!(out["a"], TrackAssignment { track: 1, total: 1 });
}

#[test]
fn singleton_unnumbered_when_disabled() {
    let a = clip("a", "2026-01-01T00:00:00Z");
    let clips = [&a];
    let contexts = contexts_all(clips, "root");

    let out = assign_track_numbers(&clips, &contexts, &no_leads(), false);

    assert!(out.is_empty(), "a lone track is left unnumbered");
}

#[test]
fn multi_track_album_still_numbered_when_singletons_disabled() {
    let a = clip("a", "2026-01-01T00:00:00Z");
    let b = clip("b", "2026-01-02T00:00:00Z");
    let clips = [&a, &b];
    let contexts = contexts_all(clips, "root");

    let out = assign_track_numbers(&clips, &contexts, &no_leads(), false);

    assert_eq!(out["a"].track, 1);
    assert_eq!(out["b"].track, 2);
}

#[test]
fn duplicate_leads_in_one_album_take_the_earliest() {
    let a = clip("a", "2026-01-01T00:00:00Z");
    let b = clip("b", "2026-01-02T00:00:00Z");
    let c = clip("c", "2026-01-03T00:00:00Z");
    let clips = [&a, &b, &c];
    let contexts = contexts_all(clips, "root");

    // Both b and c flagged; the earliest of the two (b) wins track 1.
    let out = assign_track_numbers(&clips, &contexts, &leads(&["b", "c"]), true);

    assert_eq!(out["b"].track, 1);
    assert_eq!(out["a"].track, 2);
    assert_eq!(out["c"].track, 3);
}

#[test]
fn clip_absent_from_contexts_is_its_own_album() {
    let a = clip("a", "2026-01-01T00:00:00Z");
    let clips = [&a];
    let contexts = HashMap::new();

    let out = assign_track_numbers(&clips, &contexts, &no_leads(), true);

    assert_eq!(out["a"], TrackAssignment { track: 1, total: 1 });
}

#[test]
fn empty_input_yields_empty_map() {
    let out = assign_track_numbers(&[], &HashMap::new(), &no_leads(), true);
    assert!(out.is_empty());
}

#[test]
fn resolve_matches_exact_prefix_and_reports_the_rest() {
    let a = clip("b320f4cf-26ef-4e6a-8d7b-aa4a7096952e", "");
    let b = clip("c6f6a1a5-7c6a-4424-9249-3fa847dc0a3a", "");
    let clips = [&a, &b];

    let out = resolve_lead_ids(
        &clips,
        &[
            "b320f4cf".to_owned(),                             // id8 prefix
            "c6f6a1a5-7c6a-4424-9249-3fa847dc0a3a".to_owned(), // full id
            "deadbeef".to_owned(),                             // no match
            "   ".to_owned(),                                  // ignored
        ],
    );

    assert!(
        out.resolved
            .contains("b320f4cf-26ef-4e6a-8d7b-aa4a7096952e")
    );
    assert!(
        out.resolved
            .contains("c6f6a1a5-7c6a-4424-9249-3fa847dc0a3a")
    );
    assert_eq!(out.unmatched, vec!["deadbeef".to_owned()]);
    assert!(out.ambiguous.is_empty());
}

#[test]
fn resolve_flags_ambiguous_prefixes() {
    let a = clip("ab111111-0000-0000-0000-000000000000", "");
    let b = clip("ab222222-0000-0000-0000-000000000000", "");
    let clips = [&a, &b];

    let out = resolve_lead_ids(&clips, &["ab".to_owned()]);

    assert!(
        out.resolved.is_empty(),
        "an ambiguous prefix resolves to nothing"
    );
    assert_eq!(out.ambiguous, vec!["ab".to_owned()]);
}

#[test]
fn resolve_is_case_insensitive() {
    let a = clip("b320f4cf-26ef-4e6a-8d7b-aa4a7096952e", "");
    let clips = [&a];

    let out = resolve_lead_ids(&clips, &["B320F4CF".to_owned()]);

    assert!(
        out.resolved
            .contains("b320f4cf-26ef-4e6a-8d7b-aa4a7096952e")
    );
}
