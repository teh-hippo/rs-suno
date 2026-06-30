//! Disk and CDN helpers for the `fetch` command: public downloads, cover-art
//! selection, and atomic file writes into the `downloads/` directory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use suno_core::{Clip, Http, HttpRequest, Method};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Download a public resource (CDN audio, rendered WAV, or cover art).
///
/// These URLs are unauthenticated, so the request carries no token; it reuses
/// the engine's [`Http`] port for a single GET.
pub async fn get_bytes(http: &impl Http, url: &str) -> Result<Vec<u8>> {
    let response = http
        .send(HttpRequest {
            method: Method::Get,
            url: url.to_owned(),
            headers: Vec::new(),
        })
        .await
        .map_err(|err| anyhow::anyhow!("request failed: {err}"))?;
    if !(200..=299).contains(&response.status) {
        bail!("download failed for {url}: status {}", response.status);
    }
    Ok(response.body)
}

/// Download the clip's cover art, returning `None` if unavailable (non-fatal).
pub async fn cover(http: &impl Http, clip: &Clip) -> Option<Vec<u8>> {
    let url = cover_url(clip)?;
    get_bytes(http, url).await.ok()
}

/// The preferred cover-art URL: the large image, then the standard image, then
/// the video cover (mirrors ha-suno's `selected_image_url` order).
fn cover_url(clip: &Clip) -> Option<&str> {
    [
        clip.image_large_url.as_str(),
        clip.image_url.as_str(),
        clip.video_cover_url.as_str(),
    ]
    .into_iter()
    .find(|url| !url.is_empty())
}

/// Write `bytes` to `path` atomically via a temporary file and rename.
///
/// The temp name is process-unique so two concurrent writers never race on it,
/// and a drop guard removes it if writing or the final rename fails.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_sibling(path);
    let _scratch = Scratch(tmp.clone());
    std::fs::write(&tmp, bytes).with_context(|| format!("could not write {}", tmp.display()))?;
    replace(&tmp, path).with_context(|| format!("could not finalise {}", path.display()))?;
    Ok(())
}

/// Rename `from` onto `to`, replacing any existing destination.
///
/// `std::fs::rename` overwrites atomically on Unix but fails on Windows when the
/// destination exists, so fall back to removing it and renaming again. The first
/// attempt keeps the Unix path atomic.
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) if to.exists() => {
            std::fs::remove_file(to)?;
            std::fs::rename(from, to)
        }
        Err(err) => Err(err),
    }
}

/// A hidden, same-directory temporary path so the rename stays on one device.
fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_owned());
    path.with_file_name(format!(".{name}.{}.part", unique_stamp()))
}

/// A process- and call-unique stamp for temporary file names.
fn unique_stamp() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{seq}", std::process::id())
}

/// Removes its temporary path when dropped, even on the error path.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art_clip(image_large: &str, image: &str, video_cover: &str) -> Clip {
        Clip {
            image_large_url: image_large.to_owned(),
            image_url: image.to_owned(),
            video_cover_url: video_cover.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn cover_url_prefers_the_large_image() {
        let clip = art_clip("large", "standard", "video");
        assert_eq!(cover_url(&clip), Some("large"));
    }

    #[test]
    fn cover_url_falls_back_to_the_standard_image() {
        let clip = art_clip("", "standard", "video");
        assert_eq!(cover_url(&clip), Some("standard"));
    }

    #[test]
    fn cover_url_falls_back_to_the_video_cover() {
        let clip = art_clip("", "", "video");
        assert_eq!(cover_url(&clip), Some("video"));
    }

    #[test]
    fn cover_url_is_none_without_any_art() {
        let clip = art_clip("", "", "");
        assert_eq!(cover_url(&clip), None);
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp() {
        let dir = Path::new("target").join(format!("write-atomic-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.bin");

        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"second");

        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["clip.bin".to_owned()]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
