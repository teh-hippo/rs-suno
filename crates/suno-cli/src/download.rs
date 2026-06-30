//! Disk and CDN helpers for the `fetch` command: public downloads, cover-art
//! selection, and atomic file writes into the `downloads/` directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use suno_core::{Clip, Http, HttpRequest, Method};

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

/// The preferred cover-art URL: the large image, then the standard image.
fn cover_url(clip: &Clip) -> Option<&str> {
    [clip.image_large_url.as_str(), clip.image_url.as_str()]
        .into_iter()
        .find(|url| !url.is_empty())
}

/// Write `bytes` to `path` atomically via a temporary file and rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_sibling(path);
    std::fs::write(&tmp, bytes).with_context(|| format!("could not write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("could not finalise {}", path.display()))?;
    Ok(())
}

/// A hidden, same-directory temporary path so the rename stays on one device.
fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_owned());
    path.with_file_name(format!(".{name}.part"))
}
