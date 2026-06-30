//! The ffmpeg port: transcode WAV bytes to FLAC bytes.
//!
//! The lossless download path renders a clip to WAV, then re-encodes it to
//! FLAC. The re-encode is the engine's only call into ffmpeg, so it sits behind
//! this trait: the CLI adapter wraps a child process (with a hard timeout),
//! while tests use a stub that returns canned FLAC bytes. The step only
//! re-encodes audio; tagging is the pure tagger's job.

use std::future::Future;

/// An ffmpeg transcode failure, carrying a human-readable, secret-free reason.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct FfmpegError(pub String);

impl FfmpegError {
    /// Build an [`FfmpegError`] from any displayable cause.
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

/// The ffmpeg port the executor transcodes through.
///
/// Async so the adapter can offload the blocking child process without stalling
/// the runtime; tests resolve immediately.
pub trait Ffmpeg {
    /// Transcode `wav` to FLAC bytes.
    fn wav_to_flac(&self, wav: &[u8]) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send;
}
