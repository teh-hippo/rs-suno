//! Pure desired-state construction: clip target entries and playlist manifests.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path};

use crate::extras::{M3u8Entry, render_clip_details, render_m3u8};
use crate::hash::{
    art_hash, art_url_hash, content_hash, embedded_art_hash, meta_hash, synced_lrc_source_hash,
    webp_art_hash,
};
use crate::lineage::LineageContext;
use crate::lyrics::render_clip_lyrics;
use crate::model::Clip;
use crate::model::Stem;
use crate::naming::{
    CharacterSet, NamingConfig, NamingRequest, render_clip_names, sanitise_name, stem_file_path,
};
use crate::reconcile::{AlbumDesired, Desired, DesiredArtifact, DesiredStem, PlaylistDesired};
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
                // Always overwritten at the synced-lyrics resolve seam
                // (`apply_synced_lrc`/`preview_synced_lrc`) before reconcile; the
                // empty default only matters for this struct literal itself.
                embedded_lyrics_hash: String::new(),
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
            path: sidecar_path(base, ArtifactKind::CoverJpg),
            source_url: url.to_owned(),
            hash: art_hash(clip),
            content: None,
        });
    }
    if toggles.details {
        let text = render_clip_details(clip, lineage);
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::DetailsTxt,
            path: sidecar_path(base, ArtifactKind::DetailsTxt),
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
            path: sidecar_path(base, ArtifactKind::LyricsTxt),
            source_url: String::new(),
            hash: content_hash(&text),
            content: Some(text),
        });
    }
    if toggles.lrc {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::Lrc,
            path: sidecar_path(base, ArtifactKind::Lrc),
            source_url: String::new(),
            hash: synced_lrc_source_hash(&clip.id),
            content: None,
        });
    }
    if toggles.video && !clip.video_url.is_empty() {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::VideoMp4,
            path: sidecar_path(base, ArtifactKind::VideoMp4),
            source_url: clip.video_url.clone(),
            hash: art_url_hash(&clip.video_url),
            content: None,
        });
    }
    artifacts
}

/// The path of a per-clip sidecar built from the song's extensionless `base`.
///
/// The per-kind extension lives once on [`ArtifactKind::sidecar_suffix`] (#355),
/// so the desired path and reconcile's stranded-sidecar relocation derive it
/// from the same source. The `.expect` is only ever reached with a per-clip
/// kind and fails loudly on a future miswiring, matching the codebase's
/// existing guard style.
fn sidecar_path(base: &str, kind: ArtifactKind) -> String {
    format!(
        "{base}{}",
        kind.sidecar_suffix()
            .expect("per-clip sidecar kind has a suffix")
    )
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

/// Derive the desired folder art for every album in `desired`, grouped by the
/// stable root id (HARDENING H2).
///
/// This is pure: it groups the selected clips by their resolved `root_id`, then
/// per album chooses the folder-art sources deterministically:
///
/// - `folder.jpg` comes from the MOST-PLAYED art-bearing variant; ties break to
///   the EARLIEST `created_at`, then the lexicographically smallest id. Its hash
///   is the chosen art's content hash ([`art_hash`]), so a most-played flip to a
///   variant sharing the same art is a no-op downstream (H1).
/// - `cover.webp` (only when `animated_covers` is set) comes from the
///   EARLIEST-created variant with a non-empty `video_cover_url`; ties break to
///   the smallest id. Its hash folds in the `webp` encode settings, so changing
///   quality/lossless/effort re-transcodes it. `None` when no variant has an
///   animated source.
/// - `cover.mp4` (only when `raw_cover` is set) is that same variant's
///   `video_cover_url` kept verbatim (no transcode), so `both` yields the raw
///   source beside its WebP re-encode. `None` when no variant has an animated
///   source.
///
/// The album folder is the common parent of the album's clips' audio paths (they
/// share `{creator}/{album}/`); `folder.jpg` lands at `{album_dir}/folder.jpg`
/// and the animated covers at `{album_dir}/cover.webp` / `{album_dir}/cover.mp4`.
pub fn album_desired(
    desired: &[Desired],
    animated_covers: bool,
    raw_cover: bool,
    webp: WebpEncodeSettings,
) -> Vec<AlbumDesired> {
    let mut groups: BTreeMap<&str, Vec<&Desired>> = BTreeMap::new();
    for d in desired {
        groups
            .entry(d.lineage.root_id.as_str())
            .or_default()
            .push(d);
    }

    groups
        .into_iter()
        .map(|(root_id, members)| {
            let album_dir = album_dir_of(&members);
            let folder_jpg = folder_jpg_source(&members).map(|source| DesiredArtifact {
                kind: ArtifactKind::FolderJpg,
                path: album_child(&album_dir, "folder.jpg"),
                source_url: source.clip.selected_image_url().unwrap_or("").to_owned(),
                hash: art_hash(&source.clip),
                content: None,
            });
            let folder_webp = animated_covers
                .then(|| folder_webp_source(&members))
                .flatten()
                .map(|source| DesiredArtifact {
                    kind: ArtifactKind::FolderWebp,
                    path: album_child(&album_dir, "cover.webp"),
                    source_url: source.clip.video_cover_url.clone(),
                    hash: webp_art_hash(&source.clip.video_cover_url, &webp),
                    content: None,
                });
            let folder_mp4 = raw_cover
                .then(|| folder_webp_source(&members))
                .flatten()
                .map(|source| DesiredArtifact {
                    kind: ArtifactKind::FolderMp4,
                    path: album_child(&album_dir, "cover.mp4"),
                    source_url: source.clip.video_cover_url.clone(),
                    hash: art_url_hash(&source.clip.video_cover_url),
                    content: None,
                });
            AlbumDesired {
                root_id: root_id.to_owned(),
                folder_jpg,
                folder_webp,
                folder_mp4,
            }
        })
        .collect()
}

/// The album folder: the common parent of the members' audio paths.
///
/// The album's clips share `{creator}/{album}/`, so any member's parent is the
/// album dir; the smallest is taken so a stray differing path stays deterministic.
fn album_dir_of(members: &[&Desired]) -> String {
    members
        .iter()
        .map(|d| parent_dir(&d.path))
        .min()
        .unwrap_or("")
        .to_owned()
}

/// The most-played art-bearing variant: the `folder.jpg` source.
///
/// Filtered to variants that carry selectable art, then the winner MAXIMISES
/// `play_count`, breaking ties to the EARLIEST `created_at` and then the
/// lexicographically smallest id, so selection is fully deterministic.
fn folder_jpg_source<'a>(members: &[&'a Desired]) -> Option<&'a Desired> {
    members
        .iter()
        .copied()
        .filter(|d| {
            d.clip
                .selected_image_url()
                .is_some_and(|url| !url.is_empty())
        })
        .min_by(|a, b| {
            b.clip
                .play_count
                .cmp(&a.clip.play_count)
                .then_with(|| a.clip.created_at.cmp(&b.clip.created_at))
                .then_with(|| a.clip.id.cmp(&b.clip.id))
        })
}

/// The first-created animated variant: the `cover.webp` source.
///
/// Filtered to variants with a non-empty `video_cover_url`, then the winner is
/// the EARLIEST `created_at`, tie-broken by the smallest id for determinism.
fn folder_webp_source<'a>(members: &[&'a Desired]) -> Option<&'a Desired> {
    members
        .iter()
        .copied()
        .filter(|d| !d.clip.video_cover_url.is_empty())
        .min_by(|a, b| {
            a.clip
                .created_at
                .cmp(&b.clip.created_at)
                .then_with(|| a.clip.id.cmp(&b.clip.id))
        })
}

/// The parent directory of a forward-slash relative path, or `""` at the root.
fn parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// Join an album dir and a file name with a forward slash, tolerating an empty
/// dir (a path at the account root).
fn album_child(album_dir: &str, name: &str) -> String {
    if album_dir.is_empty() {
        name.to_owned()
    } else {
        format!("{album_dir}/{name}")
    }
}

#[cfg(test)]
mod tests;
