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

use crate::model::Clip;

/// The selected cover-art URL: large image, then image, then video cover. This
/// matches the executor's cover-fetch preference order, so the sentinel tracks
/// exactly the art the file is tagged with.
fn selected_image_url(clip: &Clip) -> &str {
    [
        clip.image_large_url.as_str(),
        clip.image_url.as_str(),
        clip.video_cover_url.as_str(),
    ]
    .into_iter()
    .find(|url| !url.is_empty())
    .unwrap_or("")
}

/// A short, stable hex digest of `bytes` (FNV-1a, 64-bit).
fn digest(bytes: &[u8]) -> String {
    let mut hasher = fnv::FnvHasher::default();
    hasher.write(bytes);
    format!("{:016x}", hasher.finish())
}

/// A sentinel for the clip's tag-bearing metadata and chosen art.
///
/// Covers every field that affects file *content* — title, tags, the selected
/// art URL, video cover, lineage, album, the prompt and description, and the
/// account handle — so a change to any of them is detected as a needed retag.
/// Path-affecting fields such as `display_name` are excluded on purpose: a path
/// change is a rename, detected by comparing the rendered path with the stored
/// one. `title` is included so a title change triggers both a rename and a
/// retag.
pub fn meta_hash(clip: &Clip) -> String {
    let fields = format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        clip.title,
        clip.tags,
        selected_image_url(clip),
        clip.video_cover_url,
        clip.root_ancestor_id,
        clip.lineage_status,
        clip.album_title,
        clip.prompt,
        clip.gpt_description_prompt,
        clip.handle,
    );
    digest(fields.as_bytes())
}

/// A sentinel for the embedded cover art: a digest of the selected art URL, or
/// the empty string when the clip carries no art. A mismatch against the
/// manifest means the file on disk holds stale art even if its tags are current.
pub fn art_hash(clip: &Clip) -> String {
    let url = selected_image_url(clip);
    if url.is_empty() {
        String::new()
    } else {
        digest(url.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn meta_hash_is_stable() {
        // Golden value: a change here means the sentinel encoding changed and
        // every existing manifest would see a spurious retag. Change with care.
        let h = meta_hash(&sample());
        assert_eq!(h, "e6816acf2f162bba");
        assert_eq!(h.len(), 16);
        assert_eq!(h, meta_hash(&sample()));
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
    fn meta_hash_ignores_path_only_fields() {
        let mut other = sample();
        other.display_name = "Someone Else".to_owned();
        assert_eq!(meta_hash(&sample()), meta_hash(&other));
    }

    #[test]
    fn meta_hash_changes_when_a_content_field_changes() {
        let base = meta_hash(&sample());
        for mutate in [
            |c: &mut Clip| c.title = "Different".to_owned(),
            |c: &mut Clip| c.tags = "lofi".to_owned(),
            |c: &mut Clip| c.album_title = "Other Album".to_owned(),
            |c: &mut Clip| c.image_large_url = "https://cdn1.suno.ai/new.jpeg".to_owned(),
            |c: &mut Clip| c.handle = "bob".to_owned(),
        ] {
            let mut clip = sample();
            mutate(&mut clip);
            assert_ne!(meta_hash(&clip), base);
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
