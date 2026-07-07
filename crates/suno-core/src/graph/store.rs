//! The [`LineageStore`] container: the relational archive plus its query and
//! ingest logic. [`LineageStore::update`] is the sole mutator; the store
//! takes the wall clock as a `now` string so it stays free of IO.

use std::collections::btree_map::Iter;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::album_art::{AlbumArt, PlaylistState};
use crate::identity::Owner;
use crate::lineage::{
    AttributionEdge, Edge, EdgeRole, EdgeType, LineageContext, Resolution, ResolveStatus, RootInfo,
    attribution_edges, immediate_parent, lineage_edges,
};
use crate::model::Clip;

use super::node::{CacheEntry, EdgeStatus, Node, StoredEdge};

/// The whole lineage graph, kept relational for a clean SQLite migration.
///
/// `nodes` and `resolution_cache` are [`BTreeMap`]s and `edges` is sorted after
/// every [`update`](LineageStore::update), so serialisation is deterministic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LineageStore {
    /// On-disk schema version, so a future migration can branch on it.
    pub schema_version: u32,
    /// Every clip ever seen (including trashed ancestors), keyed by clip id.
    pub(crate) nodes: BTreeMap<String, Node>,
    /// Every observed parent link, as a flat relational list.
    pub(crate) edges: Vec<StoredEdge>,
    /// The last resolved (or last-known) root per clip, keyed by clip id.
    pub(crate) resolution_cache: BTreeMap<String, CacheEntry>,
    /// The reconciled folder-art state per album, keyed by the album's stable
    /// root id (HARDENING H2). Additive: absent in older stores, defaults empty.
    /// Stays `pub`: the CLI executor and reconcile inputs borrow this map across
    /// the crate boundary (`&mut store.albums`).
    pub albums: BTreeMap<String, AlbumArt>,
    /// The reconciled `.m3u8` state per playlist, keyed by the playlist's Suno
    /// id (the synthetic `"liked"` id for the liked feed). Additive: absent in
    /// older stores, defaults empty.
    /// Stays `pub`: the CLI executor and reconcile inputs borrow this map across
    /// the crate boundary (`&mut store.playlists`).
    pub playlists: BTreeMap<String, PlaylistState>,
    /// The Suno account this library is pinned to (trust-on-first-use). Absent
    /// in older stores and in a fresh library until the first run adopts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner: Option<Owner>,
    /// Manual album-name overrides, keyed by lineage root id, layered over the
    /// store each run from config (see [`set_album_overrides`]). Runtime-only:
    /// it is never serialised, so it can never persist into the durable graph or
    /// silently outlive its config entry.
    ///
    /// [`set_album_overrides`]: LineageStore::set_album_overrides
    #[serde(skip)]
    pub(crate) album_overrides: BTreeMap<String, String>,
    /// The set of root ids eligible for an album name (an override or a
    /// collision suffix): every non-empty `root_id` that appears as a *value* in
    /// [`resolution_cache`](Self::resolution_cache). This is the single source
    /// both override-application ([`effective_root_title`]) and collision
    /// detection ([`colliding_root_titles`]) draw from, so they can never
    /// disagree about which roots exist. Runtime-only and derived from the cache
    /// via [`refresh_eligible_roots`]; kept in sync by [`update`] and refreshed
    /// after a load.
    ///
    /// [`effective_root_title`]: LineageStore::effective_root_title
    /// [`colliding_root_titles`]: LineageStore::colliding_root_titles
    /// [`refresh_eligible_roots`]: LineageStore::refresh_eligible_roots
    /// [`update`]: LineageStore::update
    #[serde(skip)]
    eligible_root_ids: HashSet<String>,
    /// Runtime index from edge identity to its row in `edges`, rebuilt from the
    /// vector and kept in sync so upserts are O(1) without changing on-disk
    /// shape.
    #[serde(skip)]
    edge_index: HashMap<EdgeKey, usize>,
}

impl Default for LineageStore {
    fn default() -> Self {
        Self {
            schema_version: 1,
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            resolution_cache: BTreeMap::new(),
            albums: BTreeMap::new(),
            playlists: BTreeMap::new(),
            owner: None,
            album_overrides: BTreeMap::new(),
            eligible_root_ids: HashSet::new(),
            edge_index: HashMap::new(),
        }
    }
}

/// Equality over the durable graph only.
///
/// `album_overrides` and `eligible_root_ids` are runtime-only overlays
/// (`#[serde(skip)]`): the first is layered from config each run, the second is
/// a cache derived from `resolution_cache`. Neither is part of the persisted
/// relational shape, so two stores are equal when their durable content is,
/// regardless of the overrides in force or whether the derived set has been
/// refreshed after a load.
impl PartialEq for LineageStore {
    fn eq(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.nodes == other.nodes
            && self.edges == other.edges
            && self.resolution_cache == other.resolution_cache
            && self.albums == other.albums
            && self.playlists == other.playlists
            && self.owner == other.owner
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EdgeKey {
    child_id: String,
    parent_id: String,
    edge_type: String,
    role: EdgeRole,
    ordinal: u32,
}

impl EdgeKey {
    fn new(child_id: &str, parent_id: &str, edge_type: &str, role: EdgeRole, ordinal: u32) -> Self {
        Self {
            child_id: child_id.to_owned(),
            parent_id: parent_id.to_owned(),
            edge_type: edge_type.to_owned(),
            role,
            ordinal,
        }
    }

    fn from_stored(edge: &StoredEdge) -> Self {
        Self::new(
            &edge.child_id,
            &edge.parent_id,
            &edge.edge_type,
            edge.role,
            edge.ordinal,
        )
    }
}

impl LineageStore {
    /// Create an empty store at the current schema version.
    pub fn new() -> Self {
        Self::default()
    }

    /// Layer this run's manual album-name overrides onto the store.
    ///
    /// Keyed by lineage root id, sourced from the account's config each run and
    /// never persisted (the field is `#[serde(skip)]`). Applied wherever the
    /// album title is resolved ([`context_for`], [`album_for_id`],
    /// [`colliding_root_titles`]), so a single call makes the folder path, the
    /// `ALBUM` tag, the change hash, the on-disk index, and disambiguation all
    /// reflect the preferred name from one source of truth.
    ///
    /// [`context_for`]: LineageStore::context_for
    /// [`album_for_id`]: LineageStore::album_for_id
    /// [`colliding_root_titles`]: LineageStore::colliding_root_titles
    pub fn set_album_overrides(&mut self, overrides: BTreeMap<String, String>) {
        self.album_overrides = overrides;
    }

    /// The effective album title for a lineage root: the manual override when
    /// one is configured for `root_id` AND that root is eligible (see
    /// [`eligible_root_ids`]), otherwise the derived `root_title`.
    ///
    /// This is the single point at which a manual override supplants the derived
    /// name, so every consumer that resolves an album title routes through it.
    ///
    /// The override is applied only when `root_id` is in the eligible set —
    /// exactly the roots [`colliding_root_titles`] groups over. Tying
    /// override-application and collision-detection to one set means an override
    /// is never applied to a root that collision detection cannot suffix, which
    /// would otherwise let two distinct roots share one album folder. The set is
    /// the non-empty `root_id`s appearing as cache *values*, so it covers normal
    /// resolved roots and gap-filled/archived ancestor roots (a value for their
    /// children, never a key) alike. A truly uncached fallback self-root on a
    /// resolution-failed run appears nowhere in the cache and is NOT overridden;
    /// it folders under its own derived title this run and converges to the
    /// override on a later run once the root resolves. This is intended, safe
    /// degraded behaviour: a transient resolution miss can never collapse two
    /// albums onto one path.
    ///
    /// [`eligible_root_ids`]: Self::eligible_root_ids
    /// [`colliding_root_titles`]: LineageStore::colliding_root_titles
    fn effective_root_title(&self, root_id: &str, root_title: String) -> String {
        if !self.eligible_root_ids.contains(root_id) {
            return root_title;
        }
        match self.album_overrides.get(root_id) {
            Some(name) if !name.trim().is_empty() => name.clone(),
            _ => root_title,
        }
    }

    /// Recompute the eligible-root set from the resolution cache.
    ///
    /// The set is the non-empty `root_id`s across the cache's values (the roots
    /// every clip resolves to), which is exactly what [`colliding_root_titles`]
    /// groups over. Called at the end of [`update`] and after a load (the field
    /// is not serialised), so the set always reflects the populated cache.
    ///
    /// [`colliding_root_titles`]: LineageStore::colliding_root_titles
    /// [`update`]: LineageStore::update
    pub fn refresh_eligible_roots(&mut self) {
        self.eligible_root_ids = self
            .resolution_cache
            .values()
            .map(|entry| entry.root_id.as_str())
            .filter(|root_id| !root_id.is_empty())
            .map(str::to_owned)
            .collect();
    }

    /// The eligible-root set, for tests that assert override-application and
    /// collision detection share one domain.
    #[cfg(test)]
    pub(crate) fn eligible_root_ids_for_test(&self) -> &HashSet<String> {
        &self.eligible_root_ids
    }

    /// The node for `id`, if present.
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// The cached root resolution for `id`, if present.
    pub fn get_root(&self, id: &str) -> Option<&CacheEntry> {
        self.resolution_cache.get(id)
    }

    /// Build a [`LineageContext`] for `clip` from the durable store.
    ///
    /// This is the source of truth for every file-affecting lineage decision
    /// (album folder, embedded tags, the change hash), so a dropped resolution
    /// call never rewrites the library (HARDENING H3). The root comes from the
    /// monotonic resolution cache (the clip's own id when the store has no
    /// better answer) and the root title and date from that root's archived
    /// node, so a transient miss keeps the last-known-good album (even for a
    /// since-purged ancestor) and the Year tag anchors on the root's year. The
    /// parent edge is read structurally from the clip itself.
    pub fn context_for(&self, clip: &Clip) -> LineageContext {
        let cached = self.get_root(&clip.id);
        let root_id = cached
            .map(|entry| entry.root_id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| clip.id.clone());
        let root_title = self
            .node(&root_id)
            .map(|node| node.title.clone())
            .unwrap_or_else(|| clip.title.clone());
        let root_title = self.effective_root_title(&root_id, root_title);
        let root_date = self
            .node(&root_id)
            .map(|node| node.created_at.clone())
            .unwrap_or_else(|| clip.created_at.clone());
        let (parent_id, edge_type) = match immediate_parent(clip) {
            Some((id, edge)) => (id, Some(edge)),
            None => (String::new(), None),
        };
        let status = cached
            .map(|entry| entry.status)
            .unwrap_or(ResolveStatus::Resolved);
        LineageContext {
            root_id,
            root_title,
            root_date,
            parent_id,
            edge_type,
            status,
        }
    }

    /// The canonical logical album title for a clip identified only by `id`.
    ///
    /// The store-side counterpart of `context_for(clip).album(clip.title)` for a
    /// clip that is not part of the current run (so no live [`Clip`] is on hand).
    /// The clip's own title and its root come from the archived nodes and the
    /// monotonic resolution cache, then the same [`LineageContext::album`] rule
    /// decides whether the clip folders under its root's album or its own title.
    /// A clip absent from the store folds to a self-root with an empty title.
    pub fn album_for_id(&self, id: &str) -> String {
        let own = self.node(id);
        let own_title = own.map(|node| node.title.clone()).unwrap_or_default();
        let own_created_at = own.map(|node| node.created_at.clone()).unwrap_or_default();
        let root_id = self
            .get_root(id)
            .map(|entry| entry.root_id.clone())
            .filter(|root| !root.is_empty())
            .unwrap_or_else(|| id.to_owned());
        let root_title = self
            .node(&root_id)
            .map(|node| node.title.clone())
            .unwrap_or_else(|| own_title.clone());
        let root_title = self.effective_root_title(&root_id, root_title);
        let root_date = self
            .node(&root_id)
            .map(|node| node.created_at.clone())
            .unwrap_or(own_created_at);
        let context = LineageContext {
            root_id,
            root_title,
            root_date,
            parent_id: String::new(),
            edge_type: None,
            status: ResolveStatus::Resolved,
        };
        context.album(&own_title)
    }

    /// The set of *effective* album titles shared by more than one distinct
    /// root.
    ///
    /// Two distinct roots must never share an album folder (two different
    /// uploads titled "Break Through" exist), so naming appends the short root
    /// id to the album of any clip whose album is in this set. It is computed
    /// from the whole archive — every eligible root (see
    /// [`eligible_root_ids`](Self::eligible_root_ids)) paired with its effective
    /// title (a manual override when configured, else the node title) — so the
    /// decision is stable across runs and independent of the current batch: a
    /// `--since`/`--limit` slice that shows only one of two same-titled roots
    /// still disambiguates, instead of oscillating between a bare and a suffixed
    /// folder. Because it folds overrides in first, a rename that collides two
    /// albums (or one that resolves a collision) is honoured consistently with
    /// the path, tag, and hash.
    ///
    /// This iterates the exact same eligible-root set that
    /// [`effective_root_title`](Self::effective_root_title) gates overrides on,
    /// so an override affects a root's album name if and only if that root is
    /// grouped here — the two can never disagree. The set is the non-empty
    /// `root_id`s appearing as cache values, so it includes gap-filled/archived
    /// ancestor roots (a value for their children, never a key) and node-less
    /// cached roots. A root with no node and no override has an empty effective
    /// title and is skipped. An uncached fallback self-root on a
    /// resolution-failed run is in neither set.
    pub fn colliding_root_titles(&self) -> BTreeSet<String> {
        let mut roots_by_title: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for root_id in &self.eligible_root_ids {
            let node_title = self
                .nodes
                .get(root_id)
                .map(|node| node.title.clone())
                .unwrap_or_default();
            let effective = self.effective_root_title(root_id, node_title);
            let title = effective.trim();
            if title.is_empty() {
                continue;
            }
            roots_by_title
                .entry(title.to_owned())
                .or_default()
                .insert(root_id.clone());
        }
        roots_by_title
            .into_iter()
            .filter(|(_, roots)| roots.len() > 1)
            .map(|(title, _)| title)
            .collect()
    }

    /// Number of nodes in the graph.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True when the graph holds no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Iterate nodes in clip-id order.
    pub fn iter(&self) -> Iter<'_, String, Node> {
        self.nodes.iter()
    }

    /// Fold this run's clips and their [`Resolution`] into the store.
    ///
    /// Pure: it takes `now` (an ISO timestamp) from the caller rather than
    /// reading a clock. Upserts a node for every clip *and* every gap-filled
    /// ancestor (so trashed ancestors are archived), upserts an edge for every
    /// [`lineage_edges`] link, and refreshes the monotonic resolution cache.
    /// `edges` is left sorted so the serialised form is deterministic.
    pub fn update(&mut self, clips: &[Clip], resolution: &Resolution, now: &str) {
        self.rebuild_edge_index();

        for clip in clips {
            self.upsert_node(clip, now);
        }
        // Gap-filled ancestors are not download candidates, but their lineage
        // must be archived before Suno purges them, so they become nodes too.
        for clip in &resolution.gap_filled {
            self.upsert_node(clip, now);
        }

        // Persist edges for the input clips AND the gap-filled ancestors. A
        // gap-filled ancestor carries its own parent pointer, so recording its
        // `lineage_edges` keeps the stored graph connected (an intermediate
        // remix is no longer a disconnected root) and lets a later run resolve
        // through it from the store, without re-fetching, even after Suno purges
        // it. Parent-endpoint bridges have no clip of their own, so they are
        // persisted directly below to keep that hop durable too.
        for clip in clips.iter().chain(resolution.gap_filled.iter()) {
            for edge in lineage_edges(clip) {
                self.upsert_edge(&clip.id, &edge, now);
            }
        }
        for (child_id, parent_id) in &resolution.bridges {
            let edge = Edge {
                parent_id: parent_id.clone(),
                edge_type: EdgeType::Derived,
                role: EdgeRole::Primary,
                ordinal: 0,
                source_field: "parent_endpoint",
            };
            self.upsert_edge(child_id, &edge, now);
        }
        // Attribution edges from `clip_roots` are additive and informational,
        // never structural: they carry the open attribution slug directly and
        // are role Secondary, so `archived_parents` (Primary, ordinal 0) never
        // reads them and root resolution stays untouched.
        for clip in clips.iter().chain(resolution.gap_filled.iter()) {
            for edge in attribution_edges(clip) {
                self.upsert_attribution_edge(&clip.id, &edge, now);
            }
        }
        self.edges.sort_by(|a, b| {
            a.child_id
                .cmp(&b.child_id)
                .then(a.ordinal.cmp(&b.ordinal))
                .then(a.parent_id.cmp(&b.parent_id))
                .then(a.edge_type.cmp(&b.edge_type))
                .then(a.role.cmp(&b.role))
        });
        self.rebuild_edge_index();

        for (child_id, info) in &resolution.roots {
            self.upsert_cache(child_id, info, now);
        }
        self.refresh_eligible_roots();
    }

    /// The persisted `child_id -> parent_id` map from the active primary edges
    /// (each clip's ordinal-0 lineage parent), for seeding
    /// [`resolve_roots`](crate::resolve_roots).
    ///
    /// This lets a resolution walk hop through an ancestor whose clip is absent
    /// this run (an intermediate remix, or one Suno has purged) using the link
    /// captured on an earlier run, instead of self-rooting. It is resolution
    /// input only: these ids are never download candidates.
    pub fn archived_parents(&self) -> HashMap<String, String> {
        self.edges
            .iter()
            .filter(|edge| {
                edge.role == EdgeRole::Primary
                    && edge.ordinal == 0
                    && edge.status == EdgeStatus::Active
            })
            .map(|edge| (edge.child_id.clone(), edge.parent_id.clone()))
            .collect()
    }

    /// Insert or refresh the node for `clip`. `first_seen_at` and `status` are
    /// set once on insert; everything else is refreshed to the latest sighting.
    fn upsert_node(&mut self, clip: &Clip, now: &str) {
        let node = self.nodes.entry(clip.id.clone()).or_insert_with(|| Node {
            first_seen_at: now.to_owned(),
            ..Node::default()
        });
        node.title = clip.title.clone();
        node.created_at = clip.created_at.clone();
        node.clip_type = clip.clip_type.clone();
        node.task = clip.task.clone();
        node.is_remix = clip.is_remix;
        node.is_trashed = clip.is_trashed;
        node.last_seen_at = now.to_owned();
    }

    /// Insert or refresh the edge from `child_id` to `edge.parent_id`, keyed by
    /// `(child_id, parent_id, edge_type, role, ordinal)`.
    fn upsert_edge(&mut self, child_id: &str, edge: &Edge, now: &str) {
        let edge_type = edge_type_slug(edge.edge_type);
        let key = EdgeKey::new(
            child_id,
            &edge.parent_id,
            edge_type,
            edge.role,
            edge.ordinal,
        );
        if let Some(&index) = self.edge_index.get(&key) {
            let existing = &mut self.edges[index];
            existing.source_field = edge.source_field.to_owned();
            existing.status = EdgeStatus::Active;
            existing.last_seen_at = now.to_owned();
        } else {
            self.edges.push(StoredEdge {
                child_id: child_id.to_owned(),
                parent_id: edge.parent_id.clone(),
                edge_type: edge_type.to_owned(),
                role: edge.role,
                source_field: edge.source_field.to_owned(),
                ordinal: edge.ordinal,
                status: EdgeStatus::Active,
                first_seen_at: now.to_owned(),
                last_seen_at: now.to_owned(),
            });
            self.edge_index.insert(key, self.edges.len() - 1);
        }
    }

    /// Insert or refresh an attribution edge from `clip_roots`, keyed like any
    /// edge by `(child_id, parent_id, edge_type, role, ordinal)`.
    ///
    /// The open attribution slug (normalised) is written DIRECTLY to
    /// `edge_type`, bypassing the closed-[`EdgeType`] slug path, so an unknown
    /// `clip_attribution_type` is preserved verbatim rather than forced into the
    /// structural enum.
    fn upsert_attribution_edge(&mut self, child_id: &str, edge: &AttributionEdge, now: &str) {
        let edge_type = normalise_slug(&edge.edge_slug);
        let key = EdgeKey::new(
            child_id,
            &edge.parent_id,
            &edge_type,
            edge.role,
            edge.ordinal,
        );
        if let Some(&index) = self.edge_index.get(&key) {
            let existing = &mut self.edges[index];
            existing.source_field = edge.source_field.to_owned();
            existing.status = EdgeStatus::Active;
            existing.last_seen_at = now.to_owned();
        } else {
            self.edges.push(StoredEdge {
                child_id: child_id.to_owned(),
                parent_id: edge.parent_id.clone(),
                edge_type,
                role: edge.role,
                source_field: edge.source_field.to_owned(),
                ordinal: edge.ordinal,
                status: EdgeStatus::Active,
                first_seen_at: now.to_owned(),
                last_seen_at: now.to_owned(),
            });
            self.edge_index.insert(key, self.edges.len() - 1);
        }
    }

    fn rebuild_edge_index(&mut self) {
        self.edge_index.clear();
        for (index, edge) in self.edges.iter().enumerate() {
            self.edge_index
                .entry(EdgeKey::from_stored(edge))
                .or_insert(index);
        }
    }

    /// Fold one clip's root resolution into the cache, monotonically.
    ///
    /// A [`Resolved`](ResolveStatus::Resolved) root always wins. A non-resolved
    /// outcome (external, unresolved, cycle) never overwrites an existing
    /// resolved root — a transient gap-fill miss must not downgrade a good
    /// album. Otherwise the last-known non-resolved status is recorded.
    fn upsert_cache(&mut self, child_id: &str, info: &RootInfo, now: &str) {
        if info.status != ResolveStatus::Resolved
            && self
                .resolution_cache
                .get(child_id)
                .is_some_and(|entry| entry.status == ResolveStatus::Resolved)
        {
            return;
        }
        self.resolution_cache.insert(
            child_id.to_owned(),
            CacheEntry {
                root_id: info.root_id.clone(),
                status: info.status,
                algorithm_version: 1,
                computed_at: now.to_owned(),
            },
        );
    }
}

/// The stable on-disk slug for an [`EdgeType`].
fn edge_type_slug(edge_type: EdgeType) -> &'static str {
    match edge_type {
        EdgeType::Cover => "cover",
        EdgeType::Remaster => "remaster",
        EdgeType::SpeedEdit => "speed_edit",
        EdgeType::Edit => "edit",
        EdgeType::Extend => "extend",
        EdgeType::SectionReplace => "section_replace",
        EdgeType::Stitch => "stitch",
        EdgeType::Derived => "derived",
        EdgeType::Uploaded => "uploaded",
    }
}

/// Normalise an open attribution slug to a stable lowercase, underscore-joined
/// form, e.g. `"Remix Cover"` -> `"remix_cover"`. An empty (or whitespace-only)
/// slug maps to `"attribution"` so an edge always carries a non-empty type.
pub(super) fn normalise_slug(slug: &str) -> String {
    let normalised = slug
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_")
        .to_lowercase();
    if normalised.is_empty() {
        "attribution".to_owned()
    } else {
        normalised
    }
}
