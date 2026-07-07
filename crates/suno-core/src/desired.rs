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

/// The per-song sidecar toggles resolved for a run.
///
/// Each mirrors one resolved setting: `animated_covers` embeds a bounded
/// animated WebP as the audio file's front cover (in place of the static JPEG)
/// for clips with a video preview, `details` the `.details.txt` dump, `lyrics`
/// the `.lyrics.txt` file, `lrc` the synced `.lrc` sidecar (Suno's word/line-level
/// timed lyrics, which also drives the MP3 `SYLT` frame and the plain lyric tag),
/// and `video` the standalone `.mp4` music video. All default off, matching the
/// compiled config defaults.
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
/// Naming is rendered as a batch so collisions are disambiguated globally in one
/// pass, then the target format's extension is appended. Each clip's `modes` is
/// stamped from `modes_by_id`: the list of every selected area (mirror and copy
/// alike) that currently holds that clip. A clip held by a `Mirror` and a `Copy`
/// area at once therefore carries both, so copy-wins protection (SYNC-8) holds.
///
/// Every clip in `clips` must have an entry in `modes_by_id` (the caller builds
/// the map from the same union), so `modes` is never empty; an empty `modes`
/// would silently drop that clip's copy protection, so it trips a `debug_assert`
/// (D6). In a release build a clip missing from the map defaults to an empty
/// list, which reconcile then treats as unprotected, so callers must never omit
/// a clip from the map.
///
/// `contexts` carries the resolved [`LineageContext`] for each clip (keyed by
/// clip id); it drives the album component, the embedded lineage tags, and the
/// change hash, so the same resolved values flow all the way to the executor. A
/// clip missing from `contexts` falls back to a self-rooted context.
///
/// `colliding_albums` is the store's authoritative set of root titles shared by
/// more than one distinct root; a clip whose album is in that set is folded into
/// a `[{root_id8}]`-suffixed folder so two distinct roots never share one,
/// regardless of which clips this batch happens to hold.
///
/// `toggles` carries the resolved per-song sidecar switches (animated cover,
/// details text, lyrics text); each gates the matching sidecar in
/// [`clip_artifacts`].
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
            // D6: an empty modes vec would silently lose SYNC-8 copy protection
            // for this clip, so the caller must always list at least one area.
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
/// `base` is the clip's extensionless audio path, so each stem file sits in the
/// `{base}.stems/` sub-folder beside the song. Keys are the stable stem id
/// (falling back to the label, then a positional key), de-duplicated so blank or
/// duplicate labels never collide. The file name and its `[stem id8]`
/// disambiguator come from [`stem_file_path`], honouring the run's character
/// set, and its extension is the resolved [`StemFormat`] — stems are stored RAW
/// (WAV by default, or MP3), never transcoded to FLAC. The rewrite hash tracks
/// the stem's public MP3 URL (a changed URL, or a format switch that moves the
/// path, re-downloads), mirroring the video sidecar.
///
/// A `Wav` stem needs the stem's own id to render its lossless WAV, so a
/// (degenerate) stem with no id falls back to `Mp3` for that stem alone.
///
/// Only ever called with an AUTHORITATIVE listing (see run.rs), so the returned
/// set is safe to drive stem removals against.
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
        // Ensure the manifest key is unique within the clip even when ids are
        // blank and labels duplicate.
        let mut key = base_key.clone();
        let mut suffix = 1;
        while !seen.insert(key.clone()) {
            key = format!("{base_key}-{suffix}");
            suffix += 1;
        }
        // The filename disambiguator is the stable stem id when present, else the
        // resolved key, so two stems can never map to the same file.
        let disambiguator = if stem.id.is_empty() {
            key.as_str()
        } else {
            stem.id.as_str()
        };
        // WAV needs the stem's own id to render; without one, store this stem as
        // MP3 so the extension always matches what actually gets written.
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

/// The per-clip sidecars desired alongside `base`, the extensionless audio path
/// (so each sidecar sits next to the audio file).
///
/// A static `CoverJpg` is emitted whenever the clip has non-empty selected art.
/// An empty art URL emits NO `CoverJpg`: reconcile reads a desired that simply
/// lacks a cover as UNKNOWN => KEEP, never a delete, so a transient empty URL
/// cannot strand or remove an existing cover. The `CoverJpg` hash tracks the art
/// URL (`art_hash`). The animated cover is not a sidecar: when
/// `toggles.animated_covers` is set it is embedded as the audio file's front
/// cover (see [`embedded_art_hash`] and the executor), which is what media
/// servers such as Navidrome actually read.
///
/// The generated text sidecars carry their body inline (`content`) and a
/// per-sidecar `content_hash`, so a change to what the file holds (a retitle for
/// details, or edited lyrics) rewrites it even when `meta_hash` is unchanged.
/// `DetailsTxt` is always emitted when `toggles.details` is set (the render is
/// total); `LyricsTxt` only when `toggles.lyrics` is set and the clip has
/// non-empty lyrics (the render is partial), so no empty lyrics file is written.
/// The synced `Lrc` is emitted under `toggles.lrc` for every clip (alignment
/// availability is knowable only from the endpoint, not the feed), carrying a
/// source-proxy hash and no inline body; its timed body is resolved from the
/// fetched alignment just before execution, and a clip with neither alignment
/// nor lyrics writes no file (its emptiness cached so it is not re-fetched).
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
        // Emitted for every clip: alignment availability is knowable only from
        // the endpoint, not the feed (a clip can carry neither `lyrics` nor a
        // `prompt` yet still have full word/line alignment), so the fetch itself
        // decides. The artifact carries no inline body and a source-proxy hash
        // keyed on the (immutable) clip id plus the render version, so reconcile
        // skips an unchanged clip with no fetch while a version bump rewrites
        // every sidecar. The body is resolved just before execution (the untimed
        // lyrics when Suno has no alignment); a clip with neither alignment nor
        // lyrics resolves to nothing and writes no `.lrc`, its emptiness cached
        // on the manifest so it is not re-fetched every run.
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
/// Each input is rendered, in Suno order, into an extended-M3U8 body: every
/// member clip id is looked up in this run's `desired` audio set and mapped to
/// its rendered relative path, title, and duration. A member **absent from the
/// desired set** is emitted as an L1 `# (not in library)` comment (an empty
/// relative path in the [`M3u8Entry`]), using the member's own title, rather
/// than a dangling path (HARDENING L1). The content hash is taken over the full
/// rendered body so a name, order, path, title, or duration change all trigger a
/// rewrite (HARDENING B1), and the file path is `<sanitised name>.m3u8` at the
/// library root.
///
/// This is pure; the caller (run) does the best-effort fetching, excludes any
/// playlist whose member fetch failed, and appends the synthetic liked feed as a
/// final input with id [`LIKED_PLAYLIST_ID`].
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
/// This is the single source of truth for turning a rendered [`PathBuf`] into a
/// stored/compared path: it is `/`-separated on every OS (on Windows the
/// per-OS separator is normalised away), so manifest paths, `parent_dir`, and
/// the deletion-safety checks behave identically across platforms.
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
