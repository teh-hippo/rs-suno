//! Stable content sentinels for change detection.
//!
//! Reconcile compares a clip's current [`meta_hash`]/[`art_hash`] against the
//! manifest to decide whether a file needs re-tagging. The hashes must be stable
//! across runs, versions, and platforms, so they use FNV-1a over a fixed field
//! encoding rather than the standard library's deliberately unspecified hasher.
//!
//! The hash inputs are the exact fields the tag writer embeds: [`meta_hash`]
//! hashes the resolved [`TrackMetadata`], and [`art_hash`] tracks the chosen art
//! URL. Anything embedded in the file is therefore in a hash, so an upstream
//! change to it triggers a retag; anything not embedded (path-affecting or
//! sidecar-only fields such as the animated-cover URL) is excluded.

use std::hash::{Hash, Hasher};

use crate::lineage::LineageContext;
use crate::model::Clip;
use crate::tag::TrackMetadata;
use crate::vocab::WebpEncodeSettings;

/// A short, stable hex digest of `bytes` (FNV-1a, 64-bit).
fn digest(bytes: &[u8]) -> String {
    let mut hasher = fnv::FnvHasher::default();
    hasher.write(bytes);
    format!("{:016x}", hasher.finish())
}

/// A stable sentinel over an arbitrary generated text artefact.
///
/// Used for playlists, whose `.m3u8` body is generated rather than fetched: the
/// hash is taken over the **full rendered text**, so the playlist name, the
/// member order, and every member's relative path, title, and duration all feed
/// it (HARDENING B1: a change to anything that ends up in the file changes the
/// hash and so triggers a rewrite). Because the render is deterministic, the
/// hash is stable across runs and platforms.
pub fn content_hash(text: &str) -> String {
    digest(text.as_bytes())
}

/// A sentinel for the clip's embedded tag set.
///
/// Hashes the resolved [`TrackMetadata`] that is actually written into the file
/// (title, artist, album, date/year, lyrics, prompt, model, handle, and the
/// resolved lineage tags), so a change to any embedded tag — including the
/// artist (`display_name`) and model label, which the old hand-listed field set
/// omitted — is detected as a needed retag, while a change to a field that is
/// *not* embedded in the audio (e.g. the animated-cover URL) is not. Taking
/// [`TrackMetadata`] directly keeps this in lock-step with the tag writer
/// (HARDENING B1: if a value is embedded, it is in the change hash), so a
/// retitle, artist rename, re-point, album move, or year correction all trigger
/// a retag. Chosen art is tracked separately by [`art_hash`].
///
/// A pure path change (e.g. one driven only by a field that renames but does
/// not embed) is still handled as a rename, by comparing the rendered path with
/// the stored one, not by this hash.
pub fn meta_hash(clip: &Clip, lineage: &LineageContext) -> String {
    let mut hasher = fnv::FnvHasher::default();
    TrackMetadata::from_clip(clip, lineage).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// A stable digest of an artifact source URL (FNV-1a), or the empty string when
/// `url` is empty.
///
/// Shared by [`art_hash`] (the embedded static cover) and the external animated
/// cover sidecar, whose rewrite detection keys on the clip's `video_cover_url`
/// rather than the selected image. Keeping both on the one helper means an empty
/// URL always maps to the empty sentinel, the value reconcile reads as "no such
/// artifact this run".
pub fn art_url_hash(url: &str) -> String {
    if url.is_empty() {
        String::new()
    } else {
        digest(url.as_bytes())
    }
}

/// A digest of a transcoded animated-cover source URL *and* the encode settings
/// that shape its bytes, or the empty string when `url` is empty.
///
/// Unlike the raw `cover.mp4` (a verbatim copy, keyed on its URL alone via
/// [`art_url_hash`]), the animated `cover.webp` and per-song `.webp` are a
/// transcode: their bytes depend on quality, lossless, effort, frame-rate, and
/// width as much as on the source. Folding those into the hash means a settings
/// change (for example raising the default quality) re-encodes existing covers
/// on the next run, exactly as a changed source URL does, rather than leaving
/// them stale. An empty URL still maps to the empty "no artifact" sentinel.
pub fn webp_art_hash(url: &str, settings: &WebpEncodeSettings) -> String {
    if url.is_empty() {
        return String::new();
    }
    let mut hasher = fnv::FnvHasher::default();
    hasher.write(url.as_bytes());
    hasher.write_u8(0);
    hasher.write_u8(settings.quality);
    hasher.write_u8(u8::from(settings.lossless));
    hasher.write_u8(settings.compression_level);
    hasher.write_u32(settings.max_fps);
    match settings.max_width {
        Some(width) => {
            hasher.write_u8(1);
            hasher.write_u32(width);
        }
        None => hasher.write_u8(0),
    }
    format!("{:016x}", hasher.finish())
}

/// The change-detection version for the synced `.lrc` body. Bump this when the
/// rendered `.lrc` format changes so existing sidecars are rewritten on the next
/// run (their stored hash then no longer matches, exactly as edited content
/// would move a [`content_hash`]).
pub const SYNCED_LRC_VERSION: u32 = 2;

/// A stable per-clip source sentinel for the synced `.lrc` sidecar.
///
/// Suno's forced alignment for a given clip is immutable (the audio and its
/// lyrics are fixed once generated), so the sidecar's rewrite detection keys on
/// the clip id plus the render [`SYNCED_LRC_VERSION`] rather than the fetched
/// body. This lets reconcile skip an unchanged clip WITHOUT a network fetch (the
/// timed body is resolved only when a write is actually planned), while a
/// version bump rewrites every sidecar. It mirrors how the cover sidecars key on
/// their source URL rather than the fetched bytes ("the hash tracks the source").
pub fn synced_lrc_source_hash(clip_id: &str) -> String {
    content_hash(&format!("synced-lrc/v{SYNCED_LRC_VERSION}/{clip_id}"))
}

/// A sentinel for the embedded cover art: a digest of the selected art URL, or
/// the empty string when the clip carries no art. A mismatch against the
/// manifest means the file on disk holds stale art even if its tags are current.
pub fn art_hash(clip: &Clip) -> String {
    art_url_hash(clip.selected_image_url().unwrap_or(""))
}

/// The embedded-cover sentinel that accounts for the animated-WebP embed.
///
/// When `embed_animated` is set and the clip has a `video_cover_url`, the audio
/// file embeds a bounded animated WebP derived from that source, so its identity
/// is the source URL, the encode `settings`, AND the static image URL. The
/// static URL is folded in because the WebP falls back to that static JPEG when
/// it will not fit the container, so a later change to the static art must still
/// re-tag a fallen-back file. This hashes the embed *intent*, not the runtime
/// fit outcome: `build_desired` and dry-run are pure and cannot know the encoded
/// size, so a fit fallback must not churn — the stored and desired hashes still
/// match on the next run, while a settings or source change re-embeds.
///
/// Otherwise (feature off, no video preview, or an ALAC target, which cannot
/// embed WebP) it is the plain static [`art_hash`], so a non-animated clip is
/// unaffected by this feature.
///
/// Gating the WebP-intent branch on `video_cover_url` presence is deliberate: it
/// scopes an enable-time re-tag to exactly the clips that have a preview, rather
/// than rewriting the whole library (a toggle-only hash would). The trade-off is
/// that if the feed were to omit a clip's `video_cover_url` for a run, the hash
/// would fall back to static and re-tag, then re-tag again when it reappears. In
/// practice a preview is an immutable generated asset and the v3 feed returns
/// complete clips, so this does not flap; and it is bounded churn (a tag
/// rewrite), never data loss, since the audio stream is preserved.
pub fn embedded_art_hash(
    clip: &Clip,
    embed_animated: bool,
    settings: &WebpEncodeSettings,
) -> String {
    if embed_animated && !clip.video_cover_url.is_empty() {
        let mut hasher = fnv::FnvHasher::default();
        hasher.write(b"webp-embed\0");
        hasher.write(clip.video_cover_url.as_bytes());
        hasher.write_u8(0);
        hasher.write(clip.selected_image_url().unwrap_or("").as_bytes());
        hasher.write_u8(0);
        hasher.write_u8(settings.quality);
        hasher.write_u8(u8::from(settings.lossless));
        hasher.write_u8(settings.compression_level);
        hasher.write_u32(settings.max_fps);
        match settings.max_width {
            Some(width) => {
                hasher.write_u8(1);
                hasher.write_u32(width);
            }
            None => hasher.write_u8(0),
        }
        format!("{:016x}", hasher.finish())
    } else {
        art_hash(clip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lineage::{EdgeType, ResolveStatus};

    fn sample() -> Clip {
        Clip {
            title: "Electric Storm".to_owned(),
            tags: "ambient, cinematic".to_owned(),
            image_large_url: "https://cdn1.suno.ai/image_large_abc.jpeg".to_owned(),
            image_url: "https://cdn1.suno.ai/image_abc.jpeg".to_owned(),
            video_cover_url: String::new(),
            prompt: "an orchestral storm".to_owned(),
            lyrics: "thunder rolls\nover the plains".to_owned(),
            gpt_description_prompt: "stormy".to_owned(),
            handle: "alice".to_owned(),
            display_name: "Alice".to_owned(),
            ..Default::default()
        }
    }

    /// The resolved lineage embedded alongside [`sample`]: an extension of a
    /// parent under the "Weather Series" root, created in 2023.
    fn sample_lineage() -> LineageContext {
        LineageContext {
            root_id: "root-1".to_owned(),
            root_title: "Weather Series".to_owned(),
            root_date: "2023-05-01T00:00:00Z".to_owned(),
            parent_id: "parent-1".to_owned(),
            edge_type: Some(EdgeType::Extend),
            status: ResolveStatus::Resolved,
        }
    }

    #[test]
    fn meta_hash_is_stable() {
        // Golden value: a change here means the sentinel encoding changed and
        // every existing manifest would see a spurious retag. Change with care.
        let h = meta_hash(&sample(), &sample_lineage());
        assert_eq!(h, "c247d31f60378b86");
        assert_eq!(h.len(), 16);
        assert_eq!(h, meta_hash(&sample(), &sample_lineage()));
    }

    #[test]
    fn art_hash_is_stable_and_empty_without_art() {
        let h = art_hash(&sample());
        assert_eq!(h.len(), 16);
        assert_eq!(h, art_hash(&sample()));

        let mut bare = sample();
        bare.image_large_url = String::new();
        bare.image_url = String::new();
        bare.video_cover_url = String::new();
        assert_eq!(art_hash(&bare), "");
    }

    #[test]
    fn art_url_hash_is_stable_and_empty_for_empty_url() {
        assert_eq!(art_url_hash(""), "");
        let h = art_url_hash("https://cdn1.suno.ai/video_cover.mp4");
        assert_eq!(h.len(), 16);
        assert_eq!(h, art_url_hash("https://cdn1.suno.ai/video_cover.mp4"));
        assert_ne!(h, art_url_hash("https://cdn1.suno.ai/other.mp4"));
        // art_hash routes the selected image URL through the same helper.
        assert_eq!(
            art_hash(&sample()),
            art_url_hash(sample().selected_image_url().unwrap())
        );
    }

    #[test]
    fn webp_art_hash_tracks_url_and_every_encode_setting() {
        use crate::vocab::WebpEncodeSettings;
        let url = "https://cdn1.suno.ai/video_cover.mp4";
        let base = WebpEncodeSettings::default();
        let h = webp_art_hash(url, &base);
        assert_eq!(h.len(), 16);
        assert_eq!(h, webp_art_hash(url, &base), "stable for the same inputs");
        // Empty URL is the "no artifact" sentinel regardless of settings.
        assert_eq!(webp_art_hash("", &base), "");
        // A changed source URL re-transcodes.
        assert_ne!(h, webp_art_hash("https://cdn1.suno.ai/other.mp4", &base));
        // Every setting that shapes the output bytes moves the hash, so a
        // settings change re-encodes an existing cover.
        for mutate in [
            |s: &mut WebpEncodeSettings| s.quality = s.quality.wrapping_sub(1),
            |s: &mut WebpEncodeSettings| s.lossless = !s.lossless,
            |s: &mut WebpEncodeSettings| s.compression_level = s.compression_level.wrapping_add(1),
            |s: &mut WebpEncodeSettings| s.max_fps = s.max_fps.wrapping_add(1),
            |s: &mut WebpEncodeSettings| s.max_width = None,
        ] {
            let mut settings = base;
            mutate(&mut settings);
            assert_ne!(h, webp_art_hash(url, &settings));
        }
    }

    #[test]
    fn meta_hash_tracks_the_artist_and_model_but_not_sidecar_only_fields() {
        let lineage = sample_lineage();
        let base = meta_hash(&sample(), &lineage);
        // The artist (`display_name`) and model label are embedded tags, so an
        // upstream change to either must retag (#135) -- the old hand-listed
        // hash omitted both, leaving stale tags on re-sync.
        let mut artist = sample();
        artist.display_name = "Someone Else".to_owned();
        assert_ne!(meta_hash(&artist, &lineage), base);
        let mut model = sample();
        model.model_name = "chirp-v9".to_owned();
        assert_ne!(meta_hash(&model, &lineage), base);
        // The animated-cover URL is a sidecar source, not an audio tag: it has
        // its own hash and must not force a needless audio retag (#136).
        let mut cover = sample();
        cover.video_cover_url = "https://cdn1.suno.ai/new_cover.mp4".to_owned();
        assert_eq!(meta_hash(&cover, &lineage), base);
    }

    #[test]
    fn meta_hash_changes_when_a_content_field_changes() {
        let lineage = sample_lineage();
        let base = meta_hash(&sample(), &lineage);
        // Clip-side content fields. (Art lives in `art_hash`, not here.)
        for mutate in [
            |c: &mut Clip| c.title = "Different".to_owned(),
            |c: &mut Clip| c.tags = "lofi".to_owned(),
            |c: &mut Clip| c.handle = "bob".to_owned(),
            |c: &mut Clip| c.lyrics = "new words".to_owned(),
        ] {
            let mut clip = sample();
            mutate(&mut clip);
            assert_ne!(meta_hash(&clip, &lineage), base);
        }
        // Resolved-lineage values that get embedded must also move the hash.
        for mutate in [
            |l: &mut LineageContext| l.parent_id = "other-parent".to_owned(),
            |l: &mut LineageContext| l.root_id = "other-root".to_owned(),
            |l: &mut LineageContext| l.root_title = "Other Album".to_owned(),
            |l: &mut LineageContext| l.edge_type = Some(EdgeType::Cover),
            |l: &mut LineageContext| l.root_date = "2099-01-01T00:00:00Z".to_owned(),
        ] {
            let mut lin = sample_lineage();
            mutate(&mut lin);
            assert_ne!(meta_hash(&sample(), &lin), base);
        }
    }

    #[test]
    fn art_hash_tracks_the_selected_url_in_preference_order() {
        let mut clip = sample();
        let large = art_hash(&clip);
        clip.image_large_url = String::new();
        let standard = art_hash(&clip);
        assert_ne!(large, standard);
        clip.image_url = String::new();
        clip.video_cover_url = "https://cdn1.suno.ai/video_cover.jpeg".to_owned();
        let video = art_hash(&clip);
        assert_ne!(standard, video);
    }

    #[test]
    fn content_hash_is_stable_and_tracks_any_change() {
        let text = "#EXTM3U\n#PLAYLIST:Mix\n#EXTINF:60,One\nA/One.flac\n";
        let h = content_hash(text);
        assert_eq!(h.len(), 16);
        assert_eq!(h, content_hash(text), "same text hashes the same");
        // A different name, order, path, title, or duration changes the digest.
        assert_ne!(
            h,
            content_hash("#EXTM3U\n#PLAYLIST:Other\n#EXTINF:60,One\nA/One.flac\n")
        );
        assert_ne!(
            h,
            content_hash("#EXTM3U\n#PLAYLIST:Mix\n#EXTINF:61,One\nA/One.flac\n")
        );
    }

    #[test]
    fn synced_lrc_source_hash_is_stable_per_clip_and_never_empty() {
        let a = synced_lrc_source_hash("clip-a");
        assert_eq!(a.len(), 16);
        assert_eq!(a, synced_lrc_source_hash("clip-a"), "stable per clip id");
        // Distinct clips get distinct sentinels; none is the empty ("absent")
        // value, so a desired synced `.lrc` is never mistaken for "no artifact".
        assert_ne!(a, synced_lrc_source_hash("clip-b"));
        assert!(!a.is_empty());
    }
}
