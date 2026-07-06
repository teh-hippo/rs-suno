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

use crate::config::AudioFormat;

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
/// The animated WebP is embedded as the audio file's front-cover picture. A
/// FLAC PICTURE block is length-prefixed with a 24-bit field, so a single
/// picture cannot exceed ~16 MiB; a real 5 s Suno cover at quality 95 with no
/// width cap is ~31 MiB and would never fit. The [`Default`] is therefore a
/// bounded lossy profile that reliably fits that ceiling: quality 90 at effort
/// (`compression_level`) 4, scaled to at most 640 px wide (owner measurement:
/// ~11 MiB, ~30% headroom under the cap; 800 px is ~14.5 MiB with far thinner
/// margin). Effort is capped at 4 because effort 6 only matches its size for
/// 7-13x the encode time. Lossless is opt-in and far larger (a 5 s cover is
/// ~145 MB), so it fits only the larger MP3/ALAC containers, never FLAC.
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
    /// Encode losslessly. Off by default: lossless animated WebP of real video
    /// is intrinsically huge (roughly 30x the lossy source) with no visible
    /// gain over quality 95 for a cover.
    pub lossless: bool,
    /// Encoder effort, 0-4 (higher is smaller and slower). Capped at 4 because
    /// effort 6 yields the same size for many times the encode time.
    pub compression_level: u8,
}

impl Default for WebpEncodeSettings {
    fn default() -> Self {
        Self {
            quality: 90,
            max_fps: 24,
            max_width: Some(640),
            lossless: false,
            compression_level: 4,
        }
    }
}

/// The ffmpeg port the executor transcodes through.
///
/// Async so the adapter can offload the blocking child process without stalling
/// the runtime; tests resolve immediately.
pub trait Ffmpeg {
    /// Transcode `wav` to the given lossless `format`'s bytes.
    fn wav_to_lossless(
        &self,
        wav: &[u8],
        format: AudioFormat,
    ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send;

    /// Transcode an MP4 video preview to animated WebP bytes under `settings`.
    ///
    /// Used to derive a clip's `cover.webp` sidecar from its `video_cover_url`
    /// MP4. Like [`wav_to_lossless`](Ffmpeg::wav_to_lossless) the adapter offloads
    /// the blocking child process; tests resolve immediately.
    fn mp4_to_webp(
        &self,
        mp4: &[u8],
        settings: WebpEncodeSettings,
    ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send;
}
