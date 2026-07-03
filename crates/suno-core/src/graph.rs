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
use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

use crate::lineage::{
    Edge, EdgeRole, EdgeType, LineageContext, Resolution, ResolveStatus, RootInfo,
    immediate_parent, lineage_edges,
};
use crate::manifest::ArtifactState;
use crate::model::Clip;
use crate::reconcile::ArtifactKind;

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
    pub nodes: BTreeMap<String, Node>,
    /// Every observed parent link, as a flat relational list.
    pub edges: Vec<StoredEdge>,
    /// The last resolved (or last-known) root per clip, keyed by clip id.
    pub resolution_cache: BTreeMap<String, CacheEntry>,
    /// The reconciled folder-art state per album, keyed by the album's stable
    /// root id (HARDENING H2). Additive: absent in older stores, defaults empty.
    pub albums: BTreeMap<String, AlbumArt>,
    /// The reconciled `.m3u8` state per playlist, keyed by the playlist's Suno
    /// id (the synthetic `"liked"` id for the liked feed). Additive: absent in
    /// older stores, defaults empty.
    pub playlists: BTreeMap<String, PlaylistState>,
    /// The Suno account this library is pinned to (trust-on-first-use). Absent
    /// in older stores and in a fresh library until the first run adopts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Owner>,
    /// Manual album-name overrides, keyed by lineage root id, layered over the
    /// store each run from config (see [`set_album_overrides`]). Runtime-only:
    /// it is never serialised, so it can never persist into the durable graph or
    /// silently outlive its config entry.
    ///
    /// [`set_album_overrides`]: LineageStore::set_album_overrides
    #[serde(skip)]
    pub album_overrides: BTreeMap<String, String>,
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

/// The identity guard pins a library to the account it is first synced against
/// and refuses to run it against a different account, so a mistyped or swapped
/// token can never make one account's clips look absent from source and delete
/// another account's files. `user_id` is the stable identity; `display_name`
/// is cosmetic (for messages) and refreshed opportunistically on a match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub user_id: String,
    pub display_name: String,
}

/// The verdict of comparing an authenticated account against a library's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerCheck {
    /// The library is not pinned yet, so it can be adopted (trust-on-first-use).
    FirstUse,
    /// The authenticated account owns this library.
    Match,
    /// The authenticated account differs from the pinned owner.
    Mismatch,
}

/// The PHASE 1 identity verdict: whether an authenticated account may run
/// against a library, computed with no network (see [`owner_gate`]).
///
/// This is the composition that gates deletion, kept pure so the full matrix
/// (including the lock-in cases where a configured id or the owner pin refuses
/// even when `--allow-account-change` is set) is unit-tested here rather than
/// inline in the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerGate {
    /// A configured `account_id` differs from the authenticated id: always
    /// refuse, regardless of `--allow-account-change`.
    AbortConfigMismatch,
    /// The pinned owner differs and re-pinning was not permitted: refuse.
    AbortMismatch,
    /// The pinned owner differs but re-pinning was permitted: pin the new owner
    /// and run additively (no deletions this invocation).
    Repin,
    /// The authenticated account owns this library: proceed (the caller then
    /// refreshes the pinned display name).
    Proceed,
    /// The library is not pinned yet: defer to the PHASE 2 adoption decision.
    FirstUse,
}

impl OwnerGate {
    /// Whether this outcome forces an additive (no-deletion) run.
    pub fn is_additive(self) -> bool {
        matches!(self, OwnerGate::Repin)
    }
}

/// Decide whether an authenticated account may run against a library (PHASE 1).
///
/// A configured `account_id` that differs always aborts, even with
/// `allow_change` set, because it is an explicit operator assertion. Otherwise
/// an unpinned library defers to first-use adoption, a matching owner proceeds,
/// and a differing owner either re-pins (when `allow_change`) or aborts.
pub fn owner_gate(
    store_owner: Option<&Owner>,
    configured_id: Option<&str>,
    authed_user_id: &str,
    allow_change: bool,
) -> OwnerGate {
    if let Some(configured) = configured_id
        && configured != authed_user_id
    {
        return OwnerGate::AbortConfigMismatch;
    }
    match store_owner {
        None => OwnerGate::FirstUse,
        Some(owner) if owner.user_id == authed_user_id => OwnerGate::Proceed,
        Some(_) if allow_change => OwnerGate::Repin,
        Some(_) => OwnerGate::AbortMismatch,
    }
}

/// The PHASE 2 first-use adoption decision for a not-yet-pinned library.
///
/// Computed by [`adopt_decision`] from the account's listed clip ids, the
/// library's already-owned clip ids, whether the listing is complete, and
/// whether `--allow-account-change` was passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptDecision {
    /// The destination holds no clips yet: pin it as a fresh library (normal
    /// mode; a fresh library has nothing to delete).
    PinFresh,
    /// A complete listing overlaps the existing library: same account, pin it
    /// (normal mode).
    PinAdopt,
    /// A complete listing shares nothing with the existing library but
    /// `--allow-account-change` was passed: adopt it and run additively.
    AdoptForced,
    /// A complete listing shares nothing with the existing library and no
    /// override was passed: refuse.
    Abort,
    /// A narrowed (incomplete) listing cannot confirm identity: do not pin.
    SkipPin,
}

impl AdoptDecision {
    /// Whether this outcome forces an additive (no-deletion) run.
    pub fn is_additive(self) -> bool {
        matches!(self, AdoptDecision::AdoptForced)
    }
}

/// Decide how to adopt a not-yet-pinned library from this run's listing.
///
/// An empty library is adopted outright; otherwise identity is confirmed by an
/// overlap between the authenticated account's `listed` clip ids and the
/// library's `owned` clip ids, but only on a fully `enumerated` listing. A
/// complete listing with no overlap is a different (or wiped) account: it
/// refuses, unless `allow_change` opts into a forced additive adoption. A
/// narrowed listing (a `--limit`/`--since` run, where deletion is disabled
/// anyway) cannot confirm identity, so the library is left unpinned.
pub fn adopt_decision(
    listed: &[&str],
    owned: &BTreeSet<&str>,
    enumerated: bool,
    allow_change: bool,
) -> AdoptDecision {
    if owned.is_empty() {
        return AdoptDecision::PinFresh;
    }
    if !enumerated {
        return AdoptDecision::SkipPin;
    }
    if listed.iter().any(|id| owned.contains(id)) {
        AdoptDecision::PinAdopt
    } else if allow_change {
        AdoptDecision::AdoptForced
    } else {
        AdoptDecision::Abort
    }
}

/// The reconciled folder-art state for one album (one stable root id).
///
/// Folder art is album-scoped, not per-clip, so it lives here rather than on a
/// [`ManifestEntry`](crate::manifest::ManifestEntry). Each slot records the
/// sidecar's path and the content hash of the art it was rendered from, so a
/// later reconcile rewrites only on a genuine content change (HARDENING H1: a
/// most-played flip that yields the same art hash is a no-op). Kept relational
/// (two explicit slots) so it migrates cleanly to a SQLite `album_art` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlbumArt {
    /// The album's static `folder.jpg`, sourced from the most-played variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_jpg: Option<ArtifactState>,
    /// The album's animated `cover.webp`, from the first-created animated variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_webp: Option<ArtifactState>,
}

impl AlbumArt {
    /// The stored state for one folder-art `kind`, if present. Per-clip and
    /// library kinds have no album slot and map to `None`.
    pub fn artifact(&self, kind: ArtifactKind) -> Option<&ArtifactState> {
        match kind {
            ArtifactKind::FolderJpg => self.folder_jpg.as_ref(),
            ArtifactKind::FolderWebp => self.folder_webp.as_ref(),
            ArtifactKind::CoverJpg
            | ArtifactKind::CoverWebp
            | ArtifactKind::DetailsTxt
            | ArtifactKind::LyricsTxt
            | ArtifactKind::Lrc
            | ArtifactKind::VideoMp4
            | ArtifactKind::Playlist => None,
        }
    }

    /// Set (or clear, with `None`) the state for one folder-art `kind`.
    ///
    /// The executor calls this after a folder-art write (with the new state) or
    /// delete (with `None`), so the kind-to-slot mapping lives in one place.
    /// Non-album kinds have no slot here and are no-ops.
    pub fn set(&mut self, kind: ArtifactKind, state: Option<ArtifactState>) {
        match kind {
            ArtifactKind::FolderJpg => self.folder_jpg = state,
            ArtifactKind::FolderWebp => self.folder_webp = state,
            ArtifactKind::CoverJpg
            | ArtifactKind::CoverWebp
            | ArtifactKind::DetailsTxt
            | ArtifactKind::LyricsTxt
            | ArtifactKind::Lrc
            | ArtifactKind::VideoMp4
            | ArtifactKind::Playlist => {}
        }
    }

    /// True when the album holds no folder art at all (both slots empty), so the
    /// store can prune the now-dead album row.
    pub fn is_empty(&self) -> bool {
        self.folder_jpg.is_none() && self.folder_webp.is_none()
    }
}

/// The reconciled `.m3u8` state for one playlist.
///
/// A playlist's body is *generated*, not fetched, so unlike per-clip artifacts
/// its change detection is a single content hash over the full rendered text
/// (HARDENING B1: name, order, and every member's path/title/duration feed it).
/// The `path` is the sidecar's library-relative location, tracked so a rename
/// (a playlist renamed on Suno) is detected and the old file removed. Kept as a
/// flat row so it migrates cleanly to a SQLite `playlists` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistState {
    /// The playlist's display name at the time it was last written.
    pub name: String,
    /// The `.m3u8` file's library-relative path (`<sanitised name>.m3u8`).
    pub path: String,
    /// The content hash of the rendered `.m3u8` this row was written from.
    pub hash: String,
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

    /// The account this library is pinned to, if any.
    pub fn owner(&self) -> Option<&Owner> {
        self.owner.as_ref()
    }

    /// Compare an authenticated `user_id` against the pinned owner.
    pub fn owner_check(&self, user_id: &str) -> OwnerCheck {
        match &self.owner {
            None => OwnerCheck::FirstUse,
            Some(owner) if owner.user_id == user_id => OwnerCheck::Match,
            Some(_) => OwnerCheck::Mismatch,
        }
    }

    /// Pin this library to `owner`, replacing any prior pin.
    pub fn pin_owner(&mut self, owner: Owner) {
        self.owner = Some(owner);
    }

    /// Refresh the pinned owner's display name when it has changed, returning
    /// whether it changed. A no-op when the library is not pinned.
    pub fn refresh_display_name(&mut self, display_name: &str) -> bool {
        match &mut self.owner {
            Some(owner) if owner.display_name != display_name => {
                owner.display_name = display_name.to_owned();
                true
            }
            _ => false,
        }
    }

    /// The cached root resolution for `id`, if present.
    pub fn get_root(&self, id: &str) -> Option<&CacheEntry> {
        self.resolution_cache.get(id)
    }

    /// The reconciled folder-art state for the album rooted at `root_id`.
    pub fn album_art(&self, root_id: &str) -> Option<&AlbumArt> {
        self.albums.get(root_id)
    }

    /// Set (or clear, with `None`) one folder-art `kind` for the album rooted at
    /// `root_id`.
    ///
    /// A set upserts the album row; a clear that empties the row removes it, so
    /// the store never accumulates dead all-`None` album entries. This is the
    /// store-level counterpart the CLI persists after the executor mutates the
    /// [`albums`](Self::albums) map in place.
    pub fn set_album_artifact(
        &mut self,
        root_id: &str,
        kind: ArtifactKind,
        state: Option<ArtifactState>,
    ) {
        match state {
            Some(state) => self
                .albums
                .entry(root_id.to_owned())
                .or_default()
                .set(kind, Some(state)),
            None => {
                if let Some(art) = self.albums.get_mut(root_id) {
                    art.set(kind, None);
                    if art.is_empty() {
                        self.albums.remove(root_id);
                    }
                }
            }
        }
    }

    /// The reconciled `.m3u8` state for the playlist with `id`, if present.
    pub fn playlist(&self, id: &str) -> Option<&PlaylistState> {
        self.playlists.get(id)
    }

    /// Upsert (with `Some`) or remove (with `None`) the `.m3u8` state for the
    /// playlist `id`.
    ///
    /// This is the store-level counterpart the CLI persists after the executor
    /// mutates the [`playlists`](Self::playlists) map in place: a write records
    /// the new state; a delete clears the row so the store never keeps a
    /// dangling entry for a playlist whose file was removed.
    pub fn set_playlist(&mut self, id: &str, state: Option<PlaylistState>) {
        match state {
            Some(state) => {
                self.playlists.insert(id.to_owned(), state);
            }
            None => {
                self.playlists.remove(id);
            }
        }
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
            .map(|entry| status_from_slug(&entry.status))
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
        self.refresh_eligible_roots();
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
    fn album_for_id_matches_context_for_and_handles_unknown() {
        let mut store = LineageStore::new();
        store.update(&chain_clips(), &chain_resolution(), "now");

        // A child folds under its differently-titled root, agreeing with the
        // live-clip rule via context_for.
        assert_eq!(store.album_for_id("c"), "Root");
        let cover = &chain_clips()[0];
        assert_eq!(
            store.album_for_id("c"),
            store.context_for(cover).album(&cover.title)
        );
        // The root folders under its own title.
        assert_eq!(store.album_for_id("a"), "Root");
        // An id absent from the store folds to an empty own title.
        assert_eq!(store.album_for_id("missing"), "");
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
    fn album_overrides_are_runtime_only_and_never_persist() {
        // Overrides come from config each run, so they must not serialise into
        // the durable graph or survive a round-trip (they would then outlive the
        // config entry that set them).
        let mut store = LineageStore::new();
        store.update(&chain_clips(), &chain_resolution(), "now");
        store.set_album_overrides(
            [("a".to_owned(), "Preferred".to_owned())]
                .into_iter()
                .collect(),
        );

        let value: serde_json::Value = serde_json::to_value(&store).unwrap();
        assert!(value.get("album_overrides").is_none());

        let json = serde_json::to_string(&store).unwrap();
        let back: LineageStore = serde_json::from_str(&json).unwrap();
        assert!(back.album_overrides.is_empty());
        assert_eq!(back.album_for_id("c"), "Root");
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
        // The album-art collection is additive: a store written before folder
        // art existed loads with no albums and no folder art.
        assert!(store.albums.is_empty());
        assert!(store.album_art("x").is_none());
        // The playlist collection is likewise additive: absent in an older
        // store, it defaults empty (HARDENING B2: no stored playlist means no
        // reconcile ever treats one as stale).
        assert!(store.playlists.is_empty());
        assert!(store.playlist("x").is_none());
    }

    #[test]
    fn album_art_roundtrips_and_reads_by_kind() {
        let mut store = LineageStore::new();
        store.albums.insert(
            "root-1".to_owned(),
            AlbumArt {
                folder_jpg: Some(ArtifactState {
                    path: "alice/Album/folder.jpg".to_owned(),
                    hash: "jpg-h".to_owned(),
                }),
                folder_webp: Some(ArtifactState {
                    path: "alice/Album/cover.webp".to_owned(),
                    hash: "webp-h".to_owned(),
                }),
            },
        );

        let json = serde_json::to_string(&store).unwrap();
        let back: LineageStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store, back);

        // The serialised shape is a relational `albums` map keyed by root id.
        let value: serde_json::Value = serde_json::to_value(&store).unwrap();
        let album = value.get("albums").unwrap().get("root-1").unwrap();
        assert_eq!(
            album.get("folder_jpg").unwrap().get("hash").unwrap(),
            "jpg-h"
        );

        let art = back.album_art("root-1").unwrap();
        assert_eq!(
            art.artifact(ArtifactKind::FolderJpg).unwrap().path,
            "alice/Album/folder.jpg"
        );
        assert_eq!(
            art.artifact(ArtifactKind::FolderWebp).unwrap().hash,
            "webp-h"
        );
        // A per-clip kind has no album slot.
        assert!(art.artifact(ArtifactKind::CoverJpg).is_none());
    }

    #[test]
    fn empty_album_art_omits_slots_when_serialised() {
        // An all-`None` AlbumArt round-trips and writes an empty object, so the
        // absent-slot default holds both ways.
        let empty = AlbumArt::default();
        assert!(empty.is_empty());
        let value = serde_json::to_value(&empty).unwrap();
        assert!(value.get("folder_jpg").is_none());
        assert!(value.get("folder_webp").is_none());
        let back: AlbumArt = serde_json::from_str("{}").unwrap();
        assert_eq!(back, empty);
    }

    #[test]
    fn set_album_artifact_upserts_then_prunes_when_emptied() {
        let mut store = LineageStore::new();
        let jpg = ArtifactState {
            path: "a/folder.jpg".to_owned(),
            hash: "h1".to_owned(),
        };
        store.set_album_artifact("root-1", ArtifactKind::FolderJpg, Some(jpg.clone()));
        assert_eq!(store.album_art("root-1").unwrap().folder_jpg, Some(jpg));

        // Clearing the only slot prunes the whole album row (no dead entries).
        store.set_album_artifact("root-1", ArtifactKind::FolderJpg, None);
        assert!(store.album_art("root-1").is_none());
        assert!(store.albums.is_empty());
    }

    #[test]
    fn playlist_state_roundtrips_by_id() {
        let mut store = LineageStore::new();
        store.playlists.insert(
            "pl1".to_owned(),
            PlaylistState {
                name: "Road Trip".to_owned(),
                path: "Road Trip.m3u8".to_owned(),
                hash: "abc123".to_owned(),
            },
        );

        let json = serde_json::to_string(&store).unwrap();
        let back: LineageStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store, back);

        // The serialised shape is a relational `playlists` map keyed by id.
        let value: serde_json::Value = serde_json::to_value(&store).unwrap();
        let pl = value.get("playlists").unwrap().get("pl1").unwrap();
        assert_eq!(pl.get("path").unwrap(), "Road Trip.m3u8");
        assert_eq!(pl.get("hash").unwrap(), "abc123");

        let stored = back.playlist("pl1").unwrap();
        assert_eq!(stored.name, "Road Trip");
        assert_eq!(stored.hash, "abc123");
    }

    #[test]
    fn set_playlist_upserts_then_clears() {
        let mut store = LineageStore::new();
        let state = PlaylistState {
            name: "Mix".to_owned(),
            path: "Mix.m3u8".to_owned(),
            hash: "h1".to_owned(),
        };
        store.set_playlist("pl1", Some(state.clone()));
        assert_eq!(store.playlist("pl1"), Some(&state));

        // A rewrite replaces the row in place.
        let renamed = PlaylistState {
            name: "Mix v2".to_owned(),
            path: "Mix v2.m3u8".to_owned(),
            hash: "h2".to_owned(),
        };
        store.set_playlist("pl1", Some(renamed.clone()));
        assert_eq!(store.playlist("pl1"), Some(&renamed));

        // Clearing removes the row so no dangling entry survives a delete.
        store.set_playlist("pl1", None);
        assert!(store.playlist("pl1").is_none());
        assert!(store.playlists.is_empty());
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
    fn context_for_tags_the_root_year_across_a_calendar_boundary() {
        // A December root with a January revision: both tag the root's year, so
        // the album groups under one year even across the boundary.
        let clips = vec![
            Clip {
                id: "child".into(),
                title: "Revision".into(),
                clip_type: "gen".into(),
                task: "cover".into(),
                created_at: "2024-01-02T08:00:00Z".into(),
                cover_clip_id: "root".into(),
                edited_clip_id: "root".into(),
                ..Default::default()
            },
            Clip {
                id: "root".into(),
                title: "Origin".into(),
                clip_type: "gen".into(),
                created_at: "2023-12-30T23:00:00Z".into(),
                ..Default::default()
            },
        ];
        let mut roots = HashMap::new();
        for id in ["child", "root"] {
            roots.insert(
                id.to_owned(),
                RootInfo {
                    root_id: "root".into(),
                    root_title: "Origin".into(),
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

        let child_ctx = store.context_for(&clips[0]);
        assert_eq!(child_ctx.root_id, "root");
        assert_eq!(child_ctx.root_date, "2023-12-30T23:00:00Z");
        // The January child tags the December root's year, not its own 2024.
        assert_eq!(child_ctx.year(&clips[0].created_at), "2023");

        // The root tags its own year (the same year).
        let root_ctx = store.context_for(&clips[1]);
        assert_eq!(root_ctx.year(&clips[1].created_at), "2023");
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

    /// Build the two-distinct-root store used by the disambiguation tests: `r1`
    /// and `r2` are separate `gen` roots titled `t1`/`t2`.
    fn two_root_store(t1: &str, t2: &str) -> LineageStore {
        let clips = vec![
            Clip {
                id: "r1".into(),
                title: t1.into(),
                clip_type: "gen".into(),
                ..Default::default()
            },
            Clip {
                id: "r2".into(),
                title: t2.into(),
                clip_type: "gen".into(),
                ..Default::default()
            },
        ];
        let mut roots = HashMap::new();
        roots.insert(
            "r1".to_owned(),
            RootInfo {
                root_id: "r1".into(),
                root_title: t1.into(),
                status: ResolveStatus::Resolved,
            },
        );
        roots.insert(
            "r2".to_owned(),
            RootInfo {
                root_id: "r2".into(),
                root_title: t2.into(),
                status: ResolveStatus::Resolved,
            },
        );
        let mut store = LineageStore::new();
        store.update(
            &clips,
            &Resolution {
                roots,
                gap_filled: Vec::new(),
            },
            "now",
        );
        store
    }

    #[test]
    fn album_override_flows_into_context_tag_hash_and_index() {
        // Override the lineage root's album name; every album-bearing surface
        // (the resolved context, the ALBUM tag, the change hash, and the
        // id-only index) must reflect the preferred name from one source.
        let clips = chain_clips();
        let mut store = LineageStore::new();
        store.update(&clips, &chain_resolution(), "now");

        let cover = &clips[0]; // "Cover", rooted at "a" ("Root")
        let before_hash = crate::hash::meta_hash(cover, &store.context_for(cover));

        store.set_album_overrides(
            [("a".to_owned(), "Preferred Name".to_owned())]
                .into_iter()
                .collect(),
        );

        // Every clip in the lineage now folders under the preferred album.
        for id in ["a", "b", "c"] {
            let clip = clips.iter().find(|c| c.id == id).unwrap();
            let ctx = store.context_for(clip);
            assert_eq!(ctx.album(&clip.title), "Preferred Name");
            assert_eq!(store.album_for_id(id), "Preferred Name");
        }

        // The ALBUM tag follows the override.
        let ctx = store.context_for(cover);
        let meta = crate::tag::TrackMetadata::from_clip(cover, &ctx);
        assert_eq!(meta.album, "Preferred Name");

        // The change hash shifts, so reconcile retags the file in place.
        let after_hash = crate::hash::meta_hash(cover, &ctx);
        assert_ne!(before_hash, after_hash);
    }

    #[test]
    fn empty_album_override_is_ignored() {
        // A blank value must never blank an album; the derived title stands.
        let clips = chain_clips();
        let mut store = LineageStore::new();
        store.update(&clips, &chain_resolution(), "now");
        store.set_album_overrides([("a".to_owned(), "   ".to_owned())].into_iter().collect());
        assert_eq!(store.album_for_id("c"), "Root");
    }

    #[test]
    fn album_override_creates_a_collision_that_disambiguates() {
        // Two uniquely titled roots collide once one is renamed onto the other.
        let mut store = two_root_store("Alpha", "Beta");
        assert!(store.colliding_root_titles().is_empty());

        store.set_album_overrides(
            [("r2".to_owned(), "Alpha".to_owned())]
                .into_iter()
                .collect(),
        );
        let colliding = store.colliding_root_titles();
        assert!(colliding.contains("Alpha"));
        assert_eq!(colliding.len(), 1);
    }

    #[test]
    fn album_override_resolves_a_natural_collision() {
        // Two roots share a title; renaming one apart settles the collision.
        let mut store = two_root_store("Break Through", "Break Through");
        assert!(store.colliding_root_titles().contains("Break Through"));

        store.set_album_overrides(
            [("r2".to_owned(), "Second Wind".to_owned())]
                .into_iter()
                .collect(),
        );
        assert!(store.colliding_root_titles().is_empty());
    }

    /// Insert a cache-only root: an entry in the resolution cache whose root_id
    /// has NO backing node (an external or not-yet-archived root). Such a root
    /// still folders under a configured override, so it must be visible to
    /// collision detection.
    fn insert_cache_only_root(store: &mut LineageStore, root_id: &str) {
        store.resolution_cache.insert(
            root_id.to_owned(),
            CacheEntry {
                root_id: root_id.to_owned(),
                status: "external".to_owned(),
                algorithm_version: 1,
                computed_at: "now".to_owned(),
            },
        );
        // Direct cache mutation bypasses `update`, so mirror what a real run does
        // after loading: refresh the derived eligible-root set.
        store.refresh_eligible_roots();
    }

    #[test]
    fn override_on_node_less_root_collides_with_a_real_root() {
        // A node-less (cache-only) root overridden onto a real root's title must
        // be flagged as colliding, so both albums get the [root_id8] suffix and
        // two distinct roots never share one folder.
        let mut store = LineageStore::new();
        store.update(
            std::slice::from_ref(&Clip {
                id: "realroot".into(),
                title: "Shared".into(),
                clip_type: "gen".into(),
                ..Default::default()
            }),
            &Resolution {
                roots: [(
                    "realroot".to_owned(),
                    RootInfo {
                        root_id: "realroot".into(),
                        root_title: "Shared".into(),
                        status: ResolveStatus::Resolved,
                    },
                )]
                .into_iter()
                .collect(),
                gap_filled: Vec::new(),
            },
            "now",
        );
        insert_cache_only_root(&mut store, "extroot");
        store.set_album_overrides(
            [("extroot".to_owned(), "Shared".to_owned())]
                .into_iter()
                .collect(),
        );

        let colliding = store.colliding_root_titles();
        assert!(
            colliding.contains("Shared"),
            "a node-less overridden root must still be seen by collision detection"
        );
    }

    #[test]
    fn two_node_less_roots_overridden_to_same_name_collide() {
        let mut store = LineageStore::new();
        insert_cache_only_root(&mut store, "extone");
        insert_cache_only_root(&mut store, "exttwo");
        store.set_album_overrides(
            [
                ("extone".to_owned(), "Shared".to_owned()),
                ("exttwo".to_owned(), "Shared".to_owned()),
            ]
            .into_iter()
            .collect(),
        );
        assert!(store.colliding_root_titles().contains("Shared"));
    }

    #[test]
    fn colliding_node_less_overrides_keep_album_art_paths_distinct() {
        // End-to-end guard: two node-less roots overridden to one name must not
        // collapse their album-art (folder.jpg) onto a single shared path. The
        // colliding set drives naming to append [root_id8], giving each root its
        // own album folder and so its own folder.jpg.
        let mut store = LineageStore::new();
        insert_cache_only_root(&mut store, "aaaaaaaa-root-one");
        insert_cache_only_root(&mut store, "bbbbbbbb-root-two");
        store.set_album_overrides(
            [
                ("aaaaaaaa-root-one".to_owned(), "Shared".to_owned()),
                ("bbbbbbbb-root-two".to_owned(), "Shared".to_owned()),
            ]
            .into_iter()
            .collect(),
        );
        let colliding = store.colliding_root_titles();

        let clip_of = |id: &str| Clip {
            id: id.to_owned(),
            title: "Track".to_owned(),
            display_name: "alice".to_owned(),
            image_large_url: "https://art.example/large.jpg".to_owned(),
            ..Default::default()
        };
        let ctx_of = |root_id: &str| LineageContext {
            root_id: root_id.to_owned(),
            root_title: "Shared".to_owned(),
            root_date: String::new(),
            parent_id: String::new(),
            edge_type: None,
            status: ResolveStatus::Resolved,
        };
        let clip_a = clip_of("clipaaaa-1111");
        let clip_b = clip_of("clipbbbb-2222");
        let ctx_a = ctx_of("aaaaaaaa-root-one");
        let ctx_b = ctx_of("bbbbbbbb-root-two");
        let requests = [
            crate::naming::NamingRequest {
                clip: &clip_a,
                lineage: &ctx_a,
            },
            crate::naming::NamingRequest {
                clip: &clip_b,
                lineage: &ctx_b,
            },
        ];
        let names = crate::naming::render_clip_names(
            &requests,
            &crate::naming::NamingConfig::default(),
            &colliding,
        );

        let desired_of = |clip: &Clip, ctx: &LineageContext, name: &crate::naming::RenderedName| {
            crate::reconcile::Desired {
                clip: clip.clone(),
                lineage: ctx.clone(),
                path: format!("{}.flac", name.relative_path.to_string_lossy()),
                format: crate::AudioFormat::Flac,
                meta_hash: String::new(),
                art_hash: String::new(),
                modes: vec![crate::reconcile::SourceMode::Mirror],
                trashed: false,
                private: false,
                artifacts: Vec::new(),
                stems: None,
            }
        };
        let desired = vec![
            desired_of(&clip_a, &ctx_a, &names[0]),
            desired_of(&clip_b, &ctx_b, &names[1]),
        ];

        let albums = crate::reconcile::album_desired(&desired, false);
        assert_eq!(albums.len(), 2, "each distinct root is its own album");
        let jpg_paths: Vec<String> = albums
            .iter()
            .filter_map(|a| a.folder_jpg.as_ref().map(|art| art.path.clone()))
            .collect();
        assert_eq!(jpg_paths.len(), 2, "both albums have a folder.jpg");
        assert_ne!(
            jpg_paths[0], jpg_paths[1],
            "colliding roots must not share one folder.jpg path"
        );
    }

    #[test]
    fn override_on_uncached_selected_root_is_ignored_and_keeps_albums_distinct() {
        // Residual-bug guard. On a resolution-FAILED run (no store.update), a
        // newly-listed clip is uncached, so context_for falls back to a
        // self-root (root_id = clip.id) that colliding_root_titles cannot see.
        // An override on that id must NOT apply this run, or the clip would
        // render/tag under a stored root's title with no [root_id8] suffix and
        // collapse two distinct albums onto one folder (and one folder.jpg).
        let mut store = LineageStore::new();
        store.update(
            std::slice::from_ref(&Clip {
                id: "realroot".into(),
                title: "Shared".into(),
                clip_type: "gen".into(),
                ..Default::default()
            }),
            &Resolution {
                roots: [(
                    "realroot".to_owned(),
                    RootInfo {
                        root_id: "realroot".into(),
                        root_title: "Shared".into(),
                        status: ResolveStatus::Resolved,
                    },
                )]
                .into_iter()
                .collect(),
                gap_filled: Vec::new(),
            },
            "now",
        );
        // A newly-listed clip that failed to resolve this run: it is NOT in the
        // cache, and config overrides its id onto the stored root's title.
        let new_clip = Clip {
            id: "newnewnew-9999".into(),
            title: "Solo Track".into(),
            display_name: "alice".into(),
            image_large_url: "https://art.example/large.jpg".into(),
            ..Default::default()
        };
        store.set_album_overrides(
            [("newnewnew-9999".to_owned(), "Shared".to_owned())]
                .into_iter()
                .collect(),
        );

        // The uncached clip folders under its OWN title, not the override.
        let new_ctx = store.context_for(&new_clip);
        assert_eq!(new_ctx.root_id, "newnewnew-9999");
        assert_eq!(new_ctx.album(&new_clip.title), "Solo Track");

        // Collision detection is unchanged: the stored root stands alone.
        assert!(store.colliding_root_titles().is_empty());

        // Album-art paths for the two distinct roots stay distinct.
        let real_clip = Clip {
            id: "realroot".into(),
            title: "Shared".into(),
            display_name: "alice".into(),
            image_large_url: "https://art.example/large.jpg".into(),
            ..Default::default()
        };
        let real_ctx = store.context_for(&real_clip);
        let colliding = store.colliding_root_titles();
        let requests = [
            crate::naming::NamingRequest {
                clip: &real_clip,
                lineage: &real_ctx,
            },
            crate::naming::NamingRequest {
                clip: &new_clip,
                lineage: &new_ctx,
            },
        ];
        let names = crate::naming::render_clip_names(
            &requests,
            &crate::naming::NamingConfig::default(),
            &colliding,
        );
        let desired_of = |clip: &Clip, ctx: &LineageContext, name: &crate::naming::RenderedName| {
            crate::reconcile::Desired {
                clip: clip.clone(),
                lineage: ctx.clone(),
                path: format!("{}.flac", name.relative_path.to_string_lossy()),
                format: crate::AudioFormat::Flac,
                meta_hash: String::new(),
                art_hash: String::new(),
                modes: vec![crate::reconcile::SourceMode::Mirror],
                trashed: false,
                private: false,
                artifacts: Vec::new(),
                stems: None,
            }
        };
        let desired = vec![
            desired_of(&real_clip, &real_ctx, &names[0]),
            desired_of(&new_clip, &new_ctx, &names[1]),
        ];
        let albums = crate::reconcile::album_desired(&desired, false);
        let jpg_paths: Vec<String> = albums
            .iter()
            .filter_map(|a| a.folder_jpg.as_ref().map(|art| art.path.clone()))
            .collect();
        assert_eq!(jpg_paths.len(), 2, "both albums have a folder.jpg");
        assert_ne!(
            jpg_paths[0], jpg_paths[1],
            "an uncached override must not collapse two albums onto one path"
        );
    }

    #[test]
    fn override_on_gap_filled_root_applies_to_children_and_collides() {
        // Round-3 regression. A gap-filled/archived root is a cache VALUE for its
        // children but never a cache KEY (see
        // resolve_roots_returns_gap_filled_ancestors_for_archival). An override
        // keyed by such a root must still apply to its children AND participate
        // in collision detection, so gating override-application on the cache
        // VALUE set (not the key set) is essential.
        let child = Clip {
            id: "childclip".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "gaproot".into(),
            edited_clip_id: "gaproot".into(),
            ..Default::default()
        };
        let other_root = Clip {
            id: "otherroot".into(),
            title: "Preferred".into(),
            clip_type: "gen".into(),
            ..Default::default()
        };
        let gap_ancestor = Clip {
            id: "gaproot".into(),
            title: "Working Title".into(),
            clip_type: "gen".into(),
            ..Default::default()
        };
        let mut roots = HashMap::new();
        roots.insert(
            "childclip".to_owned(),
            RootInfo {
                root_id: "gaproot".into(),
                root_title: "Working Title".into(),
                status: ResolveStatus::Resolved,
            },
        );
        roots.insert(
            "otherroot".to_owned(),
            RootInfo {
                root_id: "otherroot".into(),
                root_title: "Preferred".into(),
                status: ResolveStatus::Resolved,
            },
        );
        let mut store = LineageStore::new();
        store.update(
            &[child.clone(), other_root],
            &Resolution {
                roots,
                gap_filled: vec![gap_ancestor],
            },
            "now",
        );
        // "gaproot" is a node and a cache value, but NOT a cache key.
        assert!(store.node("gaproot").is_some());
        assert!(!store.resolution_cache.contains_key("gaproot"));

        store.set_album_overrides(
            [("gaproot".to_owned(), "Preferred".to_owned())]
                .into_iter()
                .collect(),
        );

        // The override on the gap-filled root reaches its child (would be
        // ignored under a cache-KEY gate).
        assert_eq!(store.context_for(&child).album(&child.title), "Preferred");
        assert_eq!(store.album_for_id("childclip"), "Preferred");

        // And it participates in collision detection: two distinct roots now
        // resolve to "Preferred", so it is flagged.
        assert!(store.colliding_root_titles().contains("Preferred"));
    }

    #[test]
    fn eligible_root_set_is_exactly_the_cache_value_domain() {
        // Tie-together guard: the set effective_root_title gates overrides on is
        // literally the set colliding_root_titles groups over (both read
        // eligible_root_ids), and refresh_eligible_roots computes it as the
        // non-empty root_id VALUES of the cache. If these drift, an override
        // could apply where a collision is invisible (or vice versa).
        let child = Clip {
            id: "childclip".into(),
            title: "Cover".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "gaproot".into(),
            edited_clip_id: "gaproot".into(),
            ..Default::default()
        };
        let mut roots = HashMap::new();
        roots.insert(
            "childclip".to_owned(),
            RootInfo {
                root_id: "gaproot".into(),
                root_title: "Working Title".into(),
                status: ResolveStatus::Resolved,
            },
        );
        let mut store = LineageStore::new();
        store.update(
            std::slice::from_ref(&child),
            &Resolution {
                roots,
                gap_filled: vec![Clip {
                    id: "gaproot".into(),
                    title: "Working Title".into(),
                    clip_type: "gen".into(),
                    ..Default::default()
                }],
            },
            "now",
        );

        let expected: std::collections::HashSet<String> = store
            .resolution_cache
            .values()
            .map(|entry| entry.root_id.clone())
            .filter(|root_id| !root_id.is_empty())
            .collect();
        assert_eq!(*store.eligible_root_ids_for_test(), expected);
        // The gap-filled root is in the domain (a value), not because it is a key.
        assert!(store.eligible_root_ids_for_test().contains("gaproot"));
        assert!(!store.resolution_cache.contains_key("gaproot"));
    }

    fn owner(id: &str, name: &str) -> Owner {
        Owner {
            user_id: id.to_owned(),
            display_name: name.to_owned(),
        }
    }

    #[test]
    fn owner_check_covers_first_use_match_and_mismatch() {
        let mut store = LineageStore::new();
        assert_eq!(store.owner_check("user_a"), OwnerCheck::FirstUse);

        store.pin_owner(owner("user_a", "Alice"));
        assert_eq!(store.owner_check("user_a"), OwnerCheck::Match);
        assert_eq!(store.owner_check("user_b"), OwnerCheck::Mismatch);
        assert_eq!(store.owner().unwrap().display_name, "Alice");
    }

    #[test]
    fn refresh_display_name_only_when_changed_and_never_when_unpinned() {
        let mut store = LineageStore::new();
        // Unpinned: nothing to refresh.
        assert!(!store.refresh_display_name("Alice"));
        assert!(store.owner().is_none());

        store.pin_owner(owner("user_a", "Alice"));
        // Same name is a no-op.
        assert!(!store.refresh_display_name("Alice"));
        // A changed name updates and reports the change.
        assert!(store.refresh_display_name("Alice Cooper"));
        assert_eq!(store.owner().unwrap().display_name, "Alice Cooper");
        // The user id is left untouched.
        assert_eq!(store.owner().unwrap().user_id, "user_a");
    }

    #[test]
    fn owner_gate_covers_the_full_matrix() {
        let alice = owner("user_a", "Alice");

        // Unpinned defers to first-use, regardless of the flag.
        assert_eq!(owner_gate(None, None, "user_a", false), OwnerGate::FirstUse);
        assert_eq!(owner_gate(None, None, "user_a", true), OwnerGate::FirstUse);

        // A matching owner proceeds.
        assert_eq!(
            owner_gate(Some(&alice), None, "user_a", false),
            OwnerGate::Proceed
        );

        // A differing owner aborts without the flag, re-pins with it.
        assert_eq!(
            owner_gate(Some(&alice), None, "user_b", false),
            OwnerGate::AbortMismatch
        );
        assert_eq!(
            owner_gate(Some(&alice), None, "user_b", true),
            OwnerGate::Repin
        );

        // A configured id that differs ALWAYS aborts, even with the flag and
        // even on a first-use (unpinned) library.
        assert_eq!(
            owner_gate(Some(&alice), Some("user_c"), "user_a", true),
            OwnerGate::AbortConfigMismatch
        );
        assert_eq!(
            owner_gate(None, Some("user_c"), "user_a", true),
            OwnerGate::AbortConfigMismatch
        );
        // A configured id that matches does not interfere.
        assert_eq!(
            owner_gate(Some(&alice), Some("user_a"), "user_a", false),
            OwnerGate::Proceed
        );

        // Only Repin is additive.
        assert!(OwnerGate::Repin.is_additive());
        for gate in [
            OwnerGate::AbortConfigMismatch,
            OwnerGate::AbortMismatch,
            OwnerGate::Proceed,
            OwnerGate::FirstUse,
        ] {
            assert!(!gate.is_additive());
        }
    }

    #[test]
    fn adopt_decision_covers_every_branch() {
        let owned: BTreeSet<&str> = ["c1", "c2"].into_iter().collect();
        let empty: BTreeSet<&str> = BTreeSet::new();

        // Empty library adopts outright regardless of the listing or the flag.
        assert_eq!(
            adopt_decision(&["x", "y"], &empty, true, false),
            AdoptDecision::PinFresh
        );
        // Non-empty but not enumerated: cannot confirm, so leave it unpinned.
        assert_eq!(
            adopt_decision(&["c1"], &owned, false, false),
            AdoptDecision::SkipPin
        );
        assert_eq!(
            adopt_decision(&["c1"], &owned, false, true),
            AdoptDecision::SkipPin
        );
        // Enumerated with overlap: same account, adopt in normal mode.
        assert_eq!(
            adopt_decision(&["c1", "z"], &owned, true, false),
            AdoptDecision::PinAdopt
        );
        // Enumerated with no overlap: refuse without the flag, force-adopt with.
        assert_eq!(
            adopt_decision(&["z1", "z2"], &owned, true, false),
            AdoptDecision::Abort
        );
        assert_eq!(
            adopt_decision(&["z1", "z2"], &owned, true, true),
            AdoptDecision::AdoptForced
        );

        // Only the forced adoption is additive.
        assert!(AdoptDecision::AdoptForced.is_additive());
        for decision in [
            AdoptDecision::PinFresh,
            AdoptDecision::PinAdopt,
            AdoptDecision::Abort,
            AdoptDecision::SkipPin,
        ] {
            assert!(!decision.is_additive());
        }
    }

    #[test]
    fn older_store_without_owner_loads_as_none_and_pinned_roundtrips() {
        // A store written before the owner field existed loads with owner None.
        let json = r#"{"nodes":{},"edges":[]}"#;
        let store: LineageStore = serde_json::from_str(json).unwrap();
        assert!(store.owner().is_none());
        // An unpinned store omits the field entirely (skip_serializing_if).
        let value = serde_json::to_value(&store).unwrap();
        assert!(value.get("owner").is_none());

        // A pinned store round-trips and serialises the owner.
        let mut pinned = LineageStore::new();
        pinned.pin_owner(owner("user_a", "Alice"));
        let back: LineageStore =
            serde_json::from_str(&serde_json::to_string(&pinned).unwrap()).unwrap();
        assert_eq!(back, pinned);
        assert_eq!(back.owner().unwrap().user_id, "user_a");
    }
}
