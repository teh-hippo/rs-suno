//! The filesystem adapter: the engine's [`Filesystem`] port realised with
//! `std::fs`.
//!
//! The executor passes paths relative to an account root this adapter owns, so
//! it joins every path onto that root, creates parent directories on demand,
//! and writes through the same atomic temp-and-rename as the `fetch` slice. A
//! remove of an absent file succeeds, matching the port's idempotent contract.

use std::path::{Path, PathBuf};

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

    /// Resolve a plan-relative path against the account root.
    fn resolve(&self, path: &str) -> PathBuf {
        self.root.join(path)
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
        let full = self.resolve(path);
        Self::ensure_parent(&full)?;
        write_atomic(&full, bytes).map_err(|err| FsError::new(err.to_string()))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        let to_full = self.resolve(to);
        Self::ensure_parent(&to_full)?;
        replace(&self.resolve(from), &to_full).map_err(|err| FsError::new(err.to_string()))
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        match std::fs::remove_file(self.resolve(path)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(FsError::new(err.to_string())),
        }
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, FsError> {
        std::fs::read(self.resolve(path)).map_err(|err| FsError::new(err.to_string()))
    }

    fn metadata(&self, path: &str) -> Option<FileStat> {
        std::fs::metadata(self.resolve(path))
            .ok()
            .map(|meta| FileStat {
                exists: true,
                size: meta.len(),
            })
    }
}
