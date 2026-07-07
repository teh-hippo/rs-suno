//! Pure desired-state construction: clip target entries and playlist manifests.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path};

use crate::extras::{M3u8Entry, render_clip_details, render_clip_lyrics, render_m3u8};
use crate::hash::{
    art_hash, art_url_hash, content_hash, embedded_art_hash, meta_hash, synced_lrc_source_hash,
};
use crate::lineage::LineageContext;
use crate::model::Clip;
use crate::model::Stem;
use crate::naming::{
    CharacterSet, NamingConfig, NamingRequest, render_clip_names, sanitise_name, stem_file_path,
};
use crate::reconcile::{Desired, DesiredArtifact, DesiredStem, PlaylistDesired};
use crate::vocab::{ArtifactKind, AudioFormat, SourceMode, StemFormat, WebpEncodeSettings};

/// The synthetic playlist id for the liked feed, rendered as "Liked Songs".
///
/// Suno playlist ids are UUIDs, so this short literal never collides with a real
/// playlist id in the store keyspace.
pub const LIKED_PLAYLIST_ID: &str = "liked";

/// The per-song sidecar toggles resolved for a run: each embeds or writes one
/// artefact (animated WebP cover, `.details.txt`, `.lyrics.txt`, synced `.lrc`,
/// standalone `.mp4`). All default off.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArtifactToggles {
    pub animated_covers: bool,
    pub details: bool,
    pub lyrics: bool,
    pub lrc: bool,
    pub video: bool,
    /// The animated-cover encode settings, folded into the embedded-cover hash
    /// (see [`embedded_art_hash`]) so a settings change re-embeds existing covers.
    pub webp: WebpEncodeSettings,
}

/// One fetched playlist to render: its stable id, display name, and ordered
/// member clips (already non-trashed, in Suno order).
pub struct PlaylistInput<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub members: &'a [Clip],
}

/// Build the desired target state for a union of selected clips.
///
/// Naming is rendered as a batch so collisions are disambiguated globally. Each
/// clip's `modes` is stamped from `modes_by_id` — every selected area (mirror
/// and copy alike) that holds it — so a clip in both a `Mirror` and a `Copy`
/// carries both and copy-wins protection holds (SYNC-8). Every clip must appear
/// in the map; an empty `modes` would silently drop that protection, so it trips
/// a `debug_assert` (D6) and in release defaults to unprotected.
///
/// `contexts` supplies each clip's resolved [`LineageContext`] (album, lineage
/// tags, change hash), falling back to a self-rooted context when absent.
/// `colliding_albums` is the set of root titles shared by more than one root; a
/// clip in that set is folded into a `[{root_id8}]`-suffixed folder so two roots
/// never share one. `toggles` gates the per-song sidecars in [`clip_artifacts`].
pub fn build_desired(
    clips: &[&Clip],
    format: AudioFormat,
    modes_by_id: &HashMap<String, Vec<SourceMode>>,
    contexts: &HashMap<String, LineageContext>,
    colliding_albums: &BTreeSet<String>,
    toggles: ArtifactToggles,
    naming: &NamingConfig,
) -> Vec<Desired> {
    let lineages: Vec<LineageContext> = clips
        .iter()
        .map(|clip| {
            contexts
                .get(&clip.id)
                .cloned()
                .unwrap_or_else(|| LineageContext::own_root(clip))
        })
        .collect();
    // The requests borrow `lineages`; scope them so the borrow ends before the
    // lineages are moved into the desired entries below.
    let names = {
        let requests: Vec<NamingRequest<'_>> = clips
            .iter()
            .zip(&lineages)
            .map(|(clip, lineage)| NamingRequest { clip, lineage })
            .collect();
        render_clip_names(&requests, naming, colliding_albums)
    };

    clips
        .iter()
        .zip(names)
        .zip(lineages)
        .map(|((clip, name), lineage)| {
            // The extensionless audio path; the sidecars swap the extension.
            let base = rel_to_string(&name.relative_path);
            let path = format!("{base}.{}", format.ext());
            let meta_hash = meta_hash(clip, &lineage);
            let modes = modes_by_id.get(&clip.id).cloned().unwrap_or_default();
            // D6: empty modes would silently lose SYNC-8 copy protection.
            debug_assert!(
                !modes.is_empty(),
                "clip {} has no modes in the union map",
                clip.id
            );
            // Bind the artifacts before the struct literal so `&lineage` is
            // borrowed (for the details render) before it is moved in below.
            let artifacts = clip_artifacts(clip, &base, &lineage, toggles);
            Desired {
                clip: (*clip).clone(),
                lineage,
                path,
                format,
                meta_hash,
                art_hash: embedded_art_hash(
                    clip,
                    toggles.animated_covers && format.embeds_animated_cover(),
                    &toggles.webp,
                ),
                modes,
                trashed: clip.is_trashed,
                private: false,
                artifacts,
                // Stems are threaded in after this pure pass (they need a network
                // listing); `None` means "no authoritative stem info", so a run
                // without `download_stems` leaves any local stems untouched.
                stems: None,
            }
        })
        .collect()
}

/// Build the authoritative desired stem set for one clip from its listed stems.
///
/// Each stem file sits in the `{base}.stems/` sub-folder. Keys are the stable
/// stem id (falling back to label, then a positional key), de-duplicated so
/// blank or duplicate labels never collide. Stems are stored RAW (WAV or MP3),
/// never transcoded; the rewrite hash tracks the stem's public URL. A `Wav` stem
/// needs its own id to render, so an id-less stem falls back to `Mp3`.
///
/// Only ever called with an AUTHORITATIVE listing, so the result is safe to
/// drive stem removals against.
pub fn clip_stems(
    base: &str,
    stems: &[Stem],
    stem_format: StemFormat,
    character_set: CharacterSet,
) -> Vec<DesiredStem> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for (index, stem) in stems.iter().enumerate() {
        let base_key = if !stem.id.is_empty() {
            stem.id.clone()
        } else if !stem.label.is_empty() {
            stem.label.clone()
        } else {
            format!("stem{index}")
        };
        // Keep the key unique even when ids are blank and labels duplicate.
        let mut key = base_key.clone();
        let mut suffix = 1;
        while !seen.insert(key.clone()) {
            key = format!("{base_key}-{suffix}");
            suffix += 1;
        }
        // Disambiguate by stable stem id when present, else the key, so two
        // stems never map to the same file.
        let disambiguator = if stem.id.is_empty() {
            key.as_str()
        } else {
            stem.id.as_str()
        };
        // WAV needs the stem's own id to render; without one, store as MP3.
        let format = if stem_format == StemFormat::Wav && stem.id.is_empty() {
            StemFormat::Mp3
        } else {
            stem_format
        };
        let path = stem_file_path(
            base,
            &stem.label,
            disambiguator,
            format.ext(),
            character_set,
        );
        out.push(DesiredStem {
            key,
            stem_id: stem.id.clone(),
            path,
            source_url: stem.url.clone(),
            format,
            hash: art_url_hash(&stem.url),
        });
    }
    out
}

/// The per-clip sidecars desired alongside `base`, the extensionless audio path.
///
/// A static `CoverJpg` is emitted only when the clip has non-empty selected art;
/// an empty art URL emits none, and reconcile reads a missing cover as
/// UNKNOWN => KEEP, so a transient empty URL never removes an existing cover. The
/// animated cover is not a sidecar: under `toggles.animated_covers` it is
/// embedded as the audio file's front cover.
///
/// Text sidecars carry their body inline plus a `content_hash`, so an edit
/// rewrites them even when `meta_hash` is unchanged. `DetailsTxt` is emitted
/// whenever `toggles.details` is set; `LyricsTxt` only when the clip has lyrics,
/// so no empty file is written. `Lrc` is emitted for every clip under
/// `toggles.lrc` (alignment is knowable only from the endpoint, not the feed)
/// with no inline body, resolved just before execution; a clip with neither
/// alignment nor lyrics writes nothing, its emptiness cached so it is not
/// re-fetched.
fn clip_artifacts(
    clip: &Clip,
    base: &str,
    lineage: &LineageContext,
    toggles: ArtifactToggles,
) -> Vec<DesiredArtifact> {
    let mut artifacts = Vec::new();
    if let Some(url) = clip.selected_image_url().filter(|u| !u.is_empty()) {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::CoverJpg,
            path: format!("{base}.jpg"),
            source_url: url.to_owned(),
            hash: art_hash(clip),
            content: None,
        });
    }
    if toggles.details {
        let text = render_clip_details(clip, lineage);
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::DetailsTxt,
            path: format!("{base}.details.txt"),
            source_url: String::new(),
            hash: content_hash(&text),
            content: Some(text),
        });
    }
    if toggles.lyrics
        && let Some(text) = render_clip_lyrics(clip)
    {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::LyricsTxt,
            path: format!("{base}.lyrics.txt"),
            source_url: String::new(),
            hash: content_hash(&text),
            content: Some(text),
        });
    }
    if toggles.lrc {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::Lrc,
            path: format!("{base}.lrc"),
            source_url: String::new(),
            hash: synced_lrc_source_hash(&clip.id),
            content: None,
        });
    }
    if toggles.video && !clip.video_url.is_empty() {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::VideoMp4,
            path: format!("{base}.mp4"),
            source_url: clip.video_url.clone(),
            hash: art_url_hash(&clip.video_url),
            content: None,
        });
    }
    artifacts
}

/// Build the desired `.m3u8` playlists for this run from the fetched playlists.
///
/// Each input is rendered in Suno order: every member clip id is looked up in
/// this run's `desired` audio set for its relative path, title, and duration. A
/// member absent from that set is emitted as a `# (not in library)` comment (an
/// empty relative path) rather than a dangling path (HARDENING L1). The content
/// hash covers the whole rendered body so any name, order, path, title, or
/// duration change rewrites the file (HARDENING B1), named
/// `<sanitised name>.m3u8` at the library root.
///
/// Pure: the caller does the best-effort fetching, excludes any playlist whose
/// member fetch failed, and appends the synthetic liked feed.
pub fn build_playlist_desired(
    inputs: &[PlaylistInput<'_>],
    desired: &[Desired],
) -> Vec<PlaylistDesired> {
    let by_id: HashMap<&str, &Desired> = desired.iter().map(|d| (d.clip.id.as_str(), d)).collect();
    inputs
        .iter()
        .map(|input| {
            let entries: Vec<M3u8Entry<'_>> = input
                .members
                .iter()
                .map(|member| match by_id.get(member.id.as_str()) {
                    Some(d) => M3u8Entry {
                        title: d.clip.title.as_str(),
                        duration_secs: d.clip.duration,
                        relative_path: d.path.as_str(),
                    },
                    None => M3u8Entry {
                        title: member.title.as_str(),
                        duration_secs: member.duration,
                        relative_path: "",
                    },
                })
                .collect();
            let content = render_m3u8(input.name, &entries);
            let hash = content_hash(&content);
            let path = format!("{}.m3u8", sanitise_name(input.name));
            PlaylistDesired {
                id: input.id.to_owned(),
                name: input.name.to_owned(),
                path,
                content,
                hash,
            }
        })
        .collect()
}

/// Render a relative path as a forward-slash string, dropping any non-normal
/// component so the stored path is portable and never escapes the root.
///
/// The single source of truth for turning a rendered path into a stored/compared
/// one: `/`-separated on every OS, so manifest paths, `parent_dir`, and the
/// deletion-safety checks behave identically across platforms.
pub(crate) fn rel_to_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests;
