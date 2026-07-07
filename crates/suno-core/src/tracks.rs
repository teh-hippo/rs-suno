//! Album track numbering: order a lineage album's downloaded clips and assign
//! 1-based track numbers, with an optional per-album lead override.
//!
//! An album is the set of selected clips sharing a resolved lineage root (the
//! same grouping [`album_desired`](crate::album_desired) uses for folder art).
//! Within a group, tracks are ordered by `created_at` ascending (tie-break by
//! id), so a track's number reflects when that version was made. A configured
//! *lead* clip is promoted to track 1, shifting the rest down while keeping
//! their relative order, so a later-made "main" version can still present as
//! song 1. Pure and IO-free; the CLI resolves the lead ids and folds the result
//! into each clip's [`LineageContext`].

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::lineage::LineageContext;
use crate::model::Clip;

/// A clip's assigned position within its album: 1-based `track` of `total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackAssignment {
    pub track: u32,
    pub total: u32,
}

/// The outcome of matching configured lead entries against the selected clips.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeadResolution {
    /// Full clip ids to treat as their album's lead (track 1). Deduplicated.
    pub resolved: BTreeSet<String>,
    /// Configured entries that matched no selected clip.
    pub unmatched: Vec<String>,
    /// Configured entries that matched more than one selected clip (too short a
    /// prefix); left unresolved so an ambiguous flag never silently mis-numbers.
    pub ambiguous: Vec<String>,
}

/// Resolve configured lead entries to concrete clip ids by unique id-prefix.
///
/// Each entry matches a clip when the clip id equals it or starts with it
/// (case-insensitive), so the 8-char code from a filename (`[b320f4cf]`) or a
/// full UUID both work. An entry matching exactly one clip resolves to that
/// clip's id; zero or many matches are reported for the caller to warn on rather
/// than guessing. Empty entries are ignored.
pub fn resolve_lead_ids(clips: &[&Clip], configured: &[String]) -> LeadResolution {
    let mut out = LeadResolution::default();
    for entry in configured {
        let needle = entry.trim();
        if needle.is_empty() {
            continue;
        }
        let mut matches = clips.iter().filter(|clip| id_matches(&clip.id, needle));
        match (matches.next(), matches.next()) {
            (Some(clip), None) => {
                out.resolved.insert(clip.id.clone());
            }
            (Some(_), Some(_)) => out.ambiguous.push(needle.to_owned()),
            (None, _) => out.unmatched.push(needle.to_owned()),
        }
    }
    out
}

/// Whether `id` equals or is prefixed by `needle`, ASCII-case-insensitively.
fn id_matches(id: &str, needle: &str) -> bool {
    id.get(..needle.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(needle))
}

/// Assign 1-based track numbers within each lineage album.
///
/// Groups `clips` by the resolved root id in `contexts` (a clip absent from the
/// map, or with an empty root, is its own album), orders each group by
/// `(created_at, id)`, promotes the first member found in `leads` to track 1,
/// and returns each clip's [`TrackAssignment`]. When `number_singletons` is
/// false, a lone-track album is left out of the result (unnumbered). Clips not
/// present in the returned map keep whatever number their context already holds.
pub fn assign_track_numbers(
    clips: &[&Clip],
    contexts: &HashMap<String, LineageContext>,
    leads: &BTreeSet<String>,
    number_singletons: bool,
) -> HashMap<String, TrackAssignment> {
    let mut groups: BTreeMap<&str, Vec<&Clip>> = BTreeMap::new();
    for clip in clips {
        let root = contexts
            .get(&clip.id)
            .map(|ctx| ctx.root_id.as_str())
            .filter(|root| !root.is_empty())
            .unwrap_or(clip.id.as_str());
        groups.entry(root).or_default().push(clip);
    }

    let mut out = HashMap::with_capacity(clips.len());
    for (_root, mut members) in groups {
        members.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let total = members.len() as u32;
        if total == 1 && !number_singletons {
            continue;
        }
        if let Some(pos) = members.iter().position(|clip| leads.contains(&clip.id)) {
            let lead = members.remove(pos);
            members.insert(0, lead);
        }
        for (index, clip) in members.iter().enumerate() {
            out.insert(
                clip.id.clone(),
                TrackAssignment {
                    track: index as u32 + 1,
                    total,
                },
            );
        }
    }
    out
}

#[cfg(test)]
mod tests;
