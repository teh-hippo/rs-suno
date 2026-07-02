//! Disk and CDN helpers for the `fetch` command: public downloads, cover-art
//! selection, and atomic file writes into the `downloads/` directory.

use std::fs::OpenOptions;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use suno_core::{Clip, Http, HttpRequest};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Download a public resource (CDN audio, rendered WAV, or cover art).
///
/// These URLs are unauthenticated, so the request carries no token; it reuses
/// the engine's [`Http`] port for a single GET.
pub async fn get_bytes(http: &impl Http, url: &str) -> Result<Vec<u8>> {
    let response = http
        .send(HttpRequest::get(url))
        .await
        .map_err(|err| anyhow::anyhow!("request failed: {err}"))?;
    if !(200..=299).contains(&response.status) {
        bail!("download failed for {url}: status {}", response.status);
    }
    Ok(response.body)
}

/// Download the clip's cover art, returning `None` if unavailable (non-fatal).
pub async fn cover(http: &impl Http, clip: &Clip) -> Option<Vec<u8>> {
    let url = clip.selected_image_url()?;
    get_bytes(http, url).await.ok()
}

/// Write `bytes` to `path` atomically via a temporary file and rename.
///
/// The temp name is process-unique so two concurrent writers never race on it,
/// and a drop guard removes it if writing or the final rename fails.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_impl(path, bytes, false)
}

/// Write `bytes` to `path` atomically via a temporary file and rename.
///
/// On Unix the temporary file is created with private (`0600`) permissions. That
/// mode is not applied on non-Unix platforms, where the private flag is ignored.
pub fn write_atomic_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_impl(path, bytes, true)
}

fn write_atomic_impl(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    write_atomic_with(path, bytes, private, replace)
}

fn write_atomic_with<F>(
    path: &Path,
    bytes: &[u8],
    private: bool,
    replace_fn: F,
) -> std::io::Result<()>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let tmp = temp_sibling(path);
    let _scratch = Scratch(tmp.clone());
    write_temp_file(&tmp, bytes, private)?;
    replace_fn(&tmp, path)?;
    Ok(())
}

#[cfg(unix)]
fn write_temp_file(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    if private {
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_temp_file(path: &Path, bytes: &[u8], _private: bool) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

/// Apply `mode` to `path`. If hardening fails, remove the file only when its
/// current permissions are looser than `mode`; a file already at least as
/// restrictive as `mode` is kept so a transient chmod failure never discards it.
#[cfg(unix)]
pub fn set_permissions_or_remove(path: &Path, mode: u32) -> std::io::Result<()> {
    set_permissions_or_remove_with(path, mode, |path| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    })
}

#[cfg(not(unix))]
pub fn set_permissions_or_remove(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_permissions_or_remove_with<F>(
    path: &Path,
    mode: u32,
    set_permissions: F,
) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    let Err(err) = set_permissions(path) else {
        return Ok(());
    };
    // Hardening failed. Keep the file when its permissions are already at least
    // as restrictive as the target (they grant nothing beyond `mode`); only
    // remove it when leaving it could expose it more widely than intended.
    if let Ok(meta) = std::fs::metadata(path) {
        let current = meta.permissions().mode() & 0o777;
        if current & !(mode & 0o777) == 0 {
            return Ok(());
        }
    }
    match std::fs::remove_file(path) {
        Ok(()) => Err(err),
        Err(remove_err) => Err(std::io::Error::new(
            err.kind(),
            format!(
                "{err}; also could not remove insecure file {}: {remove_err}",
                path.display()
            ),
        )),
    }
}

/// Rename `from` onto `to`, replacing any existing destination without ever
/// leaving `to` missing.
///
/// `std::fs::rename` overwrites atomically on Unix but fails on Windows when the
/// destination exists. The fallback first stashes the current destination aside,
/// swaps in the new file, and only drops the stash once the swap succeeds; a
/// failed swap restores the stash, so a valid file always sits at `to`.
pub(crate) fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) if to.exists() => {
            let backup = to.with_file_name(format!(
                ".{}.{}.bak",
                to.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default(),
                unique_stamp()
            ));
            std::fs::rename(to, &backup)?;
            match std::fs::rename(from, to) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&backup);
                    Ok(())
                }
                Err(err) => {
                    let _ = std::fs::rename(&backup, to);
                    Err(err)
                }
            }
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

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

    #[test]
    fn replace_overwrites_existing_and_leaves_no_backup() {
        let dir = Path::new("target").join(format!("replace-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        let to = dir.join("dest.bin");
        let from = dir.join("src.bin");
        std::fs::write(&to, b"old").unwrap();
        std::fs::write(&from, b"new").unwrap();

        replace(&from, &to).unwrap();

        assert_eq!(std::fs::read(&to).unwrap(), b"new");
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["dest.bin".to_owned()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_private_uses_owner_only_permissions() {
        let dir = Path::new("target").join(format!("write-atomic-private-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.bin");

        write_atomic_private(&path, b"secret").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["secret.bin".to_owned()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn set_permissions_or_remove_cleans_up_on_failure() {
        let dir = Path::new("target").join(format!("write-atomic-cleanup-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.bin");
        std::fs::write(&path, b"secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = set_permissions_or_remove_with(&path, 0o600, |_path| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "no chmod",
            ))
        })
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn set_permissions_or_remove_keeps_already_restrictive_file() {
        let dir = Path::new("target").join(format!("write-atomic-keep-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.bin");
        std::fs::write(&path, b"secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        set_permissions_or_remove_with(&path, 0o600, |_path| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "no chmod",
            ))
        })
        .unwrap();

        assert!(path.exists());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_private_cleans_up_temp_on_rename_failure() {
        let dir = Path::new("target").join(format!("write-atomic-private-fail-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.bin");
        assert!(
            write_atomic_with(&path, b"secret", true, |_tmp, _path| {
                Err(std::io::Error::other("rename failed"))
            })
            .is_err()
        );

        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.is_empty());
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
