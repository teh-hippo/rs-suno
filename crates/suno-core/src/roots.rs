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
    /// Maximum number of missing ancestor ids to fetch from the network.
    pub max_gap_fills: u32,
    /// Maximum hops to walk up a single chain before giving up.
    pub hop_cap: u32,
    /// Maximum concurrent by-id clip fetches during one gap-fill batch.
    pub concurrency: u32,
}

impl Default for ResolveOpts {
    fn default() -> Self {
        Self {
            max_gap_fills: 200,
            hop_cap: 64,
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
/// ancestor), and each chain walks at most `hop_cap` hops. A cycle yields
/// [`ResolveStatus::Cycle`]. The returned [`Resolution`] has a root entry for
/// every input clip, plus the gap-filled ancestor clips it fetched.
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
    index: HashMap<String, Clip>,
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
    hop_cap: u32,
    concurrency: u32,
}

impl<'a> Resolver<'a> {
    fn new(
        clips: &[Clip],
        opts: ResolveOpts,
        archived_parents: &'a HashMap<String, String>,
    ) -> Self {
        let index = clips
            .iter()
            .map(|clip| (clip.id.clone(), clip.clone()))
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
            hop_cap: opts.hop_cap,
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
        let mut hops = 0u32;

        loop {
            if let Some(info) = self.memo.get(&current).cloned() {
                self.assign(&chain, &info);
                return Walk::Resolved;
            }
            if visited.contains(&current) {
                let info = self.terminal(&current, ResolveStatus::Cycle);
                self.assign(&chain, &info);
                self.memo.insert(current, info);
                return Walk::Resolved;
            }
            if hops >= self.hop_cap {
                let info = self.terminal(&current, ResolveStatus::Unresolved);
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
            hops += 1;
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
        self.index.insert(clip.id.clone(), clip);
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
    fn into_resolution(self, clips: &[Clip]) -> Resolution {
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

        let mut gap_filled: Vec<Clip> = self
            .gap_filled
            .iter()
            .filter_map(|id| self.index.get(id).cloned())
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
mod tests {
    use super::*;
    use crate::auth::ClerkAuth;
    use crate::testutil::{RecordingClock, Reply, ScriptedHttp};

    // A clean six-clip chain modelled on the real `chain1` grounding data:
    // upsample -> cover -> upsample -> cover -> edit -> root. For every hop the
    // op pointer and `edited_clip_id` agree, as they do in the live shape.
    fn chain1_clips() -> Vec<Clip> {
        vec![
            Clip {
                id: "40068b49".into(),
                title: "Zac and the Sea Eagles (Lullaby Version)".into(),
                clip_type: "upsample".into(),
                task: "upsample".into(),
                is_remix: true,
                upsample_clip_id: "52962dae".into(),
                edited_clip_id: "52962dae".into(),
                ..Default::default()
            },
            Clip {
                id: "52962dae".into(),
                title: "Zac and the Sea Eagles (Edit) (Remastered)".into(),
                clip_type: "gen".into(),
                task: "cover".into(),
                is_remix: true,
                cover_clip_id: "536e1b92".into(),
                edited_clip_id: "536e1b92".into(),
                ..Default::default()
            },
            Clip {
                id: "536e1b92".into(),
                title: "Zac and the Sea Eagles (Edit) (Remastered)".into(),
                clip_type: "upsample".into(),
                task: "upsample".into(),
                is_remix: true,
                upsample_clip_id: "b9f27ee1".into(),
                edited_clip_id: "b9f27ee1".into(),
                ..Default::default()
            },
            Clip {
                id: "b9f27ee1".into(),
                title: "Zac and the Sea Eagles (Edit)".into(),
                clip_type: "gen".into(),
                task: "cover".into(),
                is_remix: true,
                cover_clip_id: "c1997d52".into(),
                edited_clip_id: "c1997d52".into(),
                ..Default::default()
            },
            Clip {
                id: "c1997d52".into(),
                title: "Zac and the Sea Eagles (Rework)".into(),
                clip_type: "edit_v3_export".into(),
                edited_clip_id: "dfb59a04".into(),
                ..Default::default()
            },
            Clip {
                id: "dfb59a04".into(),
                title: "Zac and the Sea Eagles".into(),
                clip_type: "gen".into(),
                ..Default::default()
            },
        ]
    }

    fn authed_client(http: &ScriptedHttp) -> SunoClient<RecordingClock> {
        let auth = ClerkAuth::new("eyJtoken");
        pollster::block_on(auth.authenticate(http)).unwrap();
        SunoClient::new(auth, RecordingClock::new())
    }

    fn clip_root(id: &str, handle: &str) -> crate::model::ClipRoot {
        crate::model::ClipRoot {
            id: id.to_owned(),
            handle: handle.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_roots_walks_a_connected_chain_with_no_http() {
        let http = ScriptedHttp::new();
        let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
        let clips = chain1_clips();

        let roots = pollster::block_on(resolve_roots(
            &clips,
            &HashMap::new(),
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap()
        .roots;

        assert!(
            http.calls().is_empty(),
            "a fully-connected chain must never touch the network"
        );
        assert_eq!(roots.len(), clips.len());
        for clip in &clips {
            let info = &roots[&clip.id];
            assert_eq!(info.status, ResolveStatus::Resolved);
            assert_eq!(info.root_id, "dfb59a04");
            assert_eq!(info.root_title, "Zac and the Sea Eagles");
        }
    }

    #[test]
    fn resolve_roots_gap_fills_a_missing_ancestor_by_id() {
        let cover = Clip {
            id: "child".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "root".into(),
            edited_clip_id: "root".into(),
            ..Default::default()
        };
        let root_clip = serde_json::json!({
            "id": "root", "title": "Original", "status": "complete",
            "metadata": {"type": "gen"}
        })
        .to_string();
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/clip/root", Reply::json(&root_clip));
        let client = authed_client(&http);

        let roots = pollster::block_on(resolve_roots(
            &[cover],
            &HashMap::new(),
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap()
        .roots;

        let info = &roots["child"];
        assert_eq!(info.status, ResolveStatus::Resolved);
        assert_eq!(info.root_id, "root");
        assert_eq!(info.root_title, "Original");
        assert_eq!(http.count("/api/clip/root"), 1);
        assert_eq!(
            http.count("/api/clips/parent"),
            0,
            "the parent endpoint must not be used when the per-id fetch succeeds"
        );
    }

    #[test]
    fn resolve_roots_hops_through_a_purged_ancestor_via_the_archive() {
        // A cover whose parent (an intermediate remix) is absent from this run's
        // clips AND unfetchable from the network (Suno purged it), but whose
        // parent link was persisted on an earlier run. The archived edge lets
        // the walk hop through the purged intermediate to the true root, with no
        // network call, instead of self-rooting into a duplicate album.
        let child = Clip {
            id: "child".into(),
            title: "Neue Deutsche Harte".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "mid".into(),
            edited_clip_id: "mid".into(),
            ..Default::default()
        };
        let root = Clip {
            id: "root".into(),
            title: "Original".into(),
            clip_type: "gen".into(),
            ..Default::default()
        };
        // "mid" is neither a live clip nor routed on the network double.
        let archived: HashMap<String, String> = [("mid".to_owned(), "root".to_owned())]
            .into_iter()
            .collect();
        let http = ScriptedHttp::new().with_auth();
        let client = authed_client(&http);

        let resolution = pollster::block_on(resolve_roots(
            &[child, root],
            &archived,
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap();

        let info = &resolution.roots["child"];
        assert_eq!(info.status, ResolveStatus::Resolved);
        assert_eq!(
            info.root_id, "root",
            "hopped through the purged intermediate"
        );
        assert_eq!(info.root_title, "Original");
        assert_eq!(
            http.count("/api/clip/mid"),
            0,
            "the purged intermediate is never fetched: the archived edge bridges it"
        );
        assert!(
            resolution.gap_filled.is_empty(),
            "an archived hop must not add a download candidate"
        );
    }

    #[test]
    fn resolve_roots_prefers_a_live_pointer_over_a_stale_archived_edge() {
        // When a clip is present live, its own (fresh) pointer wins; a stale
        // archived edge for that same clip is ignored (index before archive).
        let child = Clip {
            id: "child".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "live_root".into(),
            edited_clip_id: "live_root".into(),
            ..Default::default()
        };
        let live_root = Clip {
            id: "live_root".into(),
            title: "Live Root".into(),
            clip_type: "gen".into(),
            ..Default::default()
        };
        let archived: HashMap<String, String> = [("child".to_owned(), "stale_root".to_owned())]
            .into_iter()
            .collect();
        let http = ScriptedHttp::new().with_auth();
        let client = authed_client(&http);

        let info = pollster::block_on(resolve_roots(
            &[child, live_root],
            &archived,
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap()
        .roots["child"]
            .clone();

        assert_eq!(
            info.root_id, "live_root",
            "the live pointer wins over a stale archived edge"
        );
        assert_eq!(info.status, ResolveStatus::Resolved);
    }

    #[test]
    fn resolve_roots_terminates_on_a_cycle_through_archived_edges() {
        // Archived edges form a cycle a -> b -> a; the walk must terminate via
        // the visited guard, never loop.
        let child = Clip {
            id: "child".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "a".into(),
            edited_clip_id: "a".into(),
            ..Default::default()
        };
        let archived: HashMap<String, String> = [
            ("a".to_owned(), "b".to_owned()),
            ("b".to_owned(), "a".to_owned()),
        ]
        .into_iter()
        .collect();
        let http = ScriptedHttp::new().with_auth();
        let client = authed_client(&http);

        let info = pollster::block_on(resolve_roots(
            &[child],
            &archived,
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap()
        .roots["child"]
            .clone();

        assert_eq!(
            info.status,
            ResolveStatus::Cycle,
            "an archived cycle terminates as a cycle, not an infinite loop"
        );
    }

    #[test]
    fn resolve_roots_respects_the_hop_cap_through_archived_edges() {
        // A long archived chain past the hop cap terminates as unresolved,
        // without any network fetch.
        let child = Clip {
            id: "child".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "a".into(),
            edited_clip_id: "a".into(),
            ..Default::default()
        };
        let archived: HashMap<String, String> = [("a", "b"), ("b", "c"), ("c", "d"), ("d", "e")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let opts = ResolveOpts {
            max_gap_fills: 0,
            hop_cap: 2,
            concurrency: 4,
        };
        let http = ScriptedHttp::new().with_auth();
        let client = authed_client(&http);

        let info = pollster::block_on(resolve_roots(&[child], &archived, &client, &http, opts))
            .unwrap()
            .roots["child"]
            .clone();

        assert_eq!(
            info.status,
            ResolveStatus::Unresolved,
            "a chain past the hop cap terminates as unresolved"
        );
        assert_eq!(
            http.count("/api/clip"),
            0,
            "archived hops need no clip fetch"
        );
    }

    #[test]
    fn resolve_roots_without_archive_self_roots_a_purged_intermediate() {
        // The same clip WITHOUT the archived edge: the intermediate is missing
        // and unfetchable, so resolution stalls at it (external) rather than
        // reaching the true root. This is the pre-fix behaviour the archive
        // prevents.
        let child = Clip {
            id: "child".into(),
            title: "Neue Deutsche Harte".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "mid".into(),
            edited_clip_id: "mid".into(),
            ..Default::default()
        };
        let root = Clip {
            id: "root".into(),
            title: "Original".into(),
            clip_type: "gen".into(),
            ..Default::default()
        };
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/clip/mid", Reply::status(404))
            .route("/api/clips/parent", Reply::status(404));
        let client = authed_client(&http);

        let info = pollster::block_on(resolve_roots(
            &[child, root],
            &HashMap::new(),
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap()
        .roots["child"]
            .clone();

        assert_ne!(
            info.root_id, "root",
            "without the archive, resolution cannot reach the true root"
        );
        assert_ne!(
            info.status,
            ResolveStatus::Resolved,
            "the purged intermediate cannot be cleanly resolved without the archive"
        );
    }

    #[test]
    fn resolve_roots_returns_gap_filled_ancestors_for_archival() {
        // The fetched (often trashed) ancestor is handed back so a later phase
        // can archive it before Suno's purge (HARDENING H4). It resolves the
        // child's root yet stays out of the roots (download) set.
        let cover = Clip {
            id: "child".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "root".into(),
            edited_clip_id: "root".into(),
            ..Default::default()
        };
        let root_clip = serde_json::json!({
            "id": "root", "title": "Trashed Original", "status": "complete",
            "metadata": {"type": "gen"}
        })
        .to_string();
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/clip/root", Reply::json(&root_clip));
        let client = authed_client(&http);

        let resolution = pollster::block_on(resolve_roots(
            &[cover],
            &HashMap::new(),
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap();

        assert_eq!(resolution.gap_filled.len(), 1);
        assert_eq!(resolution.gap_filled[0].id, "root");
        assert_eq!(resolution.gap_filled[0].title, "Trashed Original");
        assert_eq!(resolution.roots["child"].root_id, "root");
        assert!(
            !resolution.roots.contains_key("root"),
            "gap-filled ancestors must never enter the roots set"
        );
    }

    #[test]
    fn resolve_roots_falls_back_to_the_parent_endpoint() {
        let cover = Clip {
            id: "child".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "missing".into(),
            edited_clip_id: "missing".into(),
            ..Default::default()
        };
        // The per-id fetch of `missing` 404s; the parent endpoint yields its
        // parent (the root), which the walk then bridges over `missing` to.
        let parent_body = serde_json::json!({
            "id": "root", "title": "Original", "status": "complete",
            "metadata": {"type": "gen"}
        })
        .to_string();
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/clip/missing", Reply::status(404))
            .route("/api/clips/parent", Reply::json(&parent_body));
        let client = authed_client(&http);

        let roots = pollster::block_on(resolve_roots(
            &[cover],
            &HashMap::new(),
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap()
        .roots;

        let info = &roots["child"];
        assert_eq!(info.status, ResolveStatus::Resolved);
        assert_eq!(info.root_id, "root");
        assert_eq!(info.root_title, "Original");
        assert!(
            http.count("/api/clips/parent?clip_id=missing") >= 1,
            "the missing ancestor must be resolved via the parent endpoint"
        );
    }

    #[test]
    fn resolve_roots_detects_a_cycle_without_looping() {
        let a = Clip {
            id: "a".into(),
            title: "A".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "b".into(),
            edited_clip_id: "b".into(),
            ..Default::default()
        };
        let b = Clip {
            id: "b".into(),
            title: "B".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "a".into(),
            edited_clip_id: "a".into(),
            ..Default::default()
        };
        let http = ScriptedHttp::new();
        let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());

        let roots = pollster::block_on(resolve_roots(
            &[a, b],
            &HashMap::new(),
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap()
        .roots;

        assert_eq!(roots["a"].status, ResolveStatus::Cycle);
        assert_eq!(roots["b"].status, ResolveStatus::Cycle);
        assert!(http.calls().is_empty());
    }

    #[test]
    fn resolve_roots_marks_external_when_the_budget_is_exhausted() {
        // child -> m1 (missing) -> m2 (missing) -> ...; only one gap-fill allowed.
        let child = Clip {
            id: "child".into(),
            title: "Child".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "m1".into(),
            edited_clip_id: "m1".into(),
            ..Default::default()
        };
        let m1_clip = serde_json::json!({
            "id": "m1", "title": "Middle", "status": "complete",
            "metadata": {"type": "gen", "task": "cover", "cover_clip_id": "m2", "edited_clip_id": "m2"}
        })
        .to_string();
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/clip/m1", Reply::json(&m1_clip));
        let client = authed_client(&http);
        let opts = ResolveOpts {
            max_gap_fills: 1,
            hop_cap: 64,
            concurrency: 4,
        };

        let roots = pollster::block_on(resolve_roots(
            &[child],
            &HashMap::new(),
            &client,
            &http,
            opts,
        ))
        .unwrap()
        .roots;

        let info = &roots["child"];
        assert_eq!(info.status, ResolveStatus::External);
        assert_eq!(
            info.root_id, "m2",
            "resolution stops at the first ancestor it could not fetch"
        );
        assert_eq!(http.count("/api/clip/m1"), 1);
        assert_eq!(
            http.count("/api/clip/m2"),
            0,
            "the gap-fill budget must not be exceeded"
        );
    }

    #[test]
    fn resolve_roots_external_root_endpoint_stops_the_walk() {
        // The parent endpoint reporting no parent means an external root: the
        // ancestor exists on Suno but is outside the caller's library.
        let cover = Clip {
            id: "child".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "outside".into(),
            edited_clip_id: "outside".into(),
            ..Default::default()
        };
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/clip/outside", Reply::status(404))
            .route("/api/clips/parent", Reply::status(404));
        let client = authed_client(&http);

        let roots = pollster::block_on(resolve_roots(
            &[cover],
            &HashMap::new(),
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap()
        .roots;

        let info = &roots["child"];
        assert_eq!(info.status, ResolveStatus::External);
        assert_eq!(info.root_id, "outside");
    }

    #[test]
    fn resolve_roots_seeds_a_same_owner_clip_root_but_not_a_foreign_one() {
        // A clip whose structural parent is missing triggers gap-fill. Its
        // same-owner clip_root is seeded into the same batch (an extra root
        // candidate), while its foreign-owned clip_root is NEVER fetched. The
        // structural walk alone still decides the root.
        let child = Clip {
            id: "child".into(),
            title: "Remix".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "struct-parent".into(),
            edited_clip_id: "struct-parent".into(),
            handle: "me".into(),
            clip_attribution_type: "remix".into(),
            clip_roots: vec![
                clip_root("own-root", "me"),
                clip_root("foreign-root", "stranger"),
            ],
            ..Default::default()
        };
        let struct_parent = serde_json::json!({
            "id": "struct-parent", "title": "Structural Root", "status": "complete",
            "metadata": {"type": "gen"}
        })
        .to_string();
        let own_root = serde_json::json!({
            "id": "own-root", "title": "Attribution Root", "status": "complete",
            "metadata": {"type": "gen"}
        })
        .to_string();
        // The batch returns both the structural parent and the seeded same-owner
        // root in one request; the per-id routes remain only as the fallback.
        let batch = format!(r#"{{"clips":[{struct_parent},{own_root}]}}"#);
        let http = ScriptedHttp::new()
            .with_auth()
            .route("get_songs_by_ids", Reply::json(&batch))
            .route("/api/clip/struct-parent", Reply::json(&struct_parent))
            .route("/api/clip/own-root", Reply::json(&own_root));
        let client = authed_client(&http);

        let resolution = pollster::block_on(resolve_roots(
            &[child],
            &HashMap::new(),
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap();

        // The structural walk (not clip_roots) decides the root.
        let info = &resolution.roots["child"];
        assert_eq!(info.status, ResolveStatus::Resolved);
        assert_eq!(info.root_id, "struct-parent");

        assert_eq!(
            http.count("own-root"),
            1,
            "the same-owner clip_root is seeded and fetched exactly once"
        );
        assert_eq!(
            http.count("foreign-root"),
            0,
            "a foreign-owned clip_root is NEVER seeded or fetched"
        );
    }

    #[test]
    fn resolve_roots_clip_root_seed_is_best_effort_never_bridges_or_retries() {
        // A same-owner clip_root that the batch never returns (trashed/404) is
        // simply dropped: it is never bridged, never external, never re-seeded,
        // and the structural resolution is unaffected.
        let child = Clip {
            id: "child".into(),
            title: "Remix".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "mid".into(),
            edited_clip_id: "mid".into(),
            handle: "me".into(),
            clip_attribution_type: "remix".into(),
            clip_roots: vec![clip_root("gone-root", "me")],
            ..Default::default()
        };
        // "mid" resolves to "root" over two gap-fill rounds, so the seed would be
        // re-scanned on the second round if the attempted-set did not suppress it.
        let mid = serde_json::json!({
            "id": "mid", "title": "Mid", "status": "complete",
            "metadata": {"type": "gen", "task": "cover", "cover_clip_id": "root"}
        })
        .to_string();
        let root = serde_json::json!({
            "id": "root", "title": "Root", "status": "complete",
            "metadata": {"type": "gen"}
        })
        .to_string();
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/clip/mid", Reply::json(&mid))
            .route("/api/clip/root", Reply::json(&root))
            .route("/api/clip/gone-root", Reply::status(404));
        let client = authed_client(&http);

        let resolution = pollster::block_on(resolve_roots(
            &[child],
            &HashMap::new(),
            &client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap();

        let info = &resolution.roots["child"];
        assert_eq!(info.status, ResolveStatus::Resolved);
        assert_eq!(
            info.root_id, "root",
            "the structural chain resolves normally"
        );
        assert!(
            resolution.bridges.is_empty(),
            "a dropped seed must never become a bridge"
        );
        assert!(
            !resolution.gap_filled.iter().any(|c| c.id == "gone-root"),
            "a seed the batch omits is never added"
        );
        assert_eq!(
            http.count("/api/clip/gone-root"),
            1,
            "the seed is attempted once, never retried across rounds"
        );
        assert_eq!(
            http.count("/api/clips/parent"),
            0,
            "a seed never falls through to the parent endpoint"
        );
    }
}
