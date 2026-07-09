//! Async lineage-root resolution: walk a whole library back to each clip's
//! root ancestor, gap-filling ancestors that are missing from the caller's
//! listing over the network.
//!
//! This is the IO surface lifted out of [`crate::lineage`], which stays pure.
//! A clip's parent edge is classified there; here those pure classifiers
//! ([`immediate_parent`], [`attribution_edges`]) are threaded up each parent
//! chain to a root ([`resolve_roots`]), reaching the network only through the
//! [`Http`] port (via [`SunoClient`]) to fill ancestors absent from the
//! caller's listing. The dependency is one-way: `roots` depends on `lineage`,
//! never the reverse.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::client::SunoClient;
use crate::clock::Clock;
use crate::error::Result;
use crate::http::Http;
use crate::lineage::{Resolution, ResolveStatus, RootInfo, attribution_edges, immediate_parent};
use crate::model::Clip;

/// Tunables bounding how hard [`resolve_roots`] works per call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveOpts {
    /// Maximum number of missing ancestor ids to fetch from the network. This
    /// is the only budget: in-index walking is unbounded (a cycle guard, not a
    /// hop cap, guarantees termination), so a deep chain resolves in full.
    pub max_gap_fills: u32,
    /// Maximum concurrent by-id clip fetches during one gap-fill batch.
    pub concurrency: u32,
}

impl Default for ResolveOpts {
    fn default() -> Self {
        Self {
            max_gap_fills: 200,
            concurrency: 4,
        }
    }
}

/// Resolve the root ancestor of every clip in `clips`.
///
/// Walks each clip up its [`immediate_parent`] chain to a root. Chains that
/// stay within `clips` resolve with no network access. When a parent is absent
/// from the index it is gap-filled: missing ids are fetched in a batch through
/// [`SunoClient::get_clips_by_ids`], and any id that cannot be retrieved that
/// way falls back to [`SunoClient::get_clip_parent`], which yields one ancestor
/// hop to keep walking (never assumed to be the absolute root).
///
/// Gap-filled clips (which may be trashed) are held in an index that is kept
/// structurally separate from the caller's `clips`; they exist only to resolve
/// ancestry and must never be treated as download candidates by later phases.
///
/// Bounded by [`ResolveOpts`]: at most `max_gap_fills` ancestor ids are fetched
/// (exhaustion yields [`ResolveStatus::External`] at the last reachable
/// ancestor). In-index chains are not hop-capped: a chain that stays within the
/// index (or persisted archive) is walked in full to its true parentless root,
/// however deep, terminating via a `visited` cycle guard. A cycle yields
/// [`ResolveStatus::Cycle`] rooted at the cycle's canonical (lexicographically
/// smallest) member, so cyclic data resolves order-independently. The returned
/// [`Resolution`] has a root entry for every input clip, plus the gap-filled
/// ancestor clips it fetched.
pub async fn resolve_roots(
    clips: &[Clip],
    archived_parents: &HashMap<String, String>,
    client: &SunoClient<impl Clock>,
    http: &impl Http,
    opts: ResolveOpts,
) -> Result<Resolution> {
    let mut resolver = Resolver::new(clips, opts, archived_parents);
    resolver.run(client, http).await?;
    Ok(resolver.into_resolution(clips))
}

/// The result of walking one chain as far as the current index allows.
enum Walk {
    /// The start clip's root is now recorded in the memo.
    Resolved,
    /// The walk stalled needing this ancestor id gap-filled.
    Blocked(String),
}

/// Working state for one [`resolve_roots`] call.
///
/// `index` holds the input clips plus any gap-filled ancestors so the walk can
/// read their pointers; `gap_filled` records which ids were fetched here so
/// later phases can tell ancestors apart from download candidates. `bridges`
/// maps a missing id to the known parent that the parent endpoint returned in
/// its place, and `external` records ids the API reported as parentless roots.
struct Resolver<'a> {
    index: HashMap<String, Cow<'a, Clip>>,
    /// Persisted `child_id -> parent_id` links from the durable store's primary
    /// edges. Consulted before any network gap-fill so a walk can hop through an
    /// ancestor whose clip is absent (e.g. an intermediate remix, or one Suno
    /// has since purged) using data captured on an earlier run.
    archived_parents: &'a HashMap<String, String>,
    gap_filled: HashSet<String>,
    bridges: HashMap<String, String>,
    external: HashSet<String>,
    /// Clip-root ids already attempted as a gap-fill seed, so a root that the
    /// batch never returns is tried once and then left alone (never re-seeded,
    /// never bridged, never external).
    seeded: HashSet<String>,
    memo: HashMap<String, RootInfo>,
    targets: Vec<String>,
    budget: u32,
    concurrency: u32,
}

impl<'a> Resolver<'a> {
    fn new(
        clips: &'a [Clip],
        opts: ResolveOpts,
        archived_parents: &'a HashMap<String, String>,
    ) -> Self {
        let index = clips
            .iter()
            .map(|clip| (clip.id.clone(), Cow::Borrowed(clip)))
            .collect();
        let targets = clips.iter().map(|clip| clip.id.clone()).collect();
        Self {
            index,
            archived_parents,
            gap_filled: HashSet::new(),
            bridges: HashMap::new(),
            external: HashSet::new(),
            seeded: HashSet::new(),
            memo: HashMap::new(),
            targets,
            budget: opts.max_gap_fills,
            concurrency: opts.concurrency,
        }
    }

    /// Resolve every target, gap-filling missing ancestors until the whole set
    /// is settled or the budget runs out.
    async fn run(&mut self, client: &SunoClient<impl Clock>, http: &impl Http) -> Result<()> {
        let targets = self.targets.clone();
        loop {
            let mut frontier: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            let mut blocked: Vec<(String, String)> = Vec::new();

            for target in &targets {
                if self.memo.contains_key(target) {
                    continue;
                }
                if let Walk::Blocked(missing) = self.walk(target) {
                    if seen.insert(missing.clone()) {
                        frontier.push(missing.clone());
                    }
                    blocked.push((target.clone(), missing));
                }
            }

            if blocked.is_empty() {
                break;
            }
            if self.budget == 0 || !self.gap_fill(client, http, &frontier).await? {
                self.finalise_external(&blocked);
                break;
            }
        }
        Ok(())
    }

    /// Walk `start` up its parent chain within the current index, memoising the
    /// root for every node reached. Returns [`Walk::Blocked`] with the first
    /// ancestor id that is missing and needs gap-filling.
    fn walk(&mut self, start: &str) -> Walk {
        if self.memo.contains_key(start) {
            return Walk::Resolved;
        }
        let mut chain: Vec<String> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut current = start.to_string();

        loop {
            if let Some(info) = self.memo.get(&current).cloned() {
                self.assign(&chain, &info);
                return Walk::Resolved;
            }
            if visited.contains(&current) {
                // A cycle. Root it at its canonical (lexicographically smallest)
                // member so the same cyclic data resolves the same root whatever
                // order its clips were listed in. The members are the nodes from
                // `current`'s first occurrence in the chain onward; any non-cycle
                // lead-in walked before that point is excluded.
                let cycle_start = chain.iter().position(|id| *id == current).unwrap_or(0);
                let root = chain[cycle_start..]
                    .iter()
                    .min()
                    .cloned()
                    .unwrap_or_else(|| current.clone());
                let info = self.terminal(&root, ResolveStatus::Cycle);
                self.assign(&chain, &info);
                self.memo.insert(current, info);
                return Walk::Resolved;
            }

            // The parent of `current` comes from its live/fetched clip, or from
            // a persisted archived edge when the clip itself is not in hand. An
            // id known through neither is unknown locally and must be gap-filled
            // (this is the guard: an edgeless archived node is fetched, never
            // assumed a root, so a not-yet-persisted remix still gets its real
            // parent).
            let parent_id = if let Some(clip) = self.index.get(&current) {
                immediate_parent(clip).map(|(id, _edge)| id)
            } else if let Some(parent) = self.archived_parents.get(&current) {
                Some(parent.clone())
            } else {
                return Walk::Blocked(current);
            };

            let Some(parent_id) = parent_id else {
                let info = RootInfo {
                    root_id: current.clone(),
                    root_title: self.title_of(&current),
                    status: ResolveStatus::Resolved,
                };
                self.assign(&chain, &info);
                self.memo.insert(current, info);
                return Walk::Resolved;
            };

            visited.insert(current.clone());
            chain.push(current);

            if self.index.contains_key(&parent_id) || self.archived_parents.contains_key(&parent_id)
            {
                current = parent_id;
            } else if let Some(bridged) = self.bridges.get(&parent_id).cloned() {
                visited.insert(parent_id);
                current = bridged;
            } else if self.external.contains(&parent_id) {
                let info = self.terminal(&parent_id, ResolveStatus::External);
                self.assign(&chain, &info);
                self.memo.insert(parent_id, info);
                return Walk::Resolved;
            } else {
                return Walk::Blocked(parent_id);
            }
        }
    }

    /// Fetch missing `frontier` ancestors, batching by id and falling back to
    /// the parent endpoint. Same-owner `clip_roots` are additionally seeded as
    /// best-effort root candidates. Returns whether the index (or
    /// bridges/externals) grew, so the caller can detect a stalled resolution.
    async fn gap_fill(
        &mut self,
        client: &SunoClient<impl Clock>,
        http: &impl Http,
        frontier: &[String],
    ) -> Result<bool> {
        // Structural frontier: ancestors a walk is blocked on. They get the full
        // treatment (batch fetch, then a parent-endpoint fallback that may bridge
        // one hop or mark the id external).
        let mut want: Vec<String> = frontier
            .iter()
            .filter(|id| !self.known(id))
            .cloned()
            .collect();
        want.sort();
        want.dedup();

        // Same-owner clip_root seeds: an OPTIONAL extra root candidate. They ride
        // the batch and its per-id fallback, but never the parent-endpoint path
        // below, so a seed the fetch omits is simply dropped, never bridged,
        // externalised, or forced to a root: clip_roots can neither fabricate a
        // parent link nor arm a delete. Foreign-owned roots are excluded
        // (fail-closed by handle), and each seed is attempted at most once.
        let mut seeds: Vec<String> = self
            .clip_root_seeds()
            .into_iter()
            .filter(|id| !self.known(id) && !self.seeded.contains(id) && !want.contains(id))
            .collect();
        seeds.sort();
        seeds.dedup();

        if want.is_empty() && seeds.is_empty() {
            return Ok(false);
        }

        // Frontier ids take budget priority so a blocked walk is never starved
        // by a best-effort seed.
        let frontier_take = (self.budget as usize).min(want.len());
        let frontier_batch: Vec<String> = want.into_iter().take(frontier_take).collect();
        self.budget -= frontier_batch.len() as u32;

        let seed_take = (self.budget as usize).min(seeds.len());
        let seed_batch: Vec<String> = seeds.into_iter().take(seed_take).collect();
        self.budget -= seed_batch.len() as u32;
        for id in &seed_batch {
            self.seeded.insert(id.clone());
        }

        // One batch call covers frontier + seeds; the parent-endpoint fallback
        // below is confined to the structural frontier.
        let all: Vec<&str> = frontier_batch
            .iter()
            .chain(seed_batch.iter())
            .map(String::as_str)
            .collect();
        let fetched = client
            .get_clips_by_ids(http, &all, self.concurrency as usize)
            .await?;

        let mut returned: HashSet<String> = HashSet::new();
        let mut progressed = false;
        for clip in fetched {
            returned.insert(clip.id.clone());
            if self.insert_ancestor(clip) {
                progressed = true;
            }
        }

        for id in &frontier_batch {
            if returned.contains(id) {
                continue;
            }
            match client.get_clip_parent(http, id).await? {
                Some(parent) => {
                    let parent_id = parent.id.clone();
                    self.insert_ancestor(parent);
                    self.bridges.insert(id.clone(), parent_id);
                    progressed = true;
                }
                None => {
                    self.external.insert(id.clone());
                    progressed = true;
                }
            }
        }

        Ok(progressed)
    }

    /// Same-owner `clip_root` ids across the current index, as extra root
    /// candidates for gap-fill. Foreign-owned roots are excluded (fail-closed by
    /// handle) so a foreign remix source is never folded into the owner's album.
    fn clip_root_seeds(&self) -> Vec<String> {
        let mut seeds = Vec::new();
        for clip in self.index.values() {
            for edge in attribution_edges(clip) {
                if edge.same_owner {
                    seeds.push(edge.parent_id);
                }
            }
        }
        seeds
    }

    /// Add a gap-filled ancestor to the index, tracking it as an ancestor-only
    /// clip. Returns whether it was newly added.
    fn insert_ancestor(&mut self, clip: Clip) -> bool {
        if clip.id.is_empty() || self.index.contains_key(&clip.id) {
            return false;
        }
        self.gap_filled.insert(clip.id.clone());
        self.index.insert(clip.id.clone(), Cow::Owned(clip));
        true
    }

    /// Whether an id is already resolvable without another fetch.
    fn known(&self, id: &str) -> bool {
        self.index.contains_key(id)
            || self.archived_parents.contains_key(id)
            || self.bridges.contains_key(id)
            || self.external.contains(id)
    }

    /// Mark every still-unresolved blocked target as external at the ancestor it
    /// stalled on.
    fn finalise_external(&mut self, blocked: &[(String, String)]) {
        for (target, missing) in blocked {
            if self.memo.contains_key(target) {
                continue;
            }
            let info = self.terminal(missing, ResolveStatus::External);
            self.memo.insert(target.clone(), info);
        }
    }

    /// Build a [`RootInfo`] rooted at `id`, titled from the index when present.
    fn terminal(&self, id: &str, status: ResolveStatus) -> RootInfo {
        RootInfo {
            root_id: id.to_string(),
            root_title: self.title_of(id),
            status,
        }
    }

    /// The title of an indexed clip, or empty when it is not in the index.
    fn title_of(&self, id: &str) -> String {
        self.index
            .get(id)
            .map_or_else(String::new, |clip| clip.title.clone())
    }

    /// Record `info` as the root for every node on `chain`.
    fn assign(&mut self, chain: &[String], info: &RootInfo) {
        for id in chain {
            self.memo.insert(id.clone(), info.clone());
        }
    }

    /// Project the memo onto the input clips (so every one has a root entry) and
    /// collect the gap-filled ancestors, sorted by id for a deterministic order.
    fn into_resolution(mut self, clips: &[Clip]) -> Resolution {
        let mut roots = HashMap::with_capacity(clips.len());
        for clip in clips {
            let info = self
                .memo
                .get(&clip.id)
                .cloned()
                .unwrap_or_else(|| RootInfo {
                    root_id: clip.id.clone(),
                    root_title: clip.title.clone(),
                    status: ResolveStatus::Unresolved,
                });
            roots.insert(clip.id.clone(), info);
        }

        // Gap-filled ancestors are held as `Cow::Owned`, so move them out of the
        // index rather than cloning; the input clips are borrowed and never
        // collected here.
        let gap_filled_ids = std::mem::take(&mut self.gap_filled);
        let mut gap_filled: Vec<Clip> = gap_filled_ids
            .iter()
            .filter_map(|id| self.index.remove(id))
            .map(Cow::into_owned)
            .collect();
        gap_filled.sort_by(|a, b| a.id.cmp(&b.id));

        let mut bridges: Vec<(String, String)> = self
            .bridges
            .iter()
            .map(|(child, parent)| (child.clone(), parent.clone()))
            .collect();
        bridges.sort();

        Resolution {
            roots,
            gap_filled,
            bridges,
        }
    }
}

#[cfg(test)]
mod tests;
