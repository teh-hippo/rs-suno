//! The filesystem port: the executor's only window to disk.
//!
//! The download executor never touches the disk directly. It writes, renames,
//! removes, reads, and probes files through this trait, which a CLI adapter
//! implements with `std::fs` (an atomic temp-and-rename write, a cross-platform
//! replace, and parent-directory creation). Tests use an in-memory double so
//! the executor's logic is exercised without real IO.
//!
//! Paths are relative to an account root the adapter owns; the executor only
//! ever passes the relative path a [`crate::Plan`] carries.

/// On-disk facts about one path, as probed by [`Filesystem::metadata`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileStat {
    /// Whether the file exists.
    pub exists: bool,
    /// Size of the file in bytes (zero when absent).
    pub size: u64,
}

/// Why a filesystem write failed, so the executor can treat a full disk as a
/// systemic abort rather than one more skippable per-clip fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsErrorKind {
    /// The device or quota ran out of space.
    OutOfSpace,
    /// Any other failure (permission, missing parent, corruption).
    Other,
}

/// A filesystem failure, carrying a kind and a human-readable, secret-free
/// reason.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct FsError {
    kind: FsErrorKind,
    reason: String,
}

impl FsError {
    /// Build an [`FsError`] of kind [`FsErrorKind::Other`] from any displayable
    /// cause.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            kind: FsErrorKind::Other,
            reason: reason.into(),
        }
    }

    /// Build an out-of-space [`FsError`] (kind [`FsErrorKind::OutOfSpace`]).
    pub fn out_of_space(reason: impl Into<String>) -> Self {
        Self {
            kind: FsErrorKind::OutOfSpace,
            reason: reason.into(),
        }
    }

    /// The failure kind.
    pub fn kind(&self) -> FsErrorKind {
        self.kind
    }

    /// Whether this failure was a full disk or exhausted quota.
    pub fn is_out_of_space(&self) -> bool {
        self.kind == FsErrorKind::OutOfSpace
    }
}

/// The disk port the executor writes the plan through.
///
/// Methods are synchronous: disk IO is fast and the adapter can offload it if
/// it must. Every method returns a [`Result`] so the engine never panics on an
/// IO fault; a write failure must leave any prior file intact (atomic write).
pub trait Filesystem {
    /// Write `bytes` to `path` atomically, replacing any existing file.
    ///
    /// On failure the prior file at `path` is left untouched: the adapter
    /// stages a temporary sibling and renames it into place only once the full
    /// contents are written, so a partial write can never be observed.
    fn write_atomic(&self, path: &str, bytes: &[u8]) -> Result<(), FsError>;

    /// Move `from` onto `to`, replacing any existing destination.
    fn rename(&self, from: &str, to: &str) -> Result<(), FsError>;

    /// Remove `path`. Succeeds when the file is already absent (idempotent).
    fn remove(&self, path: &str) -> Result<(), FsError>;

    /// Remove empty directories under `root`, bottom-up.
    ///
    /// After a rename/move or a delete empties an album directory, that now-dead
    /// directory is a ghost. This prunes it. The contract is strictly additive
    /// and safe:
    ///
    /// - it removes only directories that are genuinely empty, walking
    ///   depth-first so an emptied parent is pruned once its last child is;
    /// - it NEVER removes a directory holding any entry, including a hidden file
    ///   (a `.suno-manifest.json`, `.suno-lineage.json`, or `.m3u8`); and
    /// - it NEVER removes `root` itself, only directories strictly beneath it.
    ///
    /// `root` is a library-relative directory, with the empty string (or `"."`)
    /// meaning the account root. A prune failure is never fatal: the tool
    /// re-plans and retries on the next run, so this only ever tidies.
    fn prune_empty_dirs(&self, root: &str) -> Result<(), FsError>;

    /// Read the whole file at `path`.
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError>;

    /// Probe `path`, returning its [`FileStat`] or `None` when it is absent.
    fn metadata(&self, path: &str) -> Option<FileStat>;
}
