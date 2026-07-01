//! The ffmpeg adapter: the engine's [`Ffmpeg`] port realised with a child
//! process.
//!
//! The blocking transcodes in [`crate::transcode`] run on a blocking thread so
//! they never stall the async runtime. The FLAC path stages temporary files
//! under a scratch directory this adapter owns; the WebP path streams over
//! pipes and needs no scratch.

use std::future::Future;
use std::path::PathBuf;

use suno_core::{Ffmpeg, FfmpegError, WebpEncodeSettings};

/// An ffmpeg transcoder staging temporary files under one scratch directory.
pub struct FfmpegAdapter {
    scratch: PathBuf,
}

impl FfmpegAdapter {
    /// Build an adapter that stages WAV and FLAC temporaries under `scratch`.
    pub fn new(scratch: impl Into<PathBuf>) -> Self {
        Self {
            scratch: scratch.into(),
        }
    }
}

impl Ffmpeg for FfmpegAdapter {
    fn wav_to_flac(&self, wav: &[u8]) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send {
        let scratch = self.scratch.clone();
        let wav = wav.to_vec();
        async move {
            if let Err(err) = std::fs::create_dir_all(&scratch) {
                return Err(FfmpegError::new(format!(
                    "could not create scratch {}: {err}",
                    scratch.display()
                )));
            }
            tokio::task::spawn_blocking(move || crate::transcode::wav_to_flac(&wav, &scratch))
                .await
                .map_err(|err| FfmpegError::new(format!("transcode task failed: {err}")))?
                .map_err(|err| FfmpegError::new(err.to_string()))
        }
    }

    fn mp4_to_webp(
        &self,
        mp4: &[u8],
        settings: WebpEncodeSettings,
    ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send {
        let mp4 = mp4.to_vec();
        async move {
            tokio::task::spawn_blocking(move || crate::transcode::mp4_to_webp(&mp4, settings))
                .await
                .map_err(|err| FfmpegError::new(format!("transcode task failed: {err}")))?
                .map_err(|err| FfmpegError::new(err.to_string()))
        }
    }
}
