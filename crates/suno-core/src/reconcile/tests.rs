//! The reconcile unit-test suite: deterministic scenarios over crafted
//! desired/manifest/local inputs asserting the plan, and above all the
//! deletion-safety gates (mirror enumeration, copy/archive wins, private and
//! trashed handling, path-alias suppression) that are the engine's #1 invariant.

use super::*;
use crate::hash::content_hash;

fn clip(id: &str) -> Clip {
    Clip {
        id: id.to_string(),
        title: "Song".to_string(),
        ..Default::default()
    }
}

fn lineage(id: &str) -> LineageContext {
    LineageContext::own_root(&clip(id))
}

fn entry(path: &str, format: AudioFormat, meta: &str, art: &str) -> ManifestEntry {
    ManifestEntry {
        path: path.to_string(),
        format,
        meta_hash: meta.to_string(),
        art_hash: art.to_string(),
        size: 100,
        preserve: false,
        ..Default::default()
    }
}

fn preserved_entry(path: &str, format: AudioFormat, meta: &str, art: &str) -> ManifestEntry {
    ManifestEntry {
        preserve: true,
        ..entry(path, format, meta, art)
    }
}

fn desired(id: &str, path: &str, format: AudioFormat, meta: &str, art: &str) -> Desired {
    Desired {
        clip: clip(id),
        lineage: lineage(id),
        path: path.to_string(),
        format,
        meta_hash: meta.to_string(),
        art_hash: art.to_string(),
        embedded_lyrics_hash: String::new(),
        modes: vec![SourceMode::Mirror],
        trashed: false,
        private: false,
        artifacts: Vec::new(),
        stems: None,
    }
}

fn present(size: u64) -> LocalFile {
    LocalFile { exists: true, size }
}

fn local_present(id: &str) -> HashMap<String, LocalFile> {
    [(id.to_string(), present(100))].into_iter().collect()
}

fn mirror_ok() -> Vec<SourceStatus> {
    vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: true,
    }]
}

mod album_art;
mod artifacts;
mod clobber;
mod deletion;
mod gates;
mod plan;
mod playlist;
mod relocation;
mod stems;
