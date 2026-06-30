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
                FsError::new(format!("could not create {}: {err}", parent.display()))
            })?;
        }
        Ok(())
    }
}

impl Filesystem for FsAdapter {
    fn write_atomic(&self, path: &str, bytes: &[u8]) -> Result<(), FsError> {
        let full = self.resolve(path)?;
        Self::ensure_parent(&full)?;
        write_atomic(&full, bytes).map_err(|err| FsError::new(err.to_string()))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        let from_full = self.resolve(from)?;
        let to_full = self.resolve(to)?;
        Self::ensure_parent(&to_full)?;
        replace(&from_full, &to_full).map_err(|err| FsError::new(err.to_string()))
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        match std::fs::remove_file(self.resolve(path)?) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(FsError::new(err.to_string())),
        }
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
}
