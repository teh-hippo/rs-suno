//! Pure desired-state construction: clip target entries and playlist manifests.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path};

use crate::client::Stem;
use crate::config::{AudioFormat, StemFormat};
use crate::extras::{M3u8Entry, render_clip_details, render_clip_lyrics, render_m3u8};
use crate::hash::{art_hash, art_url_hash, content_hash, meta_hash, synced_lrc_source_hash};
use crate::lineage::LineageContext;
use crate::model::Clip;
use crate::naming::{
    CharacterSet, NamingConfig, NamingRequest, render_clip_names, sanitise_name, stem_file_path,
};
use crate::reconcile::{
    ArtifactKind, Desired, DesiredArtifact, DesiredStem, PlaylistDesired, SourceMode,
};

/// The synthetic playlist id for the liked feed, rendered as "Liked Songs".
///
/// Suno playlist ids are UUIDs, so this short literal never collides with a real
/// playlist id in the store keyspace.
pub const LIKED_PLAYLIST_ID: &str = "liked";

/// The per-song sidecar toggles resolved for a run.
///
/// Each mirrors one resolved setting: `animated_covers` gates the `cover.webp`,
/// `details` the `.details.txt` dump, `lyrics` the `.lyrics.txt` file, `lrc`
/// the synced `.lrc` sidecar (Suno's word/line-level timed lyrics, which also
/// drives the MP3 `SYLT` frame and the plain lyric tag), and `video` the
/// standalone `.mp4` music video. All default off, matching the compiled config
/// defaults.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArtifactToggles {
    pub animated_covers: bool,
    pub details: bool,
    pub lyrics: bool,
    pub lrc: bool,
    pub video: bool,
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
                art_hash: art_hash(clip),
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
/// A static `CoverJpg` is emitted whenever the clip has non-empty selected art;
/// an animated `CoverWebp` only when `toggles.animated_covers` is set and the
/// clip carries a video preview. An empty art URL emits NO `CoverJpg`: reconcile
/// reads a desired that simply lacks a cover as UNKNOWN => KEEP, never a delete,
/// so a transient empty URL cannot strand or remove an existing cover. The
/// `CoverJpg` hash tracks the art URL (`art_hash`); the `CoverWebp` hash tracks
/// the video URL, so a changed source re-transcodes.
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
    if toggles.animated_covers && !clip.video_cover_url.is_empty() {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::CoverWebp,
            path: format!("{base}.webp"),
            source_url: clip.video_cover_url.clone(),
            hash: art_url_hash(&clip.video_cover_url),
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
fn rel_to_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::collections::HashMap;

    use super::*;
    use crate::config::AudioFormat;
    use crate::hash::{art_hash, art_url_hash, content_hash, synced_lrc_source_hash};
    use crate::lineage::LineageContext;
    use crate::naming::NamingConfig;
    use crate::reconcile::{ArtifactKind, SourceMode};

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

    #[test]
    fn build_desired_appends_extension_and_mode() {
        let a = clip("id-a", "Song A", "alice");
        let clips = [&a];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert_eq!(desired.len(), 1);
        assert!(
            desired[0].path.ends_with(".flac"),
            "path: {}",
            desired[0].path
        );
        assert_eq!(desired[0].format, AudioFormat::Flac);
        assert_eq!(desired[0].modes, vec![SourceMode::Mirror]);
        assert!(!desired[0].trashed);
        assert!(!desired[0].private);
        let lineage = LineageContext::own_root(&a);
        assert_eq!(desired[0].meta_hash, crate::hash::meta_hash(&a, &lineage));
        assert_eq!(desired[0].art_hash, art_hash(&a));
        assert_eq!(desired[0].lineage, lineage);
    }

    #[test]
    fn build_desired_carries_the_trashed_flag_from_the_clip() {
        let mut gone = clip("id-gone", "Removed", "alice");
        gone.is_trashed = true;
        let live = clip("id-live", "Kept", "alice");
        let clips = [&gone, &live];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(desired[0].trashed, "a trashed clip is marked trashed");
        assert!(!desired[1].trashed, "a live clip is not");
    }

    #[test]
    fn build_desired_uses_supplied_lineage_context() {
        use crate::lineage::ResolveStatus;

        let a = clip("child-1", "Remix", "alice");
        let clips = [&a];
        let lineage = LineageContext {
            root_id: "root-1".to_owned(),
            root_title: "Original".to_owned(),
            root_date: String::new(),
            parent_id: "root-1".to_owned(),
            edge_type: None,
            status: ResolveStatus::Resolved,
        };
        let contexts: HashMap<String, LineageContext> =
            [(a.id.clone(), lineage.clone())].into_iter().collect();
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &contexts,
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(
            desired[0].path.contains("/Original/"),
            "path: {}",
            desired[0].path
        );
        assert_eq!(desired[0].lineage, lineage);
        assert_eq!(desired[0].meta_hash, crate::hash::meta_hash(&a, &lineage));
    }

    #[test]
    fn lineage_is_stable_when_a_later_resolution_fails() {
        use crate::graph::LineageStore;
        use crate::lineage::{Resolution, ResolveStatus, RootInfo};

        let root = Clip {
            id: "root-break".into(),
            title: "Break Through".into(),
            clip_type: "gen".into(),
            handle: "alice".into(),
            display_name: "alice".into(),
            ..Default::default()
        };
        let child = Clip {
            id: "child-remix".into(),
            title: "Remix".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "root-break".into(),
            edited_clip_id: "root-break".into(),
            handle: "alice".into(),
            display_name: "alice".into(),
            ..Default::default()
        };
        let clips = [&root, &child];

        let contexts_of = |store: &LineageStore| -> HashMap<String, LineageContext> {
            clips
                .iter()
                .map(|c| (c.id.clone(), store.context_for(c)))
                .collect()
        };

        let mut roots = HashMap::new();
        for id in ["root-break", "child-remix"] {
            roots.insert(
                id.to_owned(),
                RootInfo {
                    root_id: "root-break".into(),
                    root_title: "Break Through".into(),
                    status: ResolveStatus::Resolved,
                },
            );
        }
        let resolution = Resolution {
            roots,
            gap_filled: Vec::new(),
            bridges: Vec::new(),
        };
        let mut store = LineageStore::new();
        store.update(&[root.clone(), child.clone()], &resolution, "t1");

        let cycle1 = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &contexts_of(&store),
            &store.colliding_root_titles(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        let child1 = cycle1.iter().find(|d| d.clip.id == "child-remix").unwrap();
        assert!(
            child1.path.contains("/Break Through/"),
            "the remix should folder under its root album, got {}",
            child1.path
        );

        let cycle2 = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &contexts_of(&store),
            &store.colliding_root_titles(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        for (a, b) in cycle1.iter().zip(&cycle2) {
            assert_eq!(a.path, b.path, "album path drifted for {}", a.clip.id);
            assert_eq!(
                a.meta_hash, b.meta_hash,
                "meta_hash drifted for {}",
                a.clip.id
            );
        }

        let own = LineageContext::own_root(&child);
        assert_ne!(
            crate::hash::meta_hash(&child, &own),
            child1.meta_hash,
            "own-root fallback must differ from the store-driven hash"
        );
    }

    #[test]
    fn build_desired_disambiguates_collisions() {
        let a = clip("id-a", "Same", "alice");
        let b = clip("id-b", "Same", "alice");
        let clips = [&a, &b];
        let desired = build_desired(
            &clips,
            AudioFormat::Mp3,
            &modes_for(&clips, SourceMode::Copy),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert_ne!(desired[0].path, desired[1].path);
        assert!(desired.iter().all(|d| d.path.ends_with(".mp3")));
        assert!(desired.iter().all(|d| d.modes == vec![SourceMode::Copy]));
    }

    #[test]
    fn build_desired_uses_forward_slashes() {
        let a = clip("id-a", "Song A", "alice");
        let clips = [&a];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(!desired[0].path.contains('\\'));
        assert!(desired[0].path.contains('/'));
    }

    #[test]
    fn build_desired_emits_cover_jpg_next_to_audio() {
        let a = art_clip("id-a");
        let clips = [&a];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        let base = desired[0].path.strip_suffix(".flac").unwrap();
        assert_eq!(desired[0].artifacts.len(), 1);
        let jpg = &desired[0].artifacts[0];
        assert_eq!(jpg.kind, ArtifactKind::CoverJpg);
        assert_eq!(jpg.path, format!("{base}.jpg"));
        assert_eq!(jpg.source_url, a.selected_image_url().unwrap());
        assert_eq!(jpg.hash, art_hash(&a));
    }

    #[test]
    fn build_desired_omits_cover_jpg_when_art_is_empty() {
        let a = clip("id-a", "Song", "alice");
        assert!(a.selected_image_url().is_none());
        let clips = [&a];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                animated_covers: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        assert!(desired[0].artifacts.is_empty());
    }

    #[test]
    fn build_desired_emits_cover_webp_only_when_animated_and_video_present() {
        let with_video = Clip {
            video_cover_url: "https://cdn.suno.ai/id-a/video.mp4".to_owned(),
            ..art_clip("id-a")
        };
        let clips = [&with_video];

        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert_eq!(desired[0].artifacts.len(), 1);
        assert_eq!(desired[0].artifacts[0].kind, ArtifactKind::CoverJpg);

        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                animated_covers: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = desired[0].path.strip_suffix(".flac").unwrap();
        let webp = desired[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::CoverWebp)
            .expect("animated cover expected");
        assert_eq!(webp.path, format!("{base}.webp"));
        assert_eq!(webp.source_url, with_video.video_cover_url);
        assert_eq!(webp.hash, art_url_hash(&with_video.video_cover_url));

        let no_video = art_clip("id-b");
        let clips = [&no_video];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                animated_covers: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        assert!(
            desired[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::CoverWebp)
        );
    }

    #[test]
    fn build_desired_emits_video_mp4_only_when_enabled_and_video_present() {
        let with_video = Clip {
            video_url: "https://cdn.suno.ai/id-a/video.mp4".to_owned(),
            ..art_clip("id-a")
        };
        let clips = [&with_video];

        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(
            desired[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::VideoMp4)
        );

        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                video: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = desired[0].path.strip_suffix(".flac").unwrap();
        let video = desired[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::VideoMp4)
            .expect("video expected");
        assert_eq!(video.path, format!("{base}.mp4"));
        assert_eq!(video.source_url, with_video.video_url);
        assert_eq!(video.hash, art_url_hash(&with_video.video_url));
        assert!(video.content.is_none());

        let no_video = art_clip("id-b");
        let clips = [&no_video];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                video: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        assert!(
            desired[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::VideoMp4)
        );
    }

    #[test]
    fn build_desired_emits_details_sidecar_only_when_enabled() {
        use crate::extras::render_clip_details;
        use crate::hash::content_hash;

        let a = clip("id-a", "Song", "alice");
        let clips = [&a];

        let off = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(
            off[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::DetailsTxt)
        );

        let on = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                details: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = on[0].path.strip_suffix(".flac").unwrap();
        let details = on[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::DetailsTxt)
            .expect("details sidecar expected");
        assert_eq!(details.path, format!("{base}.details.txt"));
        assert_eq!(details.source_url, "");
        let body = render_clip_details(&a, &LineageContext::own_root(&a));
        assert_eq!(details.content.as_deref(), Some(body.as_str()));
        assert_eq!(details.hash, content_hash(&body));
    }

    #[test]
    fn build_desired_emits_lyrics_sidecar_only_when_enabled_and_present() {
        let with_lyrics = Clip {
            lyrics: "la la la".to_owned(),
            ..clip("id-a", "Song", "alice")
        };
        let clips = [&with_lyrics];

        let off = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(
            off[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::LyricsTxt)
        );

        let on = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                lyrics: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = on[0].path.strip_suffix(".flac").unwrap();
        let lyrics = on[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::LyricsTxt)
            .expect("lyrics sidecar expected");
        assert_eq!(lyrics.path, format!("{base}.lyrics.txt"));
        assert_eq!(lyrics.source_url, "");
        assert_eq!(lyrics.content.as_deref(), Some("la la la\n"));
        assert_eq!(lyrics.hash, content_hash("la la la\n"));
    }

    #[test]
    fn build_desired_emits_lrc_sidecar_only_when_enabled() {
        let with_lyrics = Clip {
            lyrics: "la la la".to_owned(),
            ..clip("id-a", "Song", "alice")
        };
        let clips = [&with_lyrics];

        let off = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(
            off[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::Lrc)
        );

        let on = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                lrc: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = on[0].path.strip_suffix(".flac").unwrap();
        let lrc = on[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::Lrc)
            .expect("lrc sidecar expected");
        assert_eq!(lrc.path, format!("{base}.lrc"));
        assert_eq!(lrc.source_url, "");
        assert_eq!(lrc.content, None);
        assert_eq!(lrc.hash, synced_lrc_source_hash(&with_lyrics.id));
    }

    #[test]
    fn build_desired_emits_lrc_sidecar_from_prompt_when_feed_omits_lyrics() {
        let prompt_only = Clip {
            prompt: "the sung words live here".to_owned(),
            ..clip("id-a", "Song", "alice")
        };
        assert!(prompt_only.lyrics.is_empty());
        let clips = [&prompt_only];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                lrc: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let lrc = desired[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::Lrc)
            .expect("lrc sidecar expected");
        assert_eq!(lrc.content, None);
        assert_eq!(lrc.hash, synced_lrc_source_hash(&prompt_only.id));
    }

    #[test]
    fn build_desired_emits_lrc_sidecar_even_when_feed_has_no_lyrics_or_prompt() {
        let bare = clip("id-a", "Song", "alice");
        assert!(bare.lyrics.is_empty() && bare.prompt.is_empty());
        let clips = [&bare];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                lrc: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let lrc = desired[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::Lrc)
            .expect("lrc sidecar expected even with no feed lyrics/prompt");
        assert_eq!(lrc.content, None);
        assert_eq!(lrc.hash, synced_lrc_source_hash(&bare.id));
    }

    #[test]
    fn build_desired_omits_lyrics_sidecar_when_clip_has_no_lyrics() {
        let no_lyrics = clip("id-a", "Song", "alice");
        assert!(no_lyrics.lyrics.is_empty());
        let clips = [&no_lyrics];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                lyrics: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        assert!(
            desired[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::LyricsTxt)
        );
    }

    #[test]
    fn build_desired_text_sidecars_are_independent() {
        let full = Clip {
            lyrics: "words".to_owned(),
            ..art_clip("id-a")
        };
        let clips = [&full];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                details: true,
                lyrics: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = desired[0].path.strip_suffix(".flac").unwrap();
        let kinds: BTreeSet<ArtifactKind> = desired[0].artifacts.iter().map(|a| a.kind).collect();
        assert!(kinds.contains(&ArtifactKind::CoverJpg));
        assert!(kinds.contains(&ArtifactKind::DetailsTxt));
        assert!(kinds.contains(&ArtifactKind::LyricsTxt));
        let path_of_kind = |k: ArtifactKind| {
            desired[0]
                .artifacts
                .iter()
                .find(|a| a.kind == k)
                .unwrap()
                .path
                .clone()
        };
        assert_eq!(
            path_of_kind(ArtifactKind::DetailsTxt),
            format!("{base}.details.txt")
        );
        assert_eq!(
            path_of_kind(ArtifactKind::LyricsTxt),
            format!("{base}.lyrics.txt")
        );
    }

    #[test]
    fn build_desired_one_pass_disambiguates_and_stamps_modes() {
        let a = clip("lib-1", "Song", "alice");
        let b = clip("pl-1", "Song", "alice");
        let clips = [&a, &b];
        let mut modes = HashMap::new();
        modes.insert("lib-1".to_owned(), vec![SourceMode::Copy]);
        modes.insert(
            "pl-1".to_owned(),
            vec![SourceMode::Mirror, SourceMode::Copy],
        );
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes,
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert_eq!(desired.len(), 2);
        assert_ne!(desired[0].path, desired[1].path);
        assert_eq!(desired[1].modes, vec![SourceMode::Mirror, SourceMode::Copy]);
    }

    #[test]
    fn build_desired_respects_custom_naming_config() {
        use crate::naming::CharacterSet;

        let a = clip("abcdefgh-1234", "Song A", "alice");
        let clips = [&a];
        let custom = NamingConfig {
            template: "{title}/{id8}".to_owned(),
            character_set: CharacterSet::Ascii,
            ..NamingConfig::default()
        };
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &HashMap::from([("abcdefgh-1234".to_owned(), vec![SourceMode::Mirror])]),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &custom,
        );
        assert!(
            desired[0].path.starts_with("Song A/"),
            "path: {}",
            desired[0].path
        );
        assert!(desired[0].path.contains(&a.id[..8]));
    }

    #[test]
    fn build_playlist_desired_orders_members_and_marks_absent() {
        let a = clip("id-a", "Song A", "alice");
        let b = clip("id-b", "Song B", "alice");
        let desired = build_desired(
            &[&a, &b],
            AudioFormat::Flac,
            &modes_for(&[&a, &b], SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        let missing = clip("id-x", "Missing Song", "bob");
        let members = vec![b.clone(), missing.clone(), a.clone()];
        let inputs = vec![PlaylistInput {
            id: "pl1",
            name: "Road/Trip",
            members: &members,
        }];

        let out = build_playlist_desired(&inputs, &desired);
        assert_eq!(out.len(), 1);
        let pl = &out[0];
        assert_eq!(pl.id, "pl1");
        assert_eq!(pl.path, "Road Trip.m3u8");
        assert!(pl.content.starts_with("#EXTM3U\n#PLAYLIST:Road/Trip\n"));

        let pos_b = pl.content.find(path_of(&desired, "id-b")).unwrap();
        let pos_missing = pl.content.find("# (not in library) Missing Song").unwrap();
        let pos_a = pl.content.find(path_of(&desired, "id-a")).unwrap();
        assert!(pos_b < pos_missing && pos_missing < pos_a);
        assert!(!pl.content.contains("Missing Song\nbob/"));
        assert_eq!(pl.hash, content_hash(&pl.content));
    }

    #[test]
    fn build_playlist_desired_builds_liked_and_multiple_in_order() {
        let a = clip("id-a", "Song A", "alice");
        let desired = build_desired(
            &[&a],
            AudioFormat::Flac,
            &modes_for(&[&a], SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        let members = vec![a.clone()];
        let inputs = vec![
            PlaylistInput {
                id: "pl1",
                name: "First",
                members: &members,
            },
            PlaylistInput {
                id: LIKED_PLAYLIST_ID,
                name: "Liked Songs",
                members: &members,
            },
        ];

        let out = build_playlist_desired(&inputs, &desired);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "pl1");
        assert_eq!(out[1].id, LIKED_PLAYLIST_ID);
        assert_eq!(out[1].path, "Liked Songs.m3u8");
        assert!(out[0].content.contains(path_of(&desired, "id-a")));
        assert!(out[1].content.contains(path_of(&desired, "id-a")));
    }

    #[test]
    fn build_playlist_desired_is_empty_for_no_inputs() {
        assert!(build_playlist_desired(&[], &[]).is_empty());
    }
}
