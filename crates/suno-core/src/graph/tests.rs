//! The lineage-store test suite: end-to-end scenarios that populate a store
//! via [`LineageStore::update`] and assert its query, cache, and serde shape.

use std::collections::HashMap;

use super::node::{EdgeStatus, NodeStatus};
use super::store::normalise_slug;
use super::*;
use crate::album_art::{AlbumArt, PlaylistState};
use crate::identity::Owner;
use crate::lineage::{EdgeRole, EdgeType, LineageContext, Resolution, ResolveStatus, RootInfo};
use crate::model::Clip;

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
        bridges: Vec::new(),
    }
}

mod albums;
mod collision;
mod context;
mod shape;
mod update;
