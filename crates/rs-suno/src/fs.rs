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

    /// Reject a resolved path that escapes the account root through a symlinked
    /// ancestor.
    ///
    /// [`resolve`](Self::resolve) bars `..`, absolute, and prefix components
    /// lexically, but `std::fs` follows directory symlinks, so on a shared
    /// library a planted link (`ln -s /elsewhere "<root>/Artist"`) could still
    /// redirect a write, rename, or remove outside the root. The deepest
    /// existing ancestor of the target is canonicalised (resolving every
    /// symlink) and must stay within the canonical root before any mutating
    /// operation runs.
    ///
    /// The root is anchored on its own deepest existing ancestor rather than
    /// required to exist, so a first write into a not-yet-created library still
    /// works (`ensure_parent` creates the tree afterwards); a subtree that does
    /// not exist yet cannot hold a planted symlink, so this stays safe.
    ///
    /// This is a check-then-use guard: it blocks a symlink planted *before* the
    /// run (the practical shared-host attack), not one raced in between this
    /// check and the write itself. Fully closing that narrow window needs
    /// handle-relative, no-follow syscalls (`openat`/`O_NOFOLLOW`) that are not
    /// portable across the Linux and Windows targets, so it is out of scope.
    fn verify_contained(&self, full: &Path) -> Result<(), FsError> {
        let real_root = canonical_existing_ancestor(&self.root)
            .ok_or_else(|| FsError::new("could not resolve the library root".to_string()))?;
        let real_full = canonical_existing_ancestor(full).ok_or_else(|| {
            FsError::new(format!(
                "could not resolve an existing ancestor of {}",
                full.display()
            ))
        })?;
        if real_full.starts_with(&real_root) {
            Ok(())
        } else {
            Err(FsError::new(format!(
                "refusing path that escapes the library root through a symlink: {}",
                full.display()
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
        self.verify_contained(&full)?;
        Self::ensure_parent(&full)?;
        write_atomic(&full, bytes).map_err(|err| classify_fs(&err))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        let from_full = self.resolve(from)?;
        let to_full = self.resolve(to)?;
        self.verify_contained(&from_full)?;
        self.verify_contained(&to_full)?;
        Self::ensure_parent(&to_full)?;
        replace(&from_full, &to_full).map_err(|err| classify_fs(&err))
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        let full = self.resolve(path)?;
        self.verify_contained(&full)?;
        match std::fs::remove_file(full) {
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
        // Never begin the walk on a symlinked directory: a named base that is a
        // planted link would let `read_dir` escape the root even though the
        // per-entry file-type guard skips symlinked children (#249).
        self.verify_contained(&base)?;
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

/// The canonical form of the deepest existing ancestor of `path` (including
/// `path` itself when it exists), resolving every symlink along the way.
///
/// Returns [`None`] only when no ancestor exists at all. Used to check
/// containment before a component that does not exist yet is created.
fn canonical_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    loop {
        if let Ok(real) = ancestor.canonicalize() {
            return Some(real);
        }
        ancestor = ancestor.parent()?;
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
///
/// Symlinked entries are never descended into or removed: `DirEntry::file_type`
/// does not follow the link, so a planted directory symlink (whose target may
/// lie outside the root) is skipped and only real directories are pruned.
fn prune_dir(dir: &Path, is_root: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            prune_dir(&entry.path(), false);
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

    /// Create a directory symlink for tests, portably. Returns `false` when the
    /// platform refuses (Windows without the symlink privilege) so the caller
    /// can skip rather than fail: `symlink` on Unix, `symlink_dir` on Windows.
    fn try_symlink_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(target, link);
        made.is_ok()
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

    #[test]
    fn creates_a_missing_root_on_first_write() {
        // `fetch <id> <new-dir>` points the adapter at a directory that does not
        // exist yet; the containment guard must not require the root to already
        // exist, or the first write would fail (regression guard).
        let parent = temp_root();
        let root = parent.join("not-created-yet");
        assert!(!root.exists());
        let adapter = FsAdapter::new(root.clone());

        adapter.write_atomic("song.mp3", b"x").unwrap();
        assert_eq!(adapter.read("song.mp3").unwrap(), b"x");
        assert!(root.join("song.mp3").exists());

        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn rejects_a_write_through_a_symlinked_parent() {
        let root = temp_root();
        let outside = temp_root();
        std::fs::create_dir_all(&outside).unwrap();
        let adapter = FsAdapter::new(root.clone());
        // A local co-user plants "root/Artist" -> outside, aiming the write out.
        if !try_symlink_dir(
            &std::fs::canonicalize(&outside).unwrap(),
            &root.join("Artist"),
        ) {
            return;
        }

        assert!(adapter.write_atomic("Artist/song.flac", b"x").is_err());
        // Nothing landed in the symlink target.
        assert!(!outside.join("song.flac").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn rejects_a_remove_through_a_symlinked_parent() {
        let root = temp_root();
        let outside = temp_root();
        std::fs::create_dir_all(&outside).unwrap();
        // A victim file the mirror must never delete through the planted link.
        std::fs::write(outside.join("victim"), b"keep").unwrap();
        let adapter = FsAdapter::new(root.clone());
        if !try_symlink_dir(
            &std::fs::canonicalize(&outside).unwrap(),
            &root.join("Artist"),
        ) {
            return;
        }

        assert!(adapter.remove("Artist/victim").is_err());
        assert!(outside.join("victim").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn rejects_a_rename_target_through_a_symlinked_parent() {
        let root = temp_root();
        let outside = temp_root();
        std::fs::create_dir_all(&outside).unwrap();
        let adapter = FsAdapter::new(root.clone());
        adapter.write_atomic("from.flac", b"x").unwrap();
        if !try_symlink_dir(
            &std::fs::canonicalize(&outside).unwrap(),
            &root.join("Artist"),
        ) {
            return;
        }

        assert!(adapter.rename("from.flac", "Artist/to.flac").is_err());
        assert!(!outside.join("to.flac").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn prune_does_not_follow_a_symlink_out_of_the_root() {
        let root = temp_root();
        let outside = temp_root();
        // An empty directory outside the root that the prune must never reach.
        std::fs::create_dir_all(outside.join("victim_empty")).unwrap();
        let adapter = FsAdapter::new(root.clone());
        if !try_symlink_dir(
            &std::fs::canonicalize(&outside).unwrap(),
            &root.join("link"),
        ) {
            return;
        }

        adapter.prune_empty_dirs("").unwrap();

        // The symlink target's empty directory survives; the link is not walked.
        assert!(outside.join("victim_empty").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn prune_rejects_a_symlinked_named_root() {
        let root = temp_root();
        let outside = temp_root();
        std::fs::create_dir_all(outside.join("victim_empty")).unwrap();
        let adapter = FsAdapter::new(root.clone());
        // A named base that is itself a planted symlink must be refused, not
        // walked: read_dir(base) would otherwise escape the root.
        if !try_symlink_dir(
            &std::fs::canonicalize(&outside).unwrap(),
            &root.join("album"),
        ) {
            return;
        }

        assert!(adapter.prune_empty_dirs("album").is_err());
        assert!(outside.join("victim_empty").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
