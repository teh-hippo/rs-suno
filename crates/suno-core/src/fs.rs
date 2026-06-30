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

/// A filesystem failure, carrying a human-readable, secret-free reason.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct FsError(pub String);

impl FsError {
    /// Build an [`FsError`] from any displayable cause.
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
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

    /// Read the whole file at `path`.
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError>;

    /// Probe `path`, returning its [`FileStat`] or `None` when it is absent.
    fn metadata(&self, path: &str) -> Option<FileStat>;
}
