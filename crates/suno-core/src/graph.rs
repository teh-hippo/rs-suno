//! The durable lineage graph store: a relational archive of clips, their parent
//! edges, and cached root resolutions.
//!
//! This is a pure serde type with no IO of its own; the CLI persists it beside
//! the library (mirroring the manifest). The shape is deliberately relational —
//! separate `nodes`, `edges`, and `resolution_cache` collections rather than an
//! adjacency blob per clip — so it migrates cleanly to SQLite later. A root's
//! title is read from its node, never copied into every row where it would go
//! stale.
//!
//! [`LineageStore::update`] is the only mutator: given the clips seen this run
//! and their [`Resolution`], it upserts nodes and edges and refreshes the
//! resolution cache. The store takes the wall clock as a `now` string from the
//! caller so it stays free of IO. The cache is monotonic (HARDENING H3): a
//! resolved root is never downgraded by a later transient miss. Gap-filled
//! (often trashed) ancestors are persisted as nodes so lineage survives Suno's
//! ~30-day trash purge.

use std::collections::btree_map::Iter;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::lineage::{
    Edge, EdgeRole, EdgeType, LineageContext, Resolution, ResolveStatus, RootInfo,
    immediate_parent, lineage_edges,
};
use crate::model::Clip;

/// The whole lineage graph, kept relational for a clean SQLite migration.
///
/// `nodes` and `resolution_cache` are [`BTreeMap`]s and `edges` is sorted after
/// every [`update`](LineageStore::update), so serialisation is deterministic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LineageStore {
    /// On-disk schema version, so a future migration can branch on it.
    pub schema_version: u32,
    /// Every clip ever seen (including trashed ancestors), keyed by clip id.
    pub nodes: BTreeMap<String, Node>,
    /// Every observed parent link, as a flat relational list.
    pub edges: Vec<StoredEdge>,
    /// The last resolved (or last-known) root per clip, keyed by clip id.
    pub resolution_cache: BTreeMap<String, CacheEntry>,
}

impl Default for LineageStore {
    fn default() -> Self {
        Self {
            schema_version: 1,
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            resolution_cache: BTreeMap::new(),
        }
    }
}

/// One clip in the graph. Mirrors the fields lineage needs to survive a purge:
/// enough to name and date the clip long after Suno deletes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Node {
    pub title: String,
    pub created_at: String,
    pub clip_type: String,
    pub task: String,
    pub is_remix: bool,
    pub is_trashed: bool,
    /// Lifecycle marker; `"observed"` for a clip seen from the feed or gap-fill.
    pub status: String,
    pub first_seen_at: String,
    pub last_seen_at: String,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            title: String::new(),
            created_at: String::new(),
            clip_type: String::new(),
            task: String::new(),
            is_remix: false,
            is_trashed: false,
            status: "observed".to_owned(),
            first_seen_at: String::new(),
            last_seen_at: String::new(),
        }
    }
}

/// One parent link, keyed (for upsert) by `(child_id, parent_id, edge_type,
/// role, ordinal)`. A flat row, not nested under its child, so it maps directly
/// to a `lineage_edges` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StoredEdge {
    pub child_id: String,
    pub parent_id: String,
    /// Stable lowercase slug, e.g. `"cover"`, `"remaster"`, `"section_replace"`.
    pub edge_type: String,
    /// `"primary"` for the rooting parent, `"secondary"` for extra sources.
    pub role: String,
    /// The clip field the parent id was read from, e.g. `"cover_clip_id"`.
    pub source_field: String,
    /// Position within its role (0 for the primary, then secondaries in order).
    pub ordinal: u32,
    /// Lifecycle marker; `"active"` for an edge observed this run.
    pub status: String,
    pub first_seen_at: String,
    pub last_seen_at: String,
}

impl Default for StoredEdge {
    fn default() -> Self {
        Self {
            child_id: String::new(),
            parent_id: String::new(),
            edge_type: String::new(),
            role: String::new(),
            source_field: String::new(),
            ordinal: 0,
            status: "active".to_owned(),
            first_seen_at: String::new(),
            last_seen_at: String::new(),
        }
    }
}

/// A cached root resolution for one clip: the O(1) album lookup, kept monotonic.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheEntry {
    pub root_id: String,
    /// `"resolved"`, or a slug of the terminal status (`"external"`, …).
    pub status: String,
    pub algorithm_version: u32,
    pub computed_at: String,
}

impl LineageStore {
    /// Create an empty store at the current schema version.
    pub fn new() -> Self {
        Self::default()
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
    /// better answer) and the root title from that root's archived node, so a
    /// transient miss keeps the last-known-good album even for a since-purged
    /// ancestor. The parent edge is read structurally from the clip itself.
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
        let (parent_id, edge_type) = match immediate_parent(clip) {
            Some((id, edge)) => (id, Some(edge)),
            None => (String::new(), None),
        };
        let status = cached
            .map(|entry| status_from_slug(&entry.status))
            .unwrap_or(ResolveStatus::Resolved);
        LineageContext {
            root_id,
            root_title,
            parent_id,
            edge_type,
            status,
        }
    }

    /// The set of root titles shared by more than one distinct root.
    ///
    /// Two distinct roots must never share an album folder (two different
    /// uploads titled "Break Through" exist), so naming appends the short root
    /// id to the album of any clip whose root title is in this set. It is
    /// computed from the whole archive — every distinct root in the resolution
    /// cache paired with its node title — so the decision is stable across runs
    /// and independent of the current batch: a `--since`/`--limit` slice that
    /// shows only one of two same-titled roots still disambiguates, instead of
    /// oscillating between a bare and a suffixed folder.
    pub fn colliding_root_titles(&self) -> BTreeSet<String> {
        let mut roots_by_title: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for entry in self.resolution_cache.values() {
            if entry.root_id.is_empty() {
                continue;
            }
            let Some(node) = self.nodes.get(&entry.root_id) else {
                continue;
            };
            let title = node.title.trim();
            if title.is_empty() {
                continue;
            }
            roots_by_title
                .entry(title.to_owned())
                .or_default()
                .insert(entry.root_id.clone());
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
        for clip in clips {
            self.upsert_node(clip, now);
        }
        // Gap-filled ancestors are not download candidates, but their lineage
        // must be archived before Suno purges them, so they become nodes too.
        for clip in &resolution.gap_filled {
            self.upsert_node(clip, now);
        }

        for clip in clips {
            for edge in lineage_edges(clip) {
                self.upsert_edge(&clip.id, &edge, now);
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

        for (child_id, info) in &resolution.roots {
            self.upsert_cache(child_id, info, now);
        }
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
        let role = edge_role_slug(edge.role);
        if let Some(existing) = self.edges.iter_mut().find(|stored| {
            stored.child_id == child_id
                && stored.parent_id == edge.parent_id
                && stored.edge_type == edge_type
                && stored.role == role
                && stored.ordinal == edge.ordinal
        }) {
            existing.source_field = edge.source_field.to_owned();
            existing.status = "active".to_owned();
            existing.last_seen_at = now.to_owned();
        } else {
            self.edges.push(StoredEdge {
                child_id: child_id.to_owned(),
                parent_id: edge.parent_id.clone(),
                edge_type: edge_type.to_owned(),
                role: role.to_owned(),
                source_field: edge.source_field.to_owned(),
                ordinal: edge.ordinal,
                status: "active".to_owned(),
                first_seen_at: now.to_owned(),
                last_seen_at: now.to_owned(),
            });
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
                .is_some_and(|entry| entry.status == "resolved")
        {
            return;
        }
        self.resolution_cache.insert(
            child_id.to_owned(),
            CacheEntry {
                root_id: info.root_id.clone(),
                status: resolve_status_slug(info.status).to_owned(),
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

/// The stable on-disk slug for an [`EdgeRole`].
fn edge_role_slug(role: EdgeRole) -> &'static str {
    match role {
        EdgeRole::Primary => "primary",
        EdgeRole::Secondary => "secondary",
    }
}

/// The stable on-disk slug for a [`ResolveStatus`].
fn resolve_status_slug(status: ResolveStatus) -> &'static str {
    match status {
        ResolveStatus::Resolved => "resolved",
        ResolveStatus::External => "external",
        ResolveStatus::Unresolved => "unresolved",
        ResolveStatus::Cycle => "cycle",
    }
}

/// Parse a cached status slug back into a [`ResolveStatus`], defaulting to
/// [`Resolved`](ResolveStatus::Resolved) for the self-root/unknown case.
fn status_from_slug(slug: &str) -> ResolveStatus {
    match slug {
        "external" => ResolveStatus::External,
        "unresolved" => ResolveStatus::Unresolved,
        "cycle" => ResolveStatus::Cycle,
        _ => ResolveStatus::Resolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A clean three-clip chain: cover -> remaster -> gen root, all present.
    fn chain_clips() -> Vec<Clip> {
        vec![
            Clip {
                id: "c".into(),
                title: "Cover".into(),
                clip_type: "gen".into(),
                task: "cover".into(),
                created_at: "t2".into(),
                cover_clip_id: "b".into(),
                edited_clip_id: "b".into(),
                ..Default::default()
            },
            Clip {
                id: "b".into(),
                title: "Remaster".into(),
                clip_type: "upsample".into(),
                task: "upsample".into(),
                created_at: "t1".into(),
                upsample_clip_id: "a".into(),
                edited_clip_id: "a".into(),
                ..Default::default()
            },
            Clip {
                id: "a".into(),
                title: "Root".into(),
                clip_type: "gen".into(),
                created_at: "t0".into(),
                ..Default::default()
            },
        ]
    }

    /// The matching resolution: every clip roots at `a`, all resolved.
    fn chain_resolution() -> Resolution {
        let mut roots = HashMap::new();
        for id in ["a", "b", "c"] {
            roots.insert(
                id.to_owned(),
                RootInfo {
                    root_id: "a".into(),
                    root_title: "Root".into(),
                    status: ResolveStatus::Resolved,
                },
            );
        }
        Resolution {
            roots,
            gap_filled: Vec::new(),
        }
    }

    fn edge<'a>(store: &'a LineageStore, child: &str, parent: &str) -> &'a StoredEdge {
        store
            .edges
            .iter()
            .find(|e| e.child_id == child && e.parent_id == parent)
            .expect("edge should exist")
    }

    #[test]
    fn new_store_is_empty_and_versioned() {
        let store = LineageStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert_eq!(store.schema_version, 1);
    }

    #[test]
    fn update_populates_nodes_edges_and_cache() {
        let mut store = LineageStore::new();
        store.update(&chain_clips(), &chain_resolution(), "now");

        // A node per clip, dated and typed from the clip.
        assert_eq!(store.len(), 3);
        let cover = store.node("c").unwrap();
        assert_eq!(cover.title, "Cover");
        assert_eq!(cover.clip_type, "gen");
        assert_eq!(cover.task, "cover");
        assert_eq!(cover.created_at, "t2");
        assert_eq!(cover.status, "observed");
        assert!(!cover.is_trashed);
        assert_eq!(cover.first_seen_at, "now");
        assert_eq!(cover.last_seen_at, "now");

        // One primary edge per non-root clip; the root emits none.
        assert_eq!(store.edges.len(), 2);
        let cb = edge(&store, "c", "b");
        assert_eq!(cb.edge_type, "cover");
        assert_eq!(cb.role, "primary");
        assert_eq!(cb.ordinal, 0);
        assert_eq!(cb.source_field, "cover_clip_id");
        assert_eq!(cb.status, "active");
        let ba = edge(&store, "b", "a");
        assert_eq!(ba.edge_type, "remaster");
        assert!(!store.edges.iter().any(|e| e.child_id == "a"));

        // The cache roots every clip at `a`, resolved.
        for id in ["a", "b", "c"] {
            let cached = store.get_root(id).unwrap();
            assert_eq!(cached.root_id, "a");
            assert_eq!(cached.status, "resolved");
            assert_eq!(cached.algorithm_version, 1);
        }
    }

    #[test]
    fn serde_roundtrip_preserves_a_relational_shape() {
        let mut store = LineageStore::new();
        store.update(&chain_clips(), &chain_resolution(), "now");

        let json = serde_json::to_string(&store).unwrap();
        let back: LineageStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store, back);

        let value: serde_json::Value = serde_json::to_value(&store).unwrap();
        assert_eq!(value.get("schema_version").unwrap(), 1);
        assert!(value.get("nodes").unwrap().is_object());
        assert!(value.get("edges").unwrap().is_array());
        assert!(value.get("resolution_cache").unwrap().is_object());

        // Relational, not adjacency: a node carries no edges/parent of its own,
        // and an edge is a flat row keyed by child and parent.
        let node = value.get("nodes").unwrap().get("c").unwrap();
        assert!(node.get("edges").is_none());
        assert!(node.get("parent_id").is_none());
        let first_edge = value.get("edges").unwrap().get(0).unwrap();
        assert!(first_edge.get("child_id").is_some());
        assert!(first_edge.get("parent_id").is_some());
    }

    #[test]
    fn update_is_idempotent_bar_last_seen() {
        let clips = chain_clips();
        let resolution = chain_resolution();
        let mut store = LineageStore::new();
        store.update(&clips, &resolution, "first");
        let node_ids: Vec<String> = store.iter().map(|(id, _)| id.clone()).collect();
        let edge_count = store.edges.len();

        store.update(&clips, &resolution, "second");

        // No new nodes, edges, or cache rows: the second run only refreshes.
        assert_eq!(
            store.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            node_ids
        );
        assert_eq!(store.edges.len(), edge_count, "edges must not duplicate");
        assert_eq!(store.resolution_cache.len(), 3);

        // first_seen_at sticks; last_seen_at advances.
        let cover = store.node("c").unwrap();
        assert_eq!(cover.first_seen_at, "first");
        assert_eq!(cover.last_seen_at, "second");
        let cb = edge(&store, "c", "b");
        assert_eq!(cb.first_seen_at, "first");
        assert_eq!(cb.last_seen_at, "second");
        // Root ids are stable across the re-run.
        assert_eq!(store.get_root("c").unwrap().root_id, "a");
    }

    #[test]
    fn cache_is_monotonic_and_never_downgrades_a_resolved_root() {
        let mut store = LineageStore::new();
        store.update(&chain_clips(), &chain_resolution(), "first");
        assert_eq!(store.get_root("c").unwrap().status, "resolved");

        // A later run where `c` fails to resolve (a transient gap-fill miss)
        // and a brand-new clip `d` that only reaches an external boundary.
        let child = Clip {
            id: "c".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "b".into(),
            edited_clip_id: "b".into(),
            ..Default::default()
        };
        let mut roots = HashMap::new();
        roots.insert(
            "c".to_owned(),
            RootInfo {
                root_id: "elsewhere".into(),
                root_title: String::new(),
                status: ResolveStatus::External,
            },
        );
        roots.insert(
            "d".to_owned(),
            RootInfo {
                root_id: "boundary".into(),
                root_title: String::new(),
                status: ResolveStatus::External,
            },
        );
        let resolution = Resolution {
            roots,
            gap_filled: Vec::new(),
        };
        store.update(&[child], &resolution, "second");

        // The resolved root of `c` is kept, not downgraded.
        let cached = store.get_root("c").unwrap();
        assert_eq!(cached.root_id, "a");
        assert_eq!(cached.status, "resolved");
        assert_eq!(cached.computed_at, "first");
        // A never-resolved clip records its last-known non-resolved status.
        let d = store.get_root("d").unwrap();
        assert_eq!(d.root_id, "boundary");
        assert_eq!(d.status, "external");
    }

    #[test]
    fn gap_filled_trashed_ancestor_is_a_durable_node() {
        // The trashed ancestor is not among `clips`; it arrives only via the
        // resolution's gap_filled set, yet must be archived as a node so its
        // lineage survives Suno's purge (HARDENING H4 / L2).
        let child = Clip {
            id: "c".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "t".into(),
            edited_clip_id: "t".into(),
            ..Default::default()
        };
        let trashed = Clip {
            id: "t".into(),
            title: "Trashed Original".into(),
            clip_type: "gen".into(),
            is_trashed: true,
            ..Default::default()
        };
        let mut roots = HashMap::new();
        roots.insert(
            "c".to_owned(),
            RootInfo {
                root_id: "t".into(),
                root_title: "Trashed Original".into(),
                status: ResolveStatus::Resolved,
            },
        );
        let resolution = Resolution {
            roots,
            gap_filled: vec![trashed],
        };
        store_update_and_assert_trashed(child, resolution);
    }

    fn store_update_and_assert_trashed(child: Clip, resolution: Resolution) {
        let mut store = LineageStore::new();
        store.update(&[child], &resolution, "now");

        let node = store
            .node("t")
            .expect("trashed ancestor should be archived");
        assert!(node.is_trashed);
        assert_eq!(node.title, "Trashed Original");
        // The child roots at the trashed ancestor.
        assert_eq!(store.get_root("c").unwrap().root_id, "t");
    }

    #[test]
    fn partial_json_loads_with_defaults() {
        // An older/partial file missing whole collections and per-row fields
        // still loads: container and row defaults fill the gaps.
        let json = r#"{"nodes":{"x":{"title":"Kept"}},"edges":[{"child_id":"x","parent_id":"y"}]}"#;
        let store: LineageStore = serde_json::from_str(json).unwrap();
        assert_eq!(store.schema_version, 1);
        let node = store.node("x").unwrap();
        assert_eq!(node.title, "Kept");
        assert_eq!(node.status, "observed");
        assert_eq!(store.edges[0].status, "active");
        assert!(store.resolution_cache.is_empty());
    }

    #[test]
    fn context_for_roots_a_remix_at_its_stored_ancestor() {
        let mut store = LineageStore::new();
        store.update(&chain_clips(), &chain_resolution(), "now");

        let child = &chain_clips()[0]; // "c", a cover of "b"
        let ctx = store.context_for(child);
        assert_eq!(ctx.root_id, "a");
        assert_eq!(ctx.root_title, "Root");
        assert_eq!(ctx.parent_id, "b");
        assert_eq!(ctx.edge_type, Some(EdgeType::Cover));
        assert_eq!(ctx.status, ResolveStatus::Resolved);
        // The remix folders under its resolved root's album.
        assert_eq!(ctx.album("Cover"), "Root");
    }

    #[test]
    fn context_for_a_root_uses_its_own_title_and_has_no_parent() {
        let mut store = LineageStore::new();
        store.update(&chain_clips(), &chain_resolution(), "now");

        let root = &chain_clips()[2]; // "a"
        let ctx = store.context_for(root);
        assert_eq!(ctx.root_id, "a");
        assert_eq!(ctx.root_title, "Root");
        assert_eq!(ctx.parent_id, "");
        assert_eq!(ctx.edge_type, None);
        assert_eq!(ctx.album("Root"), "Root");
    }

    #[test]
    fn context_for_an_unknown_clip_is_self_rooted() {
        let store = LineageStore::new();
        let orphan = Clip {
            id: "z".into(),
            title: "Lonely".into(),
            ..Default::default()
        };
        let ctx = store.context_for(&orphan);
        assert_eq!(ctx.root_id, "z");
        assert_eq!(ctx.root_title, "Lonely");
        assert_eq!(ctx.parent_id, "");
        assert_eq!(ctx.status, ResolveStatus::Resolved);
    }

    #[test]
    fn context_for_retains_a_purged_ancestor_album() {
        // The trashed ancestor arrives only via gap_filled, yet a later run
        // whose resolver failed (modelled here by simply not re-updating) must
        // still root the child at the archived ancestor with its stored title
        // (HARDENING H3).
        let child = Clip {
            id: "c".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "t".into(),
            edited_clip_id: "t".into(),
            ..Default::default()
        };
        let trashed = Clip {
            id: "t".into(),
            title: "Trashed Original".into(),
            clip_type: "gen".into(),
            is_trashed: true,
            ..Default::default()
        };
        let mut roots = HashMap::new();
        roots.insert(
            "c".to_owned(),
            RootInfo {
                root_id: "t".into(),
                root_title: "Trashed Original".into(),
                status: ResolveStatus::Resolved,
            },
        );
        let resolution = Resolution {
            roots,
            gap_filled: vec![trashed],
        };
        let mut store = LineageStore::new();
        store.update(std::slice::from_ref(&child), &resolution, "now");

        let ctx = store.context_for(&child);
        assert_eq!(ctx.root_id, "t");
        assert_eq!(ctx.root_title, "Trashed Original");
        assert_eq!(ctx.album("Cover"), "Trashed Original");
    }

    #[test]
    fn colliding_root_titles_flags_only_shared_distinct_roots() {
        // Two distinct roots share the title "Break Through"; a third root is
        // unique; a child of a shared root does not add a spurious distinct root.
        let clips = vec![
            Clip {
                id: "r1".into(),
                title: "Break Through".into(),
                clip_type: "gen".into(),
                ..Default::default()
            },
            Clip {
                id: "r2".into(),
                title: "Break Through".into(),
                clip_type: "gen".into(),
                ..Default::default()
            },
            Clip {
                id: "r3".into(),
                title: "Solo".into(),
                clip_type: "gen".into(),
                ..Default::default()
            },
            Clip {
                id: "c1".into(),
                title: "Break Through".into(),
                clip_type: "gen".into(),
                task: "cover".into(),
                cover_clip_id: "r1".into(),
                edited_clip_id: "r1".into(),
                ..Default::default()
            },
        ];
        let mut roots = HashMap::new();
        for (id, root) in [("r1", "r1"), ("r2", "r2"), ("r3", "r3"), ("c1", "r1")] {
            let title = if root == "r3" {
                "Solo"
            } else {
                "Break Through"
            };
            roots.insert(
                id.to_owned(),
                RootInfo {
                    root_id: root.into(),
                    root_title: title.into(),
                    status: ResolveStatus::Resolved,
                },
            );
        }
        let resolution = Resolution {
            roots,
            gap_filled: Vec::new(),
        };
        let mut store = LineageStore::new();
        store.update(&clips, &resolution, "now");

        let colliding = store.colliding_root_titles();
        assert!(colliding.contains("Break Through"));
        assert!(!colliding.contains("Solo"));
        assert_eq!(colliding.len(), 1);
    }
}
