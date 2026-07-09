//! The ffmpeg adapter: the engine's [`Ffmpeg`] port realised with a child
//! process.
//!
//! The blocking transcodes in [`crate::transcode`] run on a blocking thread so
//! they never stall the async runtime. The FLAC path stages temporary files
//! under a scratch directory this adapter owns; the WebP path streams over
//! pipes and needs no scratch.

use std::future::Future;
use std::path::PathBuf;

use suno_core::{AudioFormat, Ffmpeg, FfmpegError, WebpEncodeSettings};

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
    fn wav_to_lossless(
        &self,
        wav: &[u8],
        format: AudioFormat,
    ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send {
        let scratch = self.scratch.clone();
        let wav = wav.to_vec();
        async move {
            if let Err(err) = std::fs::create_dir_all(&scratch) {
                return Err(crate::diskspace::ffmpeg_error(
                    &err,
                    format!("could not create scratch {}: {err}", scratch.display()),
                ));
            }
            tokio::task::spawn_blocking(move || {
                crate::transcode::wav_to_lossless(&wav, format, &scratch)
            })
            .await
            .map_err(|err| FfmpegError::new(format!("transcode task failed: {err}")))?
            // A full disk surfaces two ways here: the staged WAV write carries
            // a real io::Error, and a failure while ffmpeg writes the output
            // .flac is proven by transcode's scratch probe, which attaches a
            // real out-of-space io::Error. Both are classified as disk-full.
            .map_err(|err| crate::diskspace::ffmpeg_error_from_anyhow(&err, err.to_string()))
        }
    }

    fn mp4_to_webp(
        &self,
        mp4: &[u8],
        settings: WebpEncodeSettings,
    ) -> impl Future<Output = Result<Vec<u8>, FfmpegError>> + Send {
        let scratch = self.scratch.clone();
        let mp4 = mp4.to_vec();
        async move {
            if let Err(err) = std::fs::create_dir_all(&scratch) {
                return Err(crate::diskspace::ffmpeg_error(
                    &err,
                    format!("could not create scratch {}: {err}", scratch.display()),
                ));
            }
            tokio::task::spawn_blocking(move || {
                crate::transcode::mp4_to_webp(&mp4, settings, &scratch)
            })
            .await
            .map_err(|err| FfmpegError::new(format!("transcode task failed: {err}")))?
            // ffmpeg reports a full disk only as stderr text and the WebP streams
            // over a pipe, so transcode probes the scratch dir and attaches a real
            // out-of-space io::Error; a full disk also surfaces from the
            // create_dir_all above. Both are classified as disk-full so a cover
            // transcode aborts the run, like the audio path.
            .map_err(|err| crate::diskspace::ffmpeg_error_from_anyhow(&err, err.to_string()))
        }
    }
}
