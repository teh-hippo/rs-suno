//! The filesystem adapter: the engine's [`Filesystem`] port realised with
//! `std::fs`.
//!
//! The executor passes paths relative to an account root this adapter owns, so
//! it joins every path onto that root, creates parent directories on demand,
//! and writes through the same atomic temp-and-rename as the `fetch` slice. A
//! remove of an absent file succeeds, matching the port's idempotent contract.

use std::path::{Component, Path, PathBuf};

use suno_core::{FileStat, Filesystem, FsError};

use crate::download::{replace, write_atomic};

/// Map an [`io::Error`](std::io::Error) from a write or rename to an [`FsError`],
/// tagging a full disk or exhausted quota so the engine aborts the run.
fn classify_fs(err: &std::io::Error) -> FsError {
    if crate::diskspace::is_out_of_space(err) {
        FsError::out_of_space(err.to_string())
    } else {
        FsError::new(err.to_string())
    }
}

/// A `std::fs` filesystem rooted at one account directory.
pub struct FsAdapter {
    root: PathBuf,
}

impl FsAdapter {
    /// Build an adapter whose relative paths resolve under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve a plan-relative path against the account root, rejecting anything
    /// that could escape it. Plans and manifests only ever carry library-relative
    /// paths, so an absolute path, a root/prefix component, or a `..` traversal
    /// signals corruption and must never reach a write, rename, or remove.
    fn resolve(&self, path: &str) -> Result<PathBuf, FsError> {
        let contained = !path.is_empty()
            && Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        if contained {
            Ok(self.root.join(path))
        } else {
            Err(FsError::new(format!(
                "refusing path outside the library root: {path}"
            )))
        }
    }

    /// Create the parent directory of `full` so a write or rename can land.
    fn ensure_parent(full: &Path) -> Result<(), FsError> {
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                if crate::diskspace::is_out_of_space(&err) {
                    FsError::out_of_space(format!("could not create {}: {err}", parent.display()))
                } else {
                    FsError::new(format!("could not create {}: {err}", parent.display()))
                }
            })?;
        }
        Ok(())
    }
}

impl Filesystem for FsAdapter {
    fn write_atomic(&self, path: &str, bytes: &[u8]) -> Result<(), FsError> {
        let full = self.resolve(path)?;
        Self::ensure_parent(&full)?;
        write_atomic(&full, bytes).map_err(|err| classify_fs(&err))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        let from_full = self.resolve(from)?;
        let to_full = self.resolve(to)?;
        Self::ensure_parent(&to_full)?;
        replace(&from_full, &to_full).map_err(|err| classify_fs(&err))
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        match std::fs::remove_file(self.resolve(path)?) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(FsError::new(err.to_string())),
        }
    }

    fn prune_empty_dirs(&self, root: &str) -> Result<(), FsError> {
        // The account root itself (empty or ".") is never resolved through the
        // traversal guard, which rejects an empty path; everything else is a
        // library-relative directory that must stay contained.
        let base = if root.is_empty() || root == "." {
            self.root.clone()
        } else {
            self.resolve(root)?
        };
        prune_dir(&base, true);
        Ok(())
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, FsError> {
        std::fs::read(self.resolve(path)?).map_err(|err| FsError::new(err.to_string()))
    }

    fn metadata(&self, path: &str) -> Option<FileStat> {
        std::fs::metadata(self.resolve(path).ok()?)
            .ok()
            .map(|meta| FileStat {
                exists: true,
                size: meta.len(),
            })
    }
}

/// Remove empty directories under `dir`, depth-first (post-order).
///
/// Children are pruned before their parent, so an emptied parent is caught in
/// the same pass once its last child is removed (inherently bottom-up). `dir`
/// itself is removed only when `!is_root`; the account root is always kept.
/// `remove_dir` succeeds only on a truly-empty directory, so any directory
/// still holding a file (hidden ones included) or a surviving subdirectory is
/// left intact; every failure is ignored, keeping the prune purely advisory.
fn prune_dir(dir: &Path, is_root: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune_dir(&path, false);
        }
    }
    if !is_root {
        let _ = std::fs::remove_dir(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = Path::new("target").join(format!("fs-adapter-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn accepts_a_nested_relative_path() {
        let root = temp_root();
        let adapter = FsAdapter::new(root.clone());

        adapter
            .write_atomic("artist/album/song.flac", b"x")
            .unwrap();
        assert_eq!(adapter.read("artist/album/song.flac").unwrap(), b"x");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_an_absolute_path() {
        let root = temp_root();
        let adapter = FsAdapter::new(root.clone());

        assert!(adapter.write_atomic("/etc/passwd", b"x").is_err());
        assert!(adapter.read("/etc/passwd").is_err());
        assert!(adapter.remove("/etc/passwd").is_err());
        assert!(adapter.metadata("/etc/passwd").is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_a_parent_traversal() {
        let root = temp_root();
        let adapter = FsAdapter::new(root.clone());

        assert!(adapter.write_atomic("../escape.flac", b"x").is_err());
        assert!(adapter.rename("ok.flac", "../escape.flac").is_err());
        assert!(adapter.remove("../../etc/passwd").is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_an_empty_path() {
        let root = temp_root();
        let adapter = FsAdapter::new(root.clone());

        assert!(adapter.write_atomic("", b"x").is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_removes_only_empty_dirs_and_keeps_the_root() {
        let root = temp_root();
        let adapter = FsAdapter::new(root.clone());

        // A kept branch holds a file; an empty branch nests only empty dirs; a
        // hidden-file branch must survive despite holding only a dotfile.
        adapter.write_atomic("keep/full/song.flac", b"x").unwrap();
        std::fs::create_dir_all(root.join("empty/leaf/deeper")).unwrap();
        std::fs::create_dir_all(root.join("hidden")).unwrap();
        std::fs::write(root.join("hidden/.suno-manifest.json"), b"{}").unwrap();

        adapter.prune_empty_dirs("").unwrap();

        // The whole empty subtree is gone, bottom-up.
        assert!(!root.join("empty").exists());
        assert!(!root.join("empty/leaf").exists());
        // A directory holding a file (even a hidden one) is untouched.
        assert!(root.join("keep/full/song.flac").exists());
        assert!(root.join("hidden/.suno-manifest.json").exists());
        // The account root itself is never removed.
        assert!(root.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_never_removes_the_named_root_even_when_empty() {
        let root = temp_root();
        let adapter = FsAdapter::new(root.clone());
        std::fs::create_dir_all(root.join("album/leaf")).unwrap();

        // Pruning under "album" clears its empty child but keeps "album" itself.
        adapter.prune_empty_dirs("album").unwrap();

        assert!(root.join("album").exists());
        assert!(!root.join("album/leaf").exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
