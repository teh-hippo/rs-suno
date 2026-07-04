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
            root_ancestor_id: "root-1".to_owned(),
            lineage_status: "continuation".to_owned(),
            album_title: "Weather Series".to_owned(),
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
