//! Out-of-space detection for IO errors, shared by the disk and ffmpeg adapters
//! and the CLI's top-level error boundary.
//!
//! A full disk (or exhausted quota) is systemic: it will fail every remaining
//! clip, so the engine treats it as a run-ending abort rather than one more
//! skippable per-clip fault. These helpers recognise it portably.

use std::io::ErrorKind;

use suno_core::{FfmpegError, FsError};

/// The actionable sentence shown when a run stops because the disk is full,
/// shared by the CLI's top-level boundary and the sync/copy run path so the
/// wording stays identical.
pub const DISK_FULL_HINT: &str = "the destination disk is full; free space and re-run.";

/// Whether an [`io::Error`](std::io::Error) means the destination ran out of
/// space or quota.
///
/// [`ErrorKind::StorageFull`] is the portable check (a raw ENOSPC and the
/// Windows `ERROR_DISK_FULL` both map to it); [`ErrorKind::QuotaExceeded`]
/// covers EDQUOT.
pub fn is_out_of_space(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded
    )
}

/// Whether an [`anyhow::Error`] carries an out-of-space failure anywhere in its
/// source chain.
///
/// A raw [`io::Error`](std::io::Error) is matched by kind, but the typed ports
/// stringify their cause into a reason with no source, so an [`FsError`] or
/// [`FfmpegError`] is matched on its own out-of-space flag. Without this, a
/// `fetch` write that returns `anyhow(FsError::out_of_space(...))` would be
/// mistaken for a generic error and exit 1.
pub fn anyhow_is_out_of_space(err: &anyhow::Error) -> bool {
    err.chain().any(link_is_out_of_space)
}

/// Whether one error-chain link is an out-of-space failure, across the io and
/// typed-port error types.
fn link_is_out_of_space(err: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(io) = err.downcast_ref::<std::io::Error>() {
        return is_out_of_space(io);
    }
    if let Some(fs) = err.downcast_ref::<FsError>() {
        return fs.is_out_of_space();
    }
    if let Some(ff) = err.downcast_ref::<FfmpegError>() {
        return ff.is_out_of_space();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn enospc_and_quota_are_out_of_space() {
        assert!(is_out_of_space(&std::io::Error::from_raw_os_error(28)));
        assert!(is_out_of_space(&std::io::Error::from(
            ErrorKind::QuotaExceeded
        )));
    }

    #[test]
    fn a_generic_io_error_is_not_out_of_space() {
        assert!(!is_out_of_space(&std::io::Error::from(
            ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn anyhow_walks_the_chain_for_an_out_of_space_source() {
        let err = Err::<(), _>(std::io::Error::from_raw_os_error(28))
            .context("could not write scratch")
            .unwrap_err();
        assert!(anyhow_is_out_of_space(&err));
    }

    #[test]
    fn anyhow_without_an_out_of_space_source_is_false() {
        let err = Err::<(), _>(std::io::Error::from(ErrorKind::PermissionDenied))
            .context("could not write scratch")
            .unwrap_err();
        assert!(!anyhow_is_out_of_space(&err));
    }

    #[test]
    fn anyhow_detects_a_typed_out_of_space_fs_error() {
        let err = anyhow::Error::from(FsError::out_of_space("no space left to write x"));
        assert!(anyhow_is_out_of_space(&err));
    }

    #[test]
    fn anyhow_detects_a_typed_out_of_space_ffmpeg_error() {
        let err = anyhow::Error::from(FfmpegError::out_of_space("no space left to transcode"));
        assert!(anyhow_is_out_of_space(&err));
    }

    #[test]
    fn anyhow_ignores_a_generic_typed_error() {
        assert!(!anyhow_is_out_of_space(&anyhow::Error::from(FsError::new(
            "permission denied"
        ))));
        assert!(!anyhow_is_out_of_space(&anyhow::Error::from(
            FfmpegError::new("bad input")
        )));
    }
}
