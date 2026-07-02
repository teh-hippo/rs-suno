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

/// Why an ffmpeg transcode failed, so the executor can treat a full scratch
/// disk as a systemic abort rather than a skippable per-clip fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfmpegErrorKind {
    /// The scratch device or quota ran out of space.
    OutOfSpace,
    /// Any other failure (bad input, missing binary, encode error).
    Other,
}

/// An ffmpeg transcode failure, carrying a kind and a human-readable,
/// secret-free reason.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct FfmpegError {
    kind: FfmpegErrorKind,
    reason: String,
}

impl FfmpegError {
    /// Build an [`FfmpegError`] of kind [`FfmpegErrorKind::Other`] from any
    /// displayable cause.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            kind: FfmpegErrorKind::Other,
            reason: reason.into(),
        }
    }

    /// Build an out-of-space [`FfmpegError`] (kind [`FfmpegErrorKind::OutOfSpace`]).
    pub fn out_of_space(reason: impl Into<String>) -> Self {
        Self {
            kind: FfmpegErrorKind::OutOfSpace,
            reason: reason.into(),
        }
    }

    /// The failure kind.
    pub fn kind(&self) -> FfmpegErrorKind {
        self.kind
    }

    /// Whether this failure was a full scratch disk or exhausted quota.
    pub fn is_out_of_space(&self) -> bool {
        self.kind == FfmpegErrorKind::OutOfSpace
    }
}

/// Encoder settings for the animated WebP cover derived from a clip's MP4
/// preview.
///
/// The [`Default`] keeps the source resolution and frame rate and turns encoder
/// effort off. This trades a larger file (still comfortably under the 25 MB
/// ceiling some players such as Symfonium place on embedded/sidecar art) for an
/// encode that stays well under the ffmpeg timeout, since full effort can take
/// minutes on a full-resolution clip. A single hardcoded default is used this
/// phase behind one `--animated-covers` toggle; per-knob tuning is deliberately
/// not surfaced on the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebpEncodeSettings {
    /// Lossy encoder quality, 0-100 (higher is better and larger). Ignored when
    /// `lossless` is set.
    pub quality: u8,
    /// Cap on the output frame rate; a faster source is downsampled to this.
    pub max_fps: u32,
    /// Optional cap on the output width in pixels: `Some(w)` scales a wider
    /// source down keeping its aspect ratio (never upscaling), while `None`
    /// keeps the source resolution.
    pub max_width: Option<u32>,
    /// Encode losslessly (much larger); off by default.
    pub lossless: bool,
    /// Spend extra encoder effort: a smaller file, but far slower. Off by
    /// default, because `libwebp_anim` at full effort can take minutes on a
    /// full-resolution clip and exceed the transcode timeout.
    pub compression: bool,
}

impl Default for WebpEncodeSettings {
    fn default() -> Self {
        Self {
            quality: 70,
            max_fps: 24,
            max_width: None,
            lossless: false,
            compression: false,
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
