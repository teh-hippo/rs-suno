//! Property test locking the #356 idempotence guarantee at the pure-naming
//! layer: a clip's rendered path is invariant to which other clips share the
//! batch, given a fixed whole-library `colliding_ids` set.
//!
//! The generators are bounded (a tiny id8 space forces collisions, short
//! titles) so cases stay cheap, and failure persistence is disabled so a run
//! never leaves regression files behind.

use super::*;
use proptest::collection::vec;
use proptest::prelude::*;

// A small id8 space (4 groups) forces clips to share the 8-char id prefix, so
// the whole-library pass actually fires. Each clip's full id is unique (the
// enumeration index `k`), so no two clips are the same clip.
fn clips_strategy() -> impl Strategy<Value = Vec<Clip>> {
    vec(0u32..4, 1..8).prop_map(|groups| {
        groups
            .into_iter()
            .enumerate()
            .map(|(k, group)| Clip {
                id: format!("{group:08x}-{k}"),
                title: "Untitled".to_owned(),
                display_name: "alice".to_owned(),
                handle: "alice".to_owned(),
                ..Default::default()
            })
            .collect()
    })
}

// The whole-library collision set over every id, keyed on the first 8 chars —
// the same grouping `LineageStore::colliding_clip_ids` performs, replicated
// here so the naming property is self-contained.
fn colliding_over(clips: &[Clip]) -> BTreeSet<String> {
    let mut by_id8: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for clip in clips {
        let id8 = clip.id.chars().take(8).collect::<String>();
        by_id8.entry(id8).or_default().insert(clip.id.clone());
    }
    by_id8
        .into_values()
        .filter(|ids| ids.len() > 1)
        .flatten()
        .collect()
}

fn render_all(clips: &[Clip], colliding_ids: &BTreeSet<String>) -> Vec<RenderedName> {
    let lineages: Vec<LineageContext> = clips.iter().map(LineageContext::own_root).collect();
    let requests: Vec<NamingRequest> = clips
        .iter()
        .zip(&lineages)
        .map(|(clip, lineage)| NamingRequest { clip, lineage })
        .collect();
    render_clip_names(
        &requests,
        &NamingConfig::default(),
        &BTreeSet::new(),
        colliding_ids,
    )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    // The #356 property: rendering any subset of a fixed library, with the same
    // whole-library `colliding_ids`, yields byte-identical paths to the full
    // render — so a stable batch re-reconciles to zero renames.
    #[test]
    fn render_is_stable_under_subsetting(
        clips in clips_strategy(),
        mask in vec(any::<bool>(), 1..8),
    ) {
        let colliding = colliding_over(&clips);
        let full = render_all(&clips, &colliding);
        let full_paths: BTreeMap<String, PathBuf> = clips
            .iter()
            .zip(&full)
            .map(|(clip, name)| (clip.id.clone(), name.relative_path.clone()))
            .collect();

        let subset: Vec<Clip> = clips
            .iter()
            .enumerate()
            .filter(|(index, _)| mask.get(*index).copied().unwrap_or(true))
            .map(|(_, clip)| clip.clone())
            .collect();
        let subset_render = render_all(&subset, &colliding);

        for (clip, name) in subset.iter().zip(&subset_render) {
            prop_assert_eq!(
                &name.relative_path,
                &full_paths[&clip.id],
                "clip {} path drifted under subsetting",
                clip.id
            );
        }
    }
}
