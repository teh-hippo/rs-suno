//! Pure lineage classification: classify a clip's parent edge and model the
//! data types a resolved library is expressed in.
//!
//! Suno records how a clip was derived across a scatter of metadata fields
//! (`task`, `type`, and a family of `*_clip_id` pointers plus `history` and
//! `concat_history`). This module turns those into a single primary parent per
//! clip ([`immediate_parent`]) classified by [`EdgeType`] and the full set of
//! parent [`Edge`]s for the later graph store ([`lineage_edges`]).
//!
//! Classification is deliberately blind to `is_remix`: that flag is a UI hint,
//! not a structural fact, so it never changes an edge. Everything here is pure
//! and free of IO: the async root-ancestor walk that gap-fills missing
//! ancestors over the network lives in [`crate::roots`], which depends on these
//! classifiers and data types (a one-way edge).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EdgeRole {
    /// The single lineage parent used for root resolution and album grouping.
    #[default]
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

/// The outcome of resolving a clip's root ancestor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveStatus {
    /// Resolution stopped at an ancestor outside the index (gap-fill budget
    /// exhausted, or the API reported it has no parent of its own).
    External,
    /// The root could not be determined within the hop cap.
    Unresolved,
    /// A cycle was detected while walking (pathological data).
    Cycle,
    /// The root was reached: a clip present in the index with no parent.
    /// Also the fallback for any unknown future status slug.
    #[default]
    #[serde(other)]
    Resolved,
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

/// The outcome of [`resolve_roots`](crate::resolve_roots): a root for every input clip, plus the
/// ancestor clips fetched to bridge gaps.
///
/// `gap_filled` is kept structurally separate from `roots` on purpose. Those
/// ancestors (often trashed) exist only so lineage could be walked; a later
/// phase persists them to the graph store so a trashed ancestor is archived
/// before Suno's purge, but they must never be treated as download candidates.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The resolved root for every clip passed to [`resolve_roots`](crate::resolve_roots), keyed by
    /// clip id.
    pub roots: HashMap<String, RootInfo>,
    /// Ancestor clips fetched during gap-fill, sorted by id. Not download
    /// candidates: they were pulled solely to complete the lineage walk.
    pub gap_filled: Vec<Clip>,
    /// Parent links discovered via the parent endpoint (`get_clip_parent`) as
    /// `(child_id, parent_id)`, sorted. The child is a bridged id that may have
    /// no clip of its own, so it is persisted as an archived edge (never a
    /// download candidate) to keep the parent-endpoint hop durable.
    pub bridges: Vec<(String, String)>,
}

/// The resolved lineage of a single clip, threaded into naming, tagging, and
/// change detection.
///
/// The bridge between the pure resolver ([`Resolution`]) and the parts of the
/// engine that turn a clip into files: it carries exactly the resolved values
/// embedded in a path or tag (the folder root, the immediate parent and its
/// edge), so those consumers never re-read raw feed fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageContext {
    /// The resolved root ancestor id (the clip's own id when it is a root).
    pub root_id: String,
    /// The root ancestor's title (empty when the root is outside the index).
    ///
    /// Built via the lineage store ([`context_for`]/[`album_for_id`]), this
    /// carries the *effective* album title, so a manual override supplants the
    /// derived title here and the path, `ALBUM` tag, and change hash all reflect
    /// it from one source. Store-less contexts ([`own_root`]) carry the raw
    /// title.
    ///
    /// [`context_for`]: crate::LineageStore::context_for
    /// [`album_for_id`]: crate::LineageStore::album_for_id
    /// [`own_root`]: LineageContext::own_root
    pub root_title: String,
    /// The root ancestor's creation timestamp (its raw `created_at`), or empty
    /// when the root is outside the index.
    ///
    /// Surfaced so the Year tag groups an album under its lineage root's year: a
    /// later revision crossing a calendar boundary still carries the root's year.
    /// Store-less contexts ([`own_root`]/[`for_clip`]) carry the clip's own
    /// `created_at`, so [`year`] then falls back to the clip's own year.
    ///
    /// [`own_root`]: LineageContext::own_root
    /// [`for_clip`]: LineageContext::for_clip
    /// [`year`]: LineageContext::year
    pub root_date: String,
    /// The immediate parent id ([`immediate_parent`]); empty for a root.
    pub parent_id: String,
    /// How the clip derives from its parent; `None` for a root.
    pub edge_type: Option<EdgeType>,
    /// How root resolution terminated.
    pub status: ResolveStatus,
    /// This clip's 1-based position within its lineage album, or `0` when
    /// unnumbered. Assigned album-wide by
    /// [`assign_track_numbers`](crate::assign_track_numbers); every constructor
    /// here leaves it `0`, so a lone or fallback context is never numbered.
    pub track: u32,
    /// The album's track count paired with [`track`](Self::track), or `0` when
    /// unnumbered.
    pub track_total: u32,
}

impl LineageContext {
    /// Build the context for `clip` from a whole-library [`Resolution`].
    ///
    /// Root id/title/status come from `resolution.roots[clip.id]`; when the clip
    /// is absent (it was not part of the resolved set) it is treated as its own
    /// resolved root. The parent id and edge come from [`immediate_parent`],
    /// which is empty/`None` for a root. `root_date` is the clip's own
    /// `created_at`: this store-less path has no window onto the root's date, so
    /// [`year`](Self::year) falls back to the clip's own year.
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
            root_date: clip.created_at.clone(),
            parent_id,
            edge_type,
            status,
            track: 0,
            track_total: 0,
        }
    }

    /// A self-rooted context for `clip`: it is treated as its own resolved root
    /// with no parent. Used as a defensive fallback where a resolved context is
    /// unavailable (a clip absent from the current desired set). `root_date` is
    /// the clip's own `created_at`, so it tags its own year.
    pub fn own_root(clip: &Clip) -> LineageContext {
        LineageContext {
            root_id: clip.id.clone(),
            root_title: clip.title.clone(),
            root_date: clip.created_at.clone(),
            parent_id: String::new(),
            edge_type: None,
            status: ResolveStatus::Resolved,
            track: 0,
            track_total: 0,
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

    /// The album's release year: the lineage root's creation year when known,
    /// otherwise `own_created_at`'s year.
    ///
    /// The root anchors the year so an album whose tracks straddle a calendar
    /// boundary (a December root with a January revision) groups under one year,
    /// mirroring how [`album`](Self::album) anchors the folder on the root's
    /// title. A root uses its own year; the fallback covers a root whose date is
    /// outside the index.
    pub fn year(&self, own_created_at: &str) -> String {
        let root_year = year_of(&self.root_date);
        if root_year.is_empty() {
            year_of(own_created_at)
        } else {
            root_year
        }
    }
}

/// The 4-digit calendar year prefix of an ISO-8601 `created_at`, or empty when
/// `created_at` is empty.
fn year_of(created_at: &str) -> String {
    created_at.chars().take(4).collect()
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

/// An attribution edge derived from a clip's nested `clip_roots` list.
///
/// This is informational lineage the API states directly (the clip was remixed
/// from these roots), NOT a structural parent. It is deliberately kept apart
/// from the [`EdgeType`]-classified structural [`Edge`]s: it is NEVER read by
/// [`immediate_parent`], `primary_parent`, [`lineage_edges`], or the root
/// walk in `crate::roots`. It feeds only the durable graph store (as a
/// secondary edge carrying the open attribution slug) and, for a same-owner
/// root, a bounded gap-fill seed. It can never fabricate a structural parent,
/// an external boundary, or a download candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributionEdge {
    /// The root clip id, normalised (`m_` stripped, sentinel dropped).
    pub parent_id: String,
    /// The raw attribution slug from `clip_attribution_type` (open, e.g.
    /// `"remix"`); normalisation to a stored form happens at the graph layer,
    /// never against the closed [`EdgeType`].
    pub edge_slug: String,
    /// Always [`EdgeRole::Secondary`]: attribution never supplants a clip's
    /// structural primary parent.
    pub role: EdgeRole,
    /// Position within the clip's `clip_roots.clips[]` list (0..N).
    pub ordinal: u32,
    /// The field these came from (always `"clip_roots"`).
    pub source_field: &'static str,
    /// Whether the root shares the clip's owner handle (fail-closed): only a
    /// same-owner root is ever gap-fill seeded.
    pub same_owner: bool,
}

/// Every attribution edge from a clip's `clip_roots` (empty when absent).
///
/// One edge per root with a usable id, in list order. Emitted for EVERY root
/// regardless of owner: a foreign-owned root still records its attribution. The
/// `same_owner` flag gates only the later gap-fill seed, never the edge itself.
pub fn attribution_edges(clip: &Clip) -> Vec<AttributionEdge> {
    let mut edges = Vec::new();
    for root in &clip.clip_roots {
        let Some(parent_id) = normalise_id(&root.id) else {
            continue;
        };
        let ordinal = edges.len() as u32;
        edges.push(AttributionEdge {
            parent_id,
            edge_slug: clip.clip_attribution_type.clone(),
            role: EdgeRole::Secondary,
            ordinal,
            source_field: "clip_roots",
            same_owner: same_owner(clip, root),
        });
    }
    edges
}

/// Whether a `clip_root` shares the clip's owner, by handle equality.
///
/// Fail-closed: an empty or missing handle on either side is a foreign owner,
/// so a rename or an absent identity never folds a foreign remix source into
/// the owner's album via the gap-fill seed. Never relax to substring or prefix
/// matching.
fn same_owner(clip: &Clip, root: &crate::model::ClipRoot) -> bool {
    let clip_handle = clip.handle.trim();
    let root_handle = root.handle.trim();
    !clip_handle.is_empty() && !root_handle.is_empty() && clip_handle == root_handle
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

#[cfg(test)]
mod tests;
