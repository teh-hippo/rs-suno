//! The desired-state builder test suite: drives `build_desired`,
//! `clip_stems`, `clip_artifacts`, and `build_playlist_desired` over crafted
//! clips and asserts the artifact set, naming, and playlist membership.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::PathBuf;

use super::*;
use crate::hash::{art_hash, art_url_hash, content_hash, synced_lrc_source_hash, webp_art_hash};
use crate::lineage::LineageContext;
use crate::naming::NamingConfig;
use crate::vocab::{ArtifactKind, AudioFormat, SourceMode, WebpEncodeSettings};

fn clip(id: &str, title: &str, handle: &str) -> Clip {
    Clip {
        id: id.to_owned(),
        title: title.to_owned(),
        handle: handle.to_owned(),
        display_name: handle.to_owned(),
        ..Default::default()
    }
}

fn no_contexts() -> HashMap<String, LineageContext> {
    HashMap::new()
}

fn no_collisions() -> BTreeSet<String> {
    BTreeSet::new()
}

fn modes_for(clips: &[&Clip], mode: SourceMode) -> HashMap<String, Vec<SourceMode>> {
    clips.iter().map(|c| (c.id.clone(), vec![mode])).collect()
}

/// `build_desired` with the defaults every plain case repeats: one `mode` for
/// all clips, no lineage contexts, no album/id collisions, and the default
/// naming config. Sites that vary any of those (custom contexts, collisions,
/// per-id modes, or naming) call `build_desired` directly.
fn desired_of(
    clips: &[&Clip],
    format: AudioFormat,
    mode: SourceMode,
    toggles: ArtifactToggles,
) -> Vec<Desired> {
    build_desired(
        clips,
        format,
        &modes_for(clips, mode),
        &no_contexts(),
        &no_collisions(),
        &no_collisions(),
        toggles,
        &NamingConfig::default(),
    )
}

fn art_clip(id: &str) -> Clip {
    Clip {
        image_large_url: format!("https://art.suno.ai/{id}/large.jpg"),
        ..clip(id, "Song", "alice")
    }
}

fn path_of<'a>(desired: &'a [Desired], id: &str) -> &'a str {
    desired
        .iter()
        .find(|d| d.clip.id == id)
        .map(|d| d.path.as_str())
        .expect("clip in desired set")
}

mod album;
mod build;
mod playlist;
mod sidecars;
