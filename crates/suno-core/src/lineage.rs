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
/// This is the bridge between the pure resolver ([`Resolution`]) and the parts
/// of the engine that turn a clip into files: it carries exactly the resolved
/// values that get embedded in a path or a tag (the root the clip folders
/// under, the immediate parent and how it derives from it), so those consumers
/// never re-read the removed `root_ancestor_id`/`album_title` feed fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageContext {
    /// The resolved root ancestor id (the clip's own id when it is a root).
    pub root_id: String,
    /// The root ancestor's title (empty when the root is outside the index).
    ///
    /// When built via the lineage store ([`context_for`]/[`album_for_id`]) this
    /// carries the *effective* album title: a manual override supplants the
    /// derived title here, so the folder path, `ALBUM` tag, and change hash all
    /// reflect it from one source. Contexts built without the store (e.g.
    /// [`own_root`]) carry the raw title.
    ///
    /// [`context_for`]: crate::LineageStore::context_for
    /// [`album_for_id`]: crate::LineageStore::album_for_id
    /// [`own_root`]: LineageContext::own_root
    pub root_title: String,
    /// The root ancestor's creation timestamp (its raw `created_at`), or empty
    /// when the root is outside the index.
    ///
    /// Surfaced so the Year tag can group an album under its lineage root's
    /// year: a later revision that crosses a calendar boundary still carries the
    /// root's year. Contexts built without the store ([`own_root`]/[`for_clip`])
    /// carry the clip's own `created_at`, so [`year`] falls back to the clip's
    /// own year when the root's date is unavailable.
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
/// [`immediate_parent`], [`primary_parent`], [`lineage_edges`], or the root
/// walk in [`crate::roots`]. It feeds only the durable graph store (as a
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
mod tests {
    use super::*;
    use crate::model::HistoryEntry;

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

    fn clip_root(id: &str, handle: &str) -> crate::model::ClipRoot {
        crate::model::ClipRoot {
            id: id.to_owned(),
            handle: handle.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn attribution_edges_map_clip_roots_in_order() {
        let clip = Clip {
            id: "child".into(),
            handle: "me".into(),
            clip_attribution_type: "remix".into(),
            clip_roots: vec![
                clip_root("own-root", "me"),
                clip_root("foreign-root", "stranger"),
            ],
            ..Default::default()
        };
        let edges = attribution_edges(&clip);
        assert_eq!(edges.len(), 2);
        assert_eq!(
            edges[0],
            AttributionEdge {
                parent_id: "own-root".into(),
                edge_slug: "remix".into(),
                role: EdgeRole::Secondary,
                ordinal: 0,
                source_field: "clip_roots",
                same_owner: true,
            }
        );
        assert_eq!(edges[1].parent_id, "foreign-root");
        assert_eq!(edges[1].ordinal, 1);
        assert!(
            !edges[1].same_owner,
            "a differently-handled root is foreign, and still emits an edge"
        );
    }

    #[test]
    fn attribution_edges_are_empty_without_clip_roots() {
        let clip = Clip {
            id: "child".into(),
            handle: "me".into(),
            ..Default::default()
        };
        assert!(attribution_edges(&clip).is_empty());
    }

    #[test]
    fn attribution_edges_same_owner_is_fail_closed() {
        // Matching non-empty handles are same-owner; an empty handle on either
        // side, or a mismatch, is foreign (never fold a foreign remix in).
        let matched = Clip {
            handle: "me".into(),
            clip_roots: vec![clip_root("r", "me")],
            ..Default::default()
        };
        assert!(attribution_edges(&matched)[0].same_owner);

        let clip_blank = Clip {
            handle: "".into(),
            clip_roots: vec![clip_root("r", "me")],
            ..Default::default()
        };
        assert!(
            !attribution_edges(&clip_blank)[0].same_owner,
            "an empty clip handle is fail-closed to foreign"
        );

        let root_blank = Clip {
            handle: "me".into(),
            clip_roots: vec![clip_root("r", "   ")],
            ..Default::default()
        };
        assert!(
            !attribution_edges(&root_blank)[0].same_owner,
            "a whitespace-only root handle is fail-closed to foreign"
        );
    }

    #[test]
    fn attribution_edges_skip_a_root_with_no_id_and_keep_contiguous_ordinals() {
        let clip = Clip {
            handle: "me".into(),
            clip_attribution_type: "remix".into(),
            clip_roots: vec![
                clip_root("", "me"),
                clip_root(ZERO_UUID, "me"),
                clip_root("real-root", "me"),
            ],
            ..Default::default()
        };
        let edges = attribution_edges(&clip);
        assert_eq!(edges.len(), 1, "empty and sentinel root ids are dropped");
        assert_eq!(edges[0].parent_id, "real-root");
        assert_eq!(edges[0].ordinal, 0, "ordinals stay contiguous after a skip");
    }

    fn resolution_with(roots: Vec<(&str, RootInfo)>) -> Resolution {
        Resolution {
            roots: roots
                .into_iter()
                .map(|(id, info)| (id.to_owned(), info))
                .collect(),
            gap_filled: Vec::new(),
            bridges: Vec::new(),
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
            root_date: String::new(),
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

    #[test]
    fn year_prefers_the_root_year_over_the_clips_own() {
        // A December root with a January revision: the child tags the root's
        // year so the album groups under one year across the boundary.
        let ctx = LineageContext {
            root_id: "root-1".into(),
            root_title: "Origin".into(),
            root_date: "2023-12-30T23:00:00Z".into(),
            parent_id: "root-1".into(),
            edge_type: Some(EdgeType::Extend),
            status: ResolveStatus::Resolved,
        };
        assert_eq!(ctx.year("2024-01-02T08:00:00Z"), "2023");
    }

    #[test]
    fn year_falls_back_to_own_when_the_root_date_is_unavailable() {
        let ctx = LineageContext {
            root_id: "outside".into(),
            root_title: String::new(),
            root_date: String::new(),
            parent_id: "outside".into(),
            edge_type: Some(EdgeType::Cover),
            status: ResolveStatus::External,
        };
        assert_eq!(ctx.year("2024-07-01T00:00:00Z"), "2024");
    }

    #[test]
    fn own_root_tags_its_own_year() {
        let clip = Clip {
            id: "solo".into(),
            title: "Solo".into(),
            created_at: "2022-05-06T12:00:00Z".into(),
            ..Default::default()
        };
        let ctx = LineageContext::own_root(&clip);
        assert_eq!(ctx.root_date, "2022-05-06T12:00:00Z");
        assert_eq!(ctx.year(&clip.created_at), "2022");
    }

    #[test]
    fn year_is_empty_when_no_date_is_known() {
        let clip = Clip::default();
        let ctx = LineageContext::own_root(&clip);
        assert_eq!(ctx.year(&clip.created_at), "");
    }
}
