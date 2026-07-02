//! Pure lineage resolution: classify a clip's parent edge and walk a library
//! back to its root ancestor.
//!
//! Suno records how a clip was derived across a scatter of metadata fields
//! (`task`, `type`, and a family of `*_clip_id` pointers plus `history` and
//! `concat_history`). This module turns those into a single primary parent per
//! clip ([`immediate_parent`]) classified by [`EdgeType`], the full set of
//! parent [`Edge`]s for the later graph store ([`lineage_edges`]), and a
//! root-ancestor map for a whole library ([`resolve_roots`]).
//!
//! Classification is deliberately blind to `is_remix`: that flag is a UI hint,
//! not a structural fact, so it never changes an edge. All resolution stays
//! pure; the only IO is the [`Http`] port reached through [`SunoClient`], used
//! solely to gap-fill ancestors that are missing from the caller's listing.

use std::collections::{HashMap, HashSet};

use crate::client::SunoClient;
use crate::clock::Clock;
use crate::error::Result;
use crate::http::Http;
use crate::model::Clip;

/// The all-zero UUID Suno uses as a "no clip" sentinel in pointer fields.
const ZERO_UUID: &str = "00000000-0000-0000-0000-000000000000";

/// How one clip relates to its immediate parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeType {
    /// A cover: re-performed audio from a source clip.
    Cover,
    /// A remaster or upsample to higher fidelity.
    Remaster,
    /// A playback-speed edit.
    SpeedEdit,
    /// A studio edit export.
    Edit,
    /// An extension appended after a source clip.
    Extend,
    /// A section (infill) replaced within a source clip.
    SectionReplace,
    /// A stitch (concatenation) of two or more segments.
    Stitch,
    /// A derived clip with a parent pointer but no more specific marker.
    Derived,
    /// An external upload with no Suno parent.
    Uploaded,
}

impl EdgeType {
    /// A human label describing the relationship to the parent.
    pub fn label(self) -> &'static str {
        match self {
            EdgeType::Cover => "Cover of",
            EdgeType::Remaster => "Remaster of",
            EdgeType::SpeedEdit => "Speed-edited from",
            EdgeType::Edit => "Edited from",
            EdgeType::Extend => "Extended from",
            EdgeType::SectionReplace => "Section replaced from",
            EdgeType::Stitch => "Stitched from",
            EdgeType::Derived => "Derived from",
            EdgeType::Uploaded => "Uploaded",
        }
    }
}

/// Whether an [`Edge`] is the clip's primary parent or a supporting one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeRole {
    /// The single lineage parent used for root resolution and album grouping.
    Primary,
    /// An additional source (an extra stitch segment, an infill's future half).
    Secondary,
}

/// One parent link of a clip, for the later lineage graph store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// The parent clip id, normalised (`m_` stripped, sentinel dropped).
    pub parent_id: String,
    /// How the clip relates to this parent.
    pub edge_type: EdgeType,
    /// Whether this is the primary parent or a secondary source.
    pub role: EdgeRole,
    /// Position within its role (0 for the primary, then secondaries in order).
    pub ordinal: u32,
    /// The metadata field this parent id was read from.
    pub source_field: &'static str,
}

/// Tunables bounding how hard [`resolve_roots`] works per call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveOpts {
    /// Maximum number of missing ancestor ids to fetch from the network.
    pub max_gap_fills: u32,
    /// Maximum hops to walk up a single chain before giving up.
    pub hop_cap: u32,
}

impl Default for ResolveOpts {
    fn default() -> Self {
        Self {
            max_gap_fills: 200,
            hop_cap: 64,
        }
    }
}

/// The outcome of resolving a clip's root ancestor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveStatus {
    /// The root was reached: a clip present in the index with no parent.
    Resolved,
    /// Resolution stopped at an ancestor outside the index (gap-fill budget
    /// exhausted, or the API reported it has no parent of its own).
    External,
    /// The root could not be determined within the hop cap.
    Unresolved,
    /// A cycle was detected while walking (pathological data).
    Cycle,
}

/// The resolved root ancestor of a clip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootInfo {
    /// The root (or boundary) ancestor id.
    pub root_id: String,
    /// The root clip's title, if it is present in the index (else empty).
    pub root_title: String,
    /// How resolution terminated.
    pub status: ResolveStatus,
}

/// The outcome of [`resolve_roots`]: a root for every input clip, plus the
/// ancestor clips fetched to bridge gaps.
///
/// `gap_filled` is kept structurally separate from `roots` on purpose. Those
/// ancestors (often trashed) exist only so lineage could be walked; a later
/// phase persists them to the graph store so a trashed ancestor is archived
/// before Suno's purge, but they must never be treated as download candidates.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The resolved root for every clip passed to [`resolve_roots`], keyed by
    /// clip id.
    pub roots: HashMap<String, RootInfo>,
    /// Ancestor clips fetched during gap-fill, sorted by id. Not download
    /// candidates: they were pulled solely to complete the lineage walk.
    pub gap_filled: Vec<Clip>,
}

/// The resolved lineage of a single clip, threaded into naming, tagging, and
/// change detection.
///
/// This is the bridge between the pure resolver ([`Resolution`]) and the parts
/// of the engine that turn a clip into files: it carries exactly the resolved
/// values that get embedded in a path or a tag (the root the clip folders
/// under, the immediate parent and how it derives from it), so those consumers
/// never re-read the now-defunct `root_ancestor_id`/`album_title` feed fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageContext {
    /// The resolved root ancestor id (the clip's own id when it is a root).
    pub root_id: String,
    /// The root ancestor's title (empty when the root is outside the index).
    pub root_title: String,
    /// The immediate parent id ([`immediate_parent`]); empty for a root.
    pub parent_id: String,
    /// How the clip derives from its parent; `None` for a root.
    pub edge_type: Option<EdgeType>,
    /// How root resolution terminated.
    pub status: ResolveStatus,
}

impl LineageContext {
    /// Build the context for `clip` from a whole-library [`Resolution`].
    ///
    /// Root id/title/status come from `resolution.roots[clip.id]`; when the clip
    /// is absent (it was not part of the resolved set) it is treated as its own
    /// resolved root. The parent id and edge come from [`immediate_parent`],
    /// which is empty/`None` for a root.
    pub fn for_clip(clip: &Clip, resolution: &Resolution) -> LineageContext {
        let (root_id, root_title, status) = match resolution.roots.get(&clip.id) {
            Some(info) => (info.root_id.clone(), info.root_title.clone(), info.status),
            None => (clip.id.clone(), clip.title.clone(), ResolveStatus::Resolved),
        };
        let (parent_id, edge_type) = match immediate_parent(clip) {
            Some((id, edge)) => (id, Some(edge)),
            None => (String::new(), None),
        };
        LineageContext {
            root_id,
            root_title,
            parent_id,
            edge_type,
            status,
        }
    }

    /// A self-rooted context for `clip`: it is treated as its own resolved root
    /// with no parent. Used as a defensive fallback where a resolved context is
    /// unavailable (a clip absent from the current desired set).
    pub fn own_root(clip: &Clip) -> LineageContext {
        LineageContext {
            root_id: clip.id.clone(),
            root_title: clip.title.clone(),
            parent_id: String::new(),
            edge_type: None,
            status: ResolveStatus::Resolved,
        }
    }

    /// The album the clip folders under: the root ancestor's title when it is a
    /// real, different root, otherwise `own_title`.
    ///
    /// A root (or an unresolved clip whose root title is empty, or a clip whose
    /// root shares its title) folders under its own title; only a resolved,
    /// differently-titled ancestor pulls the clip into the ancestor's album.
    pub fn album(&self, own_title: &str) -> String {
        let root_title = self.root_title.trim();
        if !root_title.is_empty() && self.root_title != own_title {
            self.root_title.clone()
        } else {
            own_title.to_owned()
        }
    }
}

/// Classify a clip's relationship to its parent, purely from its structure.
///
/// Inspects only `task`, `type`, and the pointer fields; never `is_remix`.
/// Returns `None` for a clip with no parent (a root, original, or upload). The
/// first matching rule wins, so more specific operations take precedence over
/// the generic `Derived` fallback.
///
/// A stitch is keyed on `type == "concat"` alone, never on a non-empty
/// `concat_history`: Suno copies a parent's `concat_history` verbatim onto
/// clips derived from a stitched track, so a cover or remaster *of* a stitch
/// still carries it. Keying on the type keeps those classified by their own
/// operation (and parented through their own pointer) instead of the stitch.
pub fn edge_type(clip: &Clip) -> Option<EdgeType> {
    let task = clip.task.as_str();
    let clip_type = clip.clip_type.as_str();

    if task == "infill" || task == "fixed_infill" {
        Some(EdgeType::SectionReplace)
    } else if task == "extend" {
        Some(EdgeType::Extend)
    } else if clip_type == "concat" {
        Some(EdgeType::Stitch)
    } else if clip_type == "edit_speed" {
        Some(EdgeType::SpeedEdit)
    } else if task == "cover" {
        Some(EdgeType::Cover)
    } else if clip_type == "upsample" || task == "upsample" {
        Some(EdgeType::Remaster)
    } else if clip_type == "edit_v3_export" {
        Some(EdgeType::Edit)
    } else if normalise_id(&clip.edited_clip_id).is_some() {
        Some(EdgeType::Derived)
    } else {
        None
    }
}

/// The clip's primary parent id and the edge that links them.
///
/// Applies the same precedence as [`edge_type`], then reads the parent pointer
/// appropriate to that operation, falling through per-op candidates in order.
/// Every id is normalised (a leading `m_` stripped, an empty or all-zero
/// sentinel treated as absent). Returns `None` for a root or when no usable
/// parent id is present.
pub fn immediate_parent(clip: &Clip) -> Option<(String, EdgeType)> {
    primary_parent(clip).map(|(id, edge, _field)| (id, edge))
}

/// Every parent link of a clip: the primary parent plus any secondaries.
///
/// The primary edge (from [`immediate_parent`]) is `Primary` with ordinal 0,
/// when a primary parent id is present. A stitch also records
/// `concat_history[1..]` as `Secondary` sources, and a section replace records
/// its `override_future_clip_id` (when distinct) as a `Secondary`. When the
/// primary pointer is absent but secondaries remain (for example a stitch whose
/// base segment id is empty), the secondaries are still emitted with their own
/// ordinals. All ids are normalised. A clip with no parent operation yields an
/// empty vector.
pub fn lineage_edges(clip: &Clip) -> Vec<Edge> {
    let Some(edge_type) = edge_type(clip) else {
        return Vec::new();
    };

    let mut edges = Vec::new();
    if let Some((parent_id, _edge, source_field)) = primary_parent(clip) {
        edges.push(Edge {
            parent_id,
            edge_type,
            role: EdgeRole::Primary,
            ordinal: 0,
            source_field,
        });
    }

    match edge_type {
        EdgeType::Stitch => {
            for (ordinal, entry) in clip.concat_history.iter().enumerate().skip(1) {
                if let Some(id) = normalise_id(&entry.id) {
                    edges.push(Edge {
                        parent_id: id,
                        edge_type,
                        role: EdgeRole::Secondary,
                        ordinal: ordinal as u32,
                        source_field: "concat_history",
                    });
                }
            }
        }
        EdgeType::SectionReplace => {
            if let Some(future) = normalise_id(&clip.override_future_clip_id)
                && edges
                    .first()
                    .is_none_or(|primary| primary.parent_id != future)
            {
                edges.push(Edge {
                    parent_id: future,
                    edge_type,
                    role: EdgeRole::Secondary,
                    ordinal: 1,
                    source_field: "override_future_clip_id",
                });
            }
        }
        _ => {}
    }

    edges
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
    client: &mut SunoClient<impl Clock>,
    http: &impl Http,
    opts: ResolveOpts,
) -> Result<Resolution> {
    let mut resolver = Resolver::new(clips, opts);
    resolver.run(client, http).await?;
    Ok(resolver.into_resolution(clips))
}

/// The clip's primary parent id, edge type, and the source field it came from.
///
/// Shared by [`immediate_parent`] and [`lineage_edges`] so the two never drift.
fn primary_parent(clip: &Clip) -> Option<(String, EdgeType, &'static str)> {
    let edge = edge_type(clip)?;
    let history_head = clip.history.first().map_or("", |entry| entry.id.as_str());
    let concat_head = clip
        .concat_history
        .first()
        .map_or("", |entry| entry.id.as_str());

    let candidates: Vec<(&str, &'static str)> = match edge {
        EdgeType::SectionReplace => vec![
            (
                clip.override_history_clip_id.as_str(),
                "override_history_clip_id",
            ),
            (
                clip.override_future_clip_id.as_str(),
                "override_future_clip_id",
            ),
            (history_head, "history"),
            (clip.edited_clip_id.as_str(), "edited_clip_id"),
        ],
        EdgeType::Extend => vec![
            (history_head, "history"),
            (clip.edited_clip_id.as_str(), "edited_clip_id"),
        ],
        EdgeType::Stitch => vec![
            (concat_head, "concat_history"),
            (clip.edited_clip_id.as_str(), "edited_clip_id"),
        ],
        EdgeType::SpeedEdit => vec![
            (clip.speed_clip_id.as_str(), "speed_clip_id"),
            (clip.edited_clip_id.as_str(), "edited_clip_id"),
        ],
        EdgeType::Cover => vec![
            (clip.cover_clip_id.as_str(), "cover_clip_id"),
            (clip.edited_clip_id.as_str(), "edited_clip_id"),
        ],
        EdgeType::Remaster => vec![
            (clip.upsample_clip_id.as_str(), "upsample_clip_id"),
            (clip.remaster_clip_id.as_str(), "remaster_clip_id"),
            (clip.edited_clip_id.as_str(), "edited_clip_id"),
        ],
        EdgeType::Edit | EdgeType::Derived => {
            vec![(clip.edited_clip_id.as_str(), "edited_clip_id")]
        }
        EdgeType::Uploaded => vec![],
    };

    candidates
        .into_iter()
        .find_map(|(value, field)| normalise_id(value).map(|id| (id, edge, field)))
}

/// Normalise a raw pointer id: strip a leading `m_`, and treat an empty or
/// all-zero sentinel value as absent.
fn normalise_id(id: &str) -> Option<String> {
    let id = id.strip_prefix("m_").unwrap_or(id);
    if id.is_empty() || id == ZERO_UUID {
        None
    } else {
        Some(id.to_string())
    }
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
struct Resolver {
    index: HashMap<String, Clip>,
    gap_filled: HashSet<String>,
    bridges: HashMap<String, String>,
    external: HashSet<String>,
    memo: HashMap<String, RootInfo>,
    targets: Vec<String>,
    budget: u32,
    hop_cap: u32,
}

impl Resolver {
    fn new(clips: &[Clip], opts: ResolveOpts) -> Self {
        let index = clips
            .iter()
            .map(|clip| (clip.id.clone(), clip.clone()))
            .collect();
        let targets = clips.iter().map(|clip| clip.id.clone()).collect();
        Self {
            index,
            gap_filled: HashSet::new(),
            bridges: HashMap::new(),
            external: HashSet::new(),
            memo: HashMap::new(),
            targets,
            budget: opts.max_gap_fills,
            hop_cap: opts.hop_cap,
        }
    }

    /// Resolve every target, gap-filling missing ancestors until the whole set
    /// is settled or the budget runs out.
    async fn run(&mut self, client: &mut SunoClient<impl Clock>, http: &impl Http) -> Result<()> {
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

            let (parent, title) = match self.index.get(&current) {
                Some(clip) => (immediate_parent(clip), clip.title.clone()),
                None => return Walk::Blocked(current),
            };

            let Some((parent_id, _edge)) = parent else {
                let info = RootInfo {
                    root_id: current.clone(),
                    root_title: title,
                    status: ResolveStatus::Resolved,
                };
                self.assign(&chain, &info);
                self.memo.insert(current, info);
                return Walk::Resolved;
            };

            visited.insert(current.clone());
            chain.push(current);

            if self.index.contains_key(&parent_id) {
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
    /// the parent endpoint. Returns whether the index (or bridges/externals)
    /// grew, so the caller can detect a stalled resolution.
    async fn gap_fill(
        &mut self,
        client: &mut SunoClient<impl Clock>,
        http: &impl Http,
        frontier: &[String],
    ) -> Result<bool> {
        let mut want: Vec<String> = frontier
            .iter()
            .filter(|id| !self.known(id))
            .cloned()
            .collect();
        if want.is_empty() {
            return Ok(false);
        }
        want.sort();
        let take = (self.budget as usize).min(want.len());
        let batch: Vec<String> = want.into_iter().take(take).collect();
        self.budget -= batch.len() as u32;

        let refs: Vec<&str> = batch.iter().map(String::as_str).collect();
        let fetched = client.get_clips_by_ids(http, &refs).await?;

        let mut returned: HashSet<String> = HashSet::new();
        let mut progressed = false;
        for clip in fetched {
            returned.insert(clip.id.clone());
            if self.insert_ancestor(clip) {
                progressed = true;
            }
        }

        for id in &batch {
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
        self.index.contains_key(id) || self.bridges.contains_key(id) || self.external.contains(id)
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

        Resolution { roots, gap_filled }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ClerkAuth;
    use crate::model::HistoryEntry;
    use crate::testutil::{RecordingClock, Reply, ScriptedHttp};

    fn history(id: &str) -> HistoryEntry {
        HistoryEntry {
            id: id.to_owned(),
            ..Default::default()
        }
    }

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
        let mut auth = ClerkAuth::new("eyJtoken");
        pollster::block_on(auth.authenticate(http)).unwrap();
        SunoClient::new(auth, RecordingClock::new())
    }

    #[test]
    fn edge_type_labels_read_naturally() {
        assert_eq!(EdgeType::Cover.label(), "Cover of");
        assert_eq!(EdgeType::Remaster.label(), "Remaster of");
        assert_eq!(EdgeType::SpeedEdit.label(), "Speed-edited from");
        assert_eq!(EdgeType::Edit.label(), "Edited from");
        assert_eq!(EdgeType::Extend.label(), "Extended from");
        assert_eq!(EdgeType::SectionReplace.label(), "Section replaced from");
        assert_eq!(EdgeType::Stitch.label(), "Stitched from");
        assert_eq!(EdgeType::Derived.label(), "Derived from");
        assert_eq!(EdgeType::Uploaded.label(), "Uploaded");
    }

    #[test]
    fn classifies_remaster_cover_edit_and_root_across_chain1() {
        let clips = chain1_clips();

        assert_eq!(edge_type(&clips[0]), Some(EdgeType::Remaster));
        assert_eq!(
            immediate_parent(&clips[0]),
            Some(("52962dae".into(), EdgeType::Remaster))
        );

        assert_eq!(edge_type(&clips[1]), Some(EdgeType::Cover));
        assert_eq!(
            immediate_parent(&clips[1]),
            Some(("536e1b92".into(), EdgeType::Cover))
        );

        assert_eq!(edge_type(&clips[4]), Some(EdgeType::Edit));
        assert_eq!(
            immediate_parent(&clips[4]),
            Some(("dfb59a04".into(), EdgeType::Edit))
        );

        assert_eq!(edge_type(&clips[5]), None);
        assert_eq!(immediate_parent(&clips[5]), None);
    }

    #[test]
    fn classifies_speed_edit_from_speed_pointer_without_edited() {
        // Real `chain2` shape: edit_speed carries speed_clip_id and no edited_clip_id.
        let clip = Clip {
            id: "6e5193b1".into(),
            title: "Go Xavi Go, Fast. (Drum n' Bass Version)".into(),
            clip_type: "edit_speed".into(),
            is_remix: true,
            speed_clip_id: "2b69882c".into(),
            ..Default::default()
        };
        assert_eq!(edge_type(&clip), Some(EdgeType::SpeedEdit));
        assert_eq!(
            immediate_parent(&clip),
            Some(("2b69882c".into(), EdgeType::SpeedEdit))
        );
    }

    #[test]
    fn empty_task_gen_is_a_root() {
        // Real `chain2` root: gen with an empty task string.
        let clip = Clip {
            id: "b4f16694".into(),
            title: "Go Xavi Go, Fast.".into(),
            clip_type: "gen".into(),
            task: String::new(),
            ..Default::default()
        };
        assert_eq!(edge_type(&clip), None);
        assert_eq!(immediate_parent(&clip), None);
    }

    #[test]
    fn classifies_extend_from_history_head() {
        let clip = Clip {
            id: "9a3dcb67".into(),
            title: "Extended".into(),
            clip_type: "gen".into(),
            task: "extend".into(),
            edited_clip_id: "0a3c311a".into(),
            history: vec![HistoryEntry {
                id: "0a3c311a".into(),
                continue_at: Some(115.35),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(edge_type(&clip), Some(EdgeType::Extend));
        assert_eq!(
            immediate_parent(&clip),
            Some(("0a3c311a".into(), EdgeType::Extend))
        );
    }

    #[test]
    fn classifies_infill_with_override_history_precedence() {
        // Real infill shape: override_history wins over future, history, and edited.
        let clip = Clip {
            id: "c0ce5c48".into(),
            title: "Section replaced".into(),
            clip_type: "gen".into(),
            task: "infill".into(),
            edited_clip_id: "cf37e05f".into(),
            override_history_clip_id: "d3d28e59".into(),
            override_future_clip_id: "ea88571e".into(),
            history: vec![HistoryEntry {
                id: "cf37e05f".into(),
                infill: true,
                infill_start_s: Some(20.4),
                infill_end_s: Some(24.92),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(edge_type(&clip), Some(EdgeType::SectionReplace));
        assert_eq!(
            immediate_parent(&clip),
            Some(("d3d28e59".into(), EdgeType::SectionReplace))
        );
    }

    #[test]
    fn fixed_infill_is_also_section_replace() {
        let clip = Clip {
            task: "fixed_infill".into(),
            override_history_clip_id: "past".into(),
            edited_clip_id: "edited".into(),
            ..Default::default()
        };
        assert_eq!(edge_type(&clip), Some(EdgeType::SectionReplace));
        assert_eq!(
            immediate_parent(&clip),
            Some(("past".into(), EdgeType::SectionReplace))
        );
    }

    #[test]
    fn classifies_stitch_from_concat_base() {
        // Real concat shape: type=concat, base segment first in concat_history.
        let clip = Clip {
            id: "43ba1ce3".into(),
            title: "Stitched".into(),
            clip_type: "concat".into(),
            concat_history: vec![
                HistoryEntry {
                    id: "ead64fbe".into(),
                    continue_at: Some(149.19),
                    ..Default::default()
                },
                history("da47b824"),
            ],
            ..Default::default()
        };
        assert_eq!(edge_type(&clip), Some(EdgeType::Stitch));
        assert_eq!(
            immediate_parent(&clip),
            Some(("ead64fbe".into(), EdgeType::Stitch))
        );
    }

    #[test]
    fn inherited_concat_history_without_concat_type_is_not_a_stitch() {
        // Suno copies a parent stitch's concat_history onto derived clips. A
        // plain `gen` that merely carries it (no type=concat, no other marker)
        // must NOT be read as a stitch; here it has no parent pointer, so it is
        // a root.
        let clip = Clip {
            clip_type: "gen".into(),
            concat_history: vec![history("base"), history("second")],
            ..Default::default()
        };
        assert_eq!(edge_type(&clip), None);
        assert_eq!(immediate_parent(&clip), None);
    }

    #[test]
    fn cover_of_a_stitch_classifies_as_cover_not_stitch() {
        // A cover OF a stitched track inherits the parent's concat_history but is
        // itself a cover: it must classify as Cover and parent via cover_clip_id,
        // never as a Stitch pointing at an inherited concat segment.
        let clip = Clip {
            id: "cov".into(),
            title: "Cover of a stitch".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "stitch-parent".into(),
            edited_clip_id: "stitch-parent".into(),
            concat_history: vec![history("inherited-base"), history("inherited-seg")],
            ..Default::default()
        };
        assert_eq!(edge_type(&clip), Some(EdgeType::Cover));
        assert_eq!(
            immediate_parent(&clip),
            Some(("stitch-parent".into(), EdgeType::Cover))
        );
    }

    #[test]
    fn upload_is_a_root() {
        let clip = Clip {
            id: "4770ef56".into(),
            title: "Uploaded audio".into(),
            clip_type: "upload".into(),
            ..Default::default()
        };
        assert_eq!(edge_type(&clip), None);
        assert_eq!(immediate_parent(&clip), None);
    }

    #[test]
    fn edited_only_clip_is_derived() {
        // A task the resolver has no specific rule for, but a parent pointer.
        let clip = Clip {
            clip_type: "gen".into(),
            task: "chop_sample_condition".into(),
            edited_clip_id: "parent-x".into(),
            ..Default::default()
        };
        assert_eq!(edge_type(&clip), Some(EdgeType::Derived));
        assert_eq!(
            immediate_parent(&clip),
            Some(("parent-x".into(), EdgeType::Derived))
        );
    }

    #[test]
    fn unmarked_clip_without_pointer_is_a_root() {
        let clip = Clip {
            clip_type: "gen".into(),
            task: "chop_sample_condition".into(),
            ..Default::default()
        };
        assert_eq!(edge_type(&clip), None);
        assert_eq!(immediate_parent(&clip), None);
    }

    #[test]
    fn is_remix_does_not_change_classification() {
        let base = Clip {
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "root-1".into(),
            edited_clip_id: "root-1".into(),
            ..Default::default()
        };
        let mut with_flag = base.clone();
        with_flag.is_remix = true;
        let mut without_flag = base;
        without_flag.is_remix = false;

        assert_eq!(edge_type(&with_flag), edge_type(&without_flag));
        assert_eq!(
            immediate_parent(&with_flag),
            immediate_parent(&without_flag)
        );
        assert_eq!(edge_type(&with_flag), Some(EdgeType::Cover));
        assert_eq!(
            immediate_parent(&with_flag),
            Some(("root-1".into(), EdgeType::Cover))
        );
    }

    #[test]
    fn zero_uuid_cover_falls_back_to_edited() {
        let clip = Clip {
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: ZERO_UUID.into(),
            edited_clip_id: "real-parent".into(),
            ..Default::default()
        };
        assert_eq!(
            immediate_parent(&clip),
            Some(("real-parent".into(), EdgeType::Cover))
        );
    }

    #[test]
    fn m_prefix_is_stripped_from_history_and_concat_ids() {
        let extend = Clip {
            clip_type: "gen".into(),
            task: "extend".into(),
            history: vec![history("m_abc123")],
            ..Default::default()
        };
        assert_eq!(
            immediate_parent(&extend),
            Some(("abc123".into(), EdgeType::Extend))
        );

        let stitch = Clip {
            clip_type: "concat".into(),
            concat_history: vec![history("m_base"), history("m_second")],
            ..Default::default()
        };
        let edges = lineage_edges(&stitch);
        assert_eq!(edges[0].parent_id, "base");
        assert_eq!(edges[1].parent_id, "second");
        assert_eq!(edges[1].role, EdgeRole::Secondary);
    }

    #[test]
    fn lineage_edges_of_a_root_is_empty() {
        let clip = Clip {
            clip_type: "gen".into(),
            ..Default::default()
        };
        assert!(lineage_edges(&clip).is_empty());
    }

    #[test]
    fn lineage_edges_records_stitch_secondaries_in_order() {
        let clip = Clip {
            clip_type: "concat".into(),
            concat_history: vec![history("base"), history("seg1"), history("seg2")],
            ..Default::default()
        };
        let edges = lineage_edges(&clip);
        assert_eq!(
            edges,
            vec![
                Edge {
                    parent_id: "base".into(),
                    edge_type: EdgeType::Stitch,
                    role: EdgeRole::Primary,
                    ordinal: 0,
                    source_field: "concat_history",
                },
                Edge {
                    parent_id: "seg1".into(),
                    edge_type: EdgeType::Stitch,
                    role: EdgeRole::Secondary,
                    ordinal: 1,
                    source_field: "concat_history",
                },
                Edge {
                    parent_id: "seg2".into(),
                    edge_type: EdgeType::Stitch,
                    role: EdgeRole::Secondary,
                    ordinal: 2,
                    source_field: "concat_history",
                },
            ]
        );
    }

    #[test]
    fn lineage_edges_emits_secondaries_when_the_primary_is_absent() {
        // A stitch whose base segment id is empty still has real secondary
        // segments: they must be emitted (with their own ordinals) rather than
        // dropped for want of a primary.
        let clip = Clip {
            clip_type: "concat".into(),
            concat_history: vec![history(""), history("seg1"), history("seg2")],
            ..Default::default()
        };
        let edges = lineage_edges(&clip);
        assert_eq!(
            edges,
            vec![
                Edge {
                    parent_id: "seg1".into(),
                    edge_type: EdgeType::Stitch,
                    role: EdgeRole::Secondary,
                    ordinal: 1,
                    source_field: "concat_history",
                },
                Edge {
                    parent_id: "seg2".into(),
                    edge_type: EdgeType::Stitch,
                    role: EdgeRole::Secondary,
                    ordinal: 2,
                    source_field: "concat_history",
                },
            ],
            "secondaries survive an empty primary base segment"
        );
    }

    #[test]
    fn lineage_edges_records_infill_future_as_secondary() {
        let clip = Clip {
            task: "infill".into(),
            override_history_clip_id: "past".into(),
            override_future_clip_id: "future".into(),
            ..Default::default()
        };
        let edges = lineage_edges(&clip);
        assert_eq!(edges[0].parent_id, "past");
        assert_eq!(edges[0].role, EdgeRole::Primary);
        assert_eq!(edges[0].source_field, "override_history_clip_id");
        assert_eq!(
            edges[1],
            Edge {
                parent_id: "future".into(),
                edge_type: EdgeType::SectionReplace,
                role: EdgeRole::Secondary,
                ordinal: 1,
                source_field: "override_future_clip_id",
            }
        );
    }

    #[test]
    fn resolve_roots_walks_a_connected_chain_with_no_http() {
        let http = ScriptedHttp::new();
        let mut client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
        let clips = chain1_clips();

        let roots = pollster::block_on(resolve_roots(
            &clips,
            &mut client,
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
        let mut client = authed_client(&http);

        let roots = pollster::block_on(resolve_roots(
            &[cover],
            &mut client,
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
        let mut client = authed_client(&http);

        let resolution = pollster::block_on(resolve_roots(
            &[cover],
            &mut client,
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
        let mut client = authed_client(&http);

        let roots = pollster::block_on(resolve_roots(
            &[cover],
            &mut client,
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
        let mut client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());

        let roots = pollster::block_on(resolve_roots(
            &[a, b],
            &mut client,
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
        let mut client = authed_client(&http);
        let opts = ResolveOpts {
            max_gap_fills: 1,
            hop_cap: 64,
        };

        let roots = pollster::block_on(resolve_roots(&[child], &mut client, &http, opts))
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
        let mut client = authed_client(&http);

        let roots = pollster::block_on(resolve_roots(
            &[cover],
            &mut client,
            &http,
            ResolveOpts::default(),
        ))
        .unwrap()
        .roots;

        let info = &roots["child"];
        assert_eq!(info.status, ResolveStatus::External);
        assert_eq!(info.root_id, "outside");
    }

    fn resolution_with(roots: Vec<(&str, RootInfo)>) -> Resolution {
        Resolution {
            roots: roots
                .into_iter()
                .map(|(id, info)| (id.to_owned(), info))
                .collect(),
            gap_filled: Vec::new(),
        }
    }

    #[test]
    fn context_for_a_root_uses_its_own_id_and_title() {
        let root = Clip {
            id: "root-1".into(),
            title: "Original".into(),
            ..Default::default()
        };
        let resolution = resolution_with(vec![(
            "root-1",
            RootInfo {
                root_id: "root-1".into(),
                root_title: "Original".into(),
                status: ResolveStatus::Resolved,
            },
        )]);

        let ctx = LineageContext::for_clip(&root, &resolution);
        assert_eq!(ctx.root_id, "root-1");
        assert_eq!(ctx.root_title, "Original");
        assert_eq!(ctx.parent_id, "");
        assert_eq!(ctx.edge_type, None);
        // A root folders under its own title.
        assert_eq!(ctx.album("Original"), "Original");
    }

    #[test]
    fn context_for_a_remix_carries_root_and_parent() {
        let child = Clip {
            id: "child-1".into(),
            title: "Remix".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "root-1".into(),
            edited_clip_id: "root-1".into(),
            ..Default::default()
        };
        let resolution = resolution_with(vec![(
            "child-1",
            RootInfo {
                root_id: "root-1".into(),
                root_title: "Original".into(),
                status: ResolveStatus::Resolved,
            },
        )]);

        let ctx = LineageContext::for_clip(&child, &resolution);
        assert_eq!(ctx.root_id, "root-1");
        assert_eq!(ctx.root_title, "Original");
        assert_eq!(ctx.parent_id, "root-1");
        assert_eq!(ctx.edge_type, Some(EdgeType::Cover));
        // A remix folders under the root's album title, not its own.
        assert_eq!(ctx.album("Remix"), "Original");
    }

    #[test]
    fn context_absent_from_resolution_is_its_own_root() {
        let clip = Clip {
            id: "lonely".into(),
            title: "Solo".into(),
            ..Default::default()
        };
        let ctx = LineageContext::for_clip(&clip, &resolution_with(vec![]));
        assert_eq!(ctx.root_id, "lonely");
        assert_eq!(ctx.root_title, "Solo");
        assert_eq!(ctx.status, ResolveStatus::Resolved);
        assert_eq!(ctx.album("Solo"), "Solo");
    }

    #[test]
    fn album_falls_back_to_own_title_when_root_title_is_empty() {
        let ctx = LineageContext {
            root_id: "outside".into(),
            root_title: String::new(),
            parent_id: "outside".into(),
            edge_type: Some(EdgeType::Cover),
            status: ResolveStatus::External,
        };
        assert_eq!(ctx.album("My Title"), "My Title");
    }

    #[test]
    fn own_root_has_no_parent() {
        let clip = Clip {
            id: "solo".into(),
            title: "Solo".into(),
            ..Default::default()
        };
        let ctx = LineageContext::own_root(&clip);
        assert_eq!(ctx.root_id, "solo");
        assert_eq!(ctx.parent_id, "");
        assert_eq!(ctx.edge_type, None);
    }
}
