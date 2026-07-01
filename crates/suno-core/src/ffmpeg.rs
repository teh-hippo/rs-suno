//! The ffmpeg port: transcode WAV bytes to FLAC bytes, and MP4 video previews
//! to animated WebP cover bytes.
//!
//! The lossless download path renders a clip to WAV, then re-encodes it to
//! FLAC. The animated-cover path fetches a clip's MP4 preview and re-encodes it
//! to a small looping WebP. Both are the engine's only calls into ffmpeg, so
//! they sit behind this trait: the CLI adapter wraps a child process (with a
//! hard timeout), while tests use a stub that returns canned bytes. The steps
//! only re-encode media; tagging is the pure tagger's job.

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

/// Encoder settings for the animated WebP cover derived from a clip's MP4
/// preview.
///
/// The [`Default`] targets a small, broadly compatible file: a couple of
/// megabytes, well under the 25 MB ceiling some players (e.g. Symfonium) place
/// on embedded/sidecar art. A single hardcoded default is used this phase behind
/// one `--animated-covers` toggle; per-knob tuning is deliberately not surfaced
/// on the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebpEncodeSettings {
    /// Lossy encoder quality, 0-100 (higher is better and larger). Ignored when
    /// `lossless` is set.
    pub quality: u8,
    /// Cap on the output frame rate; a faster source is downsampled to this.
    pub max_fps: u32,
    /// Cap on the output width in pixels; a wider source scales down keeping its
    /// aspect ratio, and a narrower one is never upscaled.
    pub max_width: u32,
    /// Encode losslessly (much larger); off by default.
    pub lossless: bool,
    /// Spend extra effort compressing (smaller file, slower encode); on by
    /// default.
    pub compression: bool,
}

impl Default for WebpEncodeSettings {
    fn default() -> Self {
        Self {
            quality: 70,
            max_fps: 24,
            max_width: 720,
            lossless: false,
            compression: true,
        }
    }
}

/// The ffmpeg port the executor transcodes through.
///
/// Async so the adapter can offload the blocking child process without stalling
/// the runtime; tests resolve immediately.
pub trait Ffmpeg {
    /// Transcode `wav` to FLAC bytes.
    fn wav_to_flac(&self, wav: &[u8]) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send;

    /// Transcode an MP4 video preview to animated WebP bytes under `settings`.
    ///
    /// Used to derive a clip's `cover.webp` sidecar from its `video_cover_url`
    /// MP4. Like [`wav_to_flac`](Ffmpeg::wav_to_flac) the adapter offloads the
    /// blocking child process; tests resolve immediately.
    fn mp4_to_webp(
        &self,
        mp4: &[u8],
        settings: WebpEncodeSettings,
    ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send;
}
