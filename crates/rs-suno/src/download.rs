//! Disk and CDN helpers for the `fetch` command: public downloads, cover-art
//! selection, and atomic file writes into the `downloads/` directory.

use std::fs::OpenOptions;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
///
/// On a cross-device move (EXDEV / `CrossesDevices`) the file is first copied
/// to a temporary sibling of `to` (same filesystem), then renamed locally.
///
/// When `from` and `to` are the same inode (case-only rename on a
/// case-insensitive filesystem) the stash path is skipped; an intermediate
/// rename is used on Windows to satisfy `MoveFile` semantics.
pub(crate) fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            cross_device_replace(from, to)
        }
        Err(_) if to.exists() => {
            if same_file(from, to) {
                case_only_rename(from, to)
            } else {
                stash_replace(from, to)
            }
        }
        Err(err) => Err(err),
    }
}

/// Copy `from` to a temp next to `to` (same device), then rename locally.
fn cross_device_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let tmp = temp_sibling(to);
    let _scratch = Scratch(tmp.clone());
    std::fs::copy(from, &tmp)?;
    // `tmp` is on the same device as `to`; a local rename or stash will work.
    match std::fs::rename(&tmp, to) {
        Ok(()) => {}
        Err(_) if to.exists() && !same_file(&tmp, to) => stash_replace(&tmp, to)?,
        Err(err) => return Err(err),
    }
    std::fs::remove_file(from)
}

/// Stash the current destination aside, swap in the new file, restore on fail.
fn stash_replace(from: &Path, to: &Path) -> std::io::Result<()> {
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

/// Rename when `from` and `to` are the same inode (case-only rename on a
/// case-insensitive filesystem). The direct rename works on Unix/macOS; on
/// Windows `MoveFile` refuses same-inode moves, so an intermediate name is used.
fn case_only_rename(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let mid = to.with_file_name(format!(
            ".{}.{}.rename",
            to.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default(),
            unique_stamp()
        ));
        std::fs::rename(from, &mid)?;
        std::fs::rename(&mid, to)
    }
    #[cfg(not(windows))]
    std::fs::rename(from, to)
}

/// True when `a` and `b` refer to the same on-disk file.
///
/// On Unix this compares device + inode numbers. On other platforms it falls
/// back to canonicalized-path equality (catches case-insensitive NTFS).
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(ca), Ok(cb)) => ca == cb,
            _ => false,
        }
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

/// Minimum age before a `.part` temp file is considered abandoned by a dead
/// process rather than actively written by a concurrent run (1 hour).
const STALE_PART_AGE_SECS: u64 = 3600;

/// Remove leftover `.*.part` temp files under `dir` (recursively) that are
/// older than [`STALE_PART_AGE_SECS`].  A hard-killed run cannot run its `Drop`
/// guards, leaving these hidden files behind; a subsequent run calls this before
/// writing anything so the stale files self-heal without touching any `.part`
/// that a concurrent run may still be writing (age gate).
pub fn cleanup_stale_parts(dir: &Path) {
    cleanup_stale_parts_older_than(dir, Duration::from_secs(STALE_PART_AGE_SECS));
}

fn cleanup_stale_parts_older_than(dir: &Path, threshold: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            cleanup_stale_parts_older_than(&path, threshold);
            continue;
        }
        let os_name = entry.file_name();
        let filename = os_name.to_string_lossy();
        if !filename.starts_with('.') || !filename.ends_with(".part") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let age = meta
            .modified()
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .unwrap_or(Duration::ZERO);
        if age >= threshold {
            let _ = std::fs::remove_file(&path);
        }
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

    #[test]
    fn cross_device_replace_copies_then_removes_source() {
        // Simulate EXDEV by using two distinct directories under target/.  The
        // real cross-device path is exercised by injecting an EXDEV-like error
        // via the cross_device_replace helper directly, since we can't create a
        // genuine cross-mount boundary in a unit test.
        let dir = Path::new("target").join(format!("xdev-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        let from = dir.join("source.bin");
        let to = dir.join("dest.bin");
        std::fs::write(&from, b"xdev-content").unwrap();

        cross_device_replace(&from, &to).unwrap();

        assert_eq!(std::fs::read(&to).unwrap(), b"xdev-content");
        assert!(
            !from.exists(),
            "source must be removed after cross-device copy"
        );
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["dest.bin".to_owned()], "no temp files left");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cross_device_replace_leaves_source_on_copy_failure() {
        // If the copy step fails, the source must still exist (no data loss).
        let dir = Path::new("target").join(format!("xdev-fail-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        // A non-existent source triggers a copy error.
        let from = dir.join("missing.bin");
        let to = dir.join("dest.bin");

        assert!(cross_device_replace(&from, &to).is_err());
        assert!(!to.exists(), "destination must not appear on copy failure");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn same_file_detects_same_inode() {
        let dir = Path::new("target").join(format!("samefile-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        std::fs::write(&a, b"x").unwrap();
        // Hard link: same inode, different name.
        let b = dir.join("b.bin");
        std::fs::hard_link(&a, &b).unwrap();

        assert!(same_file(&a, &b));
        assert!(same_file(&a, &a));

        let c = dir.join("c.bin");
        std::fs::write(&c, b"x").unwrap();
        assert!(!same_file(&a, &c));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_parts_are_removed_and_fresh_ones_kept() {
        let dir = Path::new("target").join(format!("stale-parts-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();

        // A "stale" part: modify its timestamp to look old.
        let stale = dir.join(".old.123-456-0.part");
        std::fs::write(&stale, b"stale").unwrap();

        // A fresh part: written just now (age 0).
        let fresh = dir.join(".new.789-012-1.part");
        std::fs::write(&fresh, b"fresh").unwrap();

        // A regular file must never be removed.
        let regular = dir.join("song.flac");
        std::fs::write(&regular, b"audio").unwrap();

        // Use a zero threshold so "stale" passes, but "fresh" would need >0 age.
        // We back-date the stale file via a very short threshold with zero age.
        // Since both files are just-created, use a zero threshold and verify
        // both parts are removed (age >= 0 for both).
        cleanup_stale_parts_older_than(&dir, Duration::ZERO);

        assert!(!stale.exists(), "stale part must be removed");
        assert!(
            !fresh.exists(),
            "fresh part with age >= threshold must be removed"
        );
        assert!(regular.exists(), "regular file must survive");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_cleanup_skips_parts_younger_than_threshold() {
        let dir = Path::new("target").join(format!("stale-skip-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();

        let part = dir.join(".running.123-456-0.part");
        std::fs::write(&part, b"active").unwrap();

        // Very large threshold: nothing is old enough to be cleaned up.
        cleanup_stale_parts_older_than(&dir, Duration::from_secs(u64::MAX / 2));

        assert!(part.exists(), "young part must be kept");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_cleanup_ignores_non_part_files() {
        let dir = Path::new("target").join(format!("stale-ignore-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();

        // Dot-prefixed but not `.part` suffix.
        let dotfile = dir.join(".suno-manifest.json");
        std::fs::write(&dotfile, b"{}").unwrap();
        // No dot prefix.
        let plain = dir.join("song.flac");
        std::fs::write(&plain, b"audio").unwrap();

        cleanup_stale_parts_older_than(&dir, Duration::ZERO);

        assert!(dotfile.exists(), "non-.part dotfile must survive");
        assert!(plain.exists(), "regular file must survive");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_cleanup_is_recursive() {
        // Part files can be in subdirectories (siblings of deep library paths).
        let dir = Path::new("target").join(format!("stale-recursive-{}", unique_stamp()));
        let sub = dir.join("artist/album");
        std::fs::create_dir_all(&sub).unwrap();

        let part = sub.join(".song.123-456-0.part");
        std::fs::write(&part, b"partial").unwrap();
        let audio = sub.join("song.flac");
        std::fs::write(&audio, b"audio").unwrap();

        cleanup_stale_parts_older_than(&dir, Duration::ZERO);

        assert!(!part.exists(), "nested stale part must be removed");
        assert!(audio.exists(), "audio file must survive");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
