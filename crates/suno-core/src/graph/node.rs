//! The relational graph model: clip nodes, parent edges, and cached root
//! resolutions. Pure serde data types with no logic of their own.

use serde::{Deserialize, Serialize};

use crate::lineage::{EdgeRole, ResolveStatus};

/// Lifecycle marker for a [`Node`]: `"observed"` for a clip seen from the feed or gap-fill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    #[default]
    #[serde(other)]
    Observed,
}

/// Lifecycle marker for a [`StoredEdge`]: `"active"` for an edge observed this run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeStatus {
    #[default]
    #[serde(other)]
    Active,
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
    pub status: NodeStatus,
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
            status: NodeStatus::Observed,
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
    pub role: EdgeRole,
    /// The clip field the parent id was read from, e.g. `"cover_clip_id"`.
    pub source_field: String,
    /// Position within its role (0 for the primary, then secondaries in order).
    pub ordinal: u32,
    pub status: EdgeStatus,
    pub first_seen_at: String,
    pub last_seen_at: String,
}

impl Default for StoredEdge {
    fn default() -> Self {
        Self {
            child_id: String::new(),
            parent_id: String::new(),
            edge_type: String::new(),
            role: EdgeRole::Primary,
            source_field: String::new(),
            ordinal: 0,
            status: EdgeStatus::Active,
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
    pub status: ResolveStatus,
    pub algorithm_version: u32,
    pub computed_at: String,
}
