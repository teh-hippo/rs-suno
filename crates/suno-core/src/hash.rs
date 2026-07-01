//! Stable content sentinels for change detection.
//!
//! Reconcile compares a clip's current [`meta_hash`]/[`art_hash`] against the
//! manifest to decide whether a file needs re-tagging. The hashes must be stable
//! across runs, versions, and platforms, so they use FNV-1a over a fixed field
//! encoding rather than the standard library's deliberately unspecified hasher.
//!
//! The field choices mirror the reference integration (ha-suno `clip_meta_hash`
//! and `image_url_hash`): they capture everything that affects file *content*,
//! and deliberately exclude path-affecting fields like `display_name`, since a
//! path change is detected as a rename, not a retag.

use std::hash::Hasher;

use crate::lineage::{EdgeType, LineageContext};
use crate::model::Clip;

/// A short, stable hex digest of `bytes` (FNV-1a, 64-bit).
fn digest(bytes: &[u8]) -> String {
    let mut hasher = fnv::FnvHasher::default();
    hasher.write(bytes);
    format!("{:016x}", hasher.finish())
}

/// A sentinel for the clip's tag-bearing metadata and chosen art.
///
/// Covers every field that affects file *content* — title, tags, the selected
/// art URL, video cover, the prompt and description, the account handle, and the
/// *resolved* lineage that gets embedded (immediate parent and edge, root id and
/// title, and the album the clip folders under) — so a change to any of them is
/// detected as a needed retag. This takes the resolved [`LineageContext`] rather
/// than the raw feed fields precisely because those resolved values are what end
/// up in the file (HARDENING B1: if a value is embedded, it is in the change
/// hash), so a retitle, re-point, or album move triggers a retag.
///
/// Path-affecting fields such as `display_name` are excluded on purpose: a path
/// change is a rename, detected by comparing the rendered path with the stored
/// one. `title` is included so a title change triggers both a rename and a
/// retag.
pub fn meta_hash(clip: &Clip, lineage: &LineageContext) -> String {
    let edge_label = lineage.edge_type.map(EdgeType::label).unwrap_or("");
    let fields = format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        clip.title,
        clip.tags,
        clip.selected_image_url().unwrap_or(""),
        clip.video_cover_url,
        lineage.parent_id,
        edge_label,
        lineage.root_id,
        lineage.root_title,
        lineage.album(&clip.title),
        clip.prompt,
        clip.gpt_description_prompt,
        clip.handle,
    );
    digest(fields.as_bytes())
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

/// A sentinel for the embedded cover art: a digest of the selected art URL, or
/// the empty string when the clip carries no art. A mismatch against the
/// manifest means the file on disk holds stale art even if its tags are current.
pub fn art_hash(clip: &Clip) -> String {
    art_url_hash(clip.selected_image_url().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lineage::ResolveStatus;

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
            gpt_description_prompt: "stormy".to_owned(),
            handle: "alice".to_owned(),
            display_name: "Alice".to_owned(),
            ..Default::default()
        }
    }

    /// The resolved lineage embedded alongside [`sample`]: an extension of a
    /// parent under the "Weather Series" root.
    fn sample_lineage() -> LineageContext {
        LineageContext {
            root_id: "root-1".to_owned(),
            root_title: "Weather Series".to_owned(),
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
        assert_eq!(h, "45ea84e9f71e604f");
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
    fn meta_hash_ignores_path_only_fields() {
        let lineage = sample_lineage();
        let mut other = sample();
        other.display_name = "Someone Else".to_owned();
        assert_eq!(meta_hash(&sample(), &lineage), meta_hash(&other, &lineage));
    }

    #[test]
    fn meta_hash_changes_when_a_content_field_changes() {
        let lineage = sample_lineage();
        let base = meta_hash(&sample(), &lineage);
        // Clip-side content fields.
        for mutate in [
            |c: &mut Clip| c.title = "Different".to_owned(),
            |c: &mut Clip| c.tags = "lofi".to_owned(),
            |c: &mut Clip| c.image_large_url = "https://cdn1.suno.ai/new.jpeg".to_owned(),
            |c: &mut Clip| c.handle = "bob".to_owned(),
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
}
