//! Shared vocabulary: the small, dependency-free types spoken across the crate.
//!
//! These enums and settings are named by many modules (`config`, `reconcile`,
//! `ffmpeg`, `executor`, `desired`, ...). Housing them in one leaf module keeps
//! them out of any heavy engine module, so naming a format or a source mode
//! never drags a dependency on the planner or the transcoder. The module depends
//! only on [`crate::error`] (for the `FromStr` impls) and so sits at the bottom
//! of the dependency graph.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// How a selected source treats its clips: mirror with deletion, or additive copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceMode {
    /// Mirror the source, deleting local files that leave it (rclone `sync`).
    Mirror,
    /// Copy additively; never delete (rclone `copy`).
    Copy,
}

/// The class of an external sidecar artifact a clip (or album/library) owns.
///
/// The reconcile engine keeps a single pair of artifact actions
/// (`Action::WriteArtifact` / `Action::DeleteArtifact`) rather than one variant
/// per class; the `kind` distinguishes them so the executor and the manifest can
/// route each to the right slot. Per-clip classes
/// ([`CoverJpg`](ArtifactKind::CoverJpg), [`CoverWebp`](ArtifactKind::CoverWebp),
/// [`DetailsTxt`](ArtifactKind::DetailsTxt), [`LyricsTxt`](ArtifactKind::LyricsTxt),
/// [`Lrc`](ArtifactKind::Lrc), and [`VideoMp4`](ArtifactKind::VideoMp4)) map to
/// a manifest entry field; the album/library classes are reconciled by later
/// phases and have no per-clip manifest slot yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtifactKind {
    /// The per-song external cover, sourced from `image_large_url`.
    CoverJpg,
    /// Retired: the per-song animated cover is now embedded in the audio file's
    /// front-cover picture, not written as a `<track>.webp` sidecar. The kind is
    /// kept so a `.webp` written by an older version stays tracked and is cleaned
    /// up (it is delete-eligible; see `removed_kind_delete_eligible` in
    /// `reconcile`); it is never emitted into a new desired set.
    CoverWebp,
    /// The per-song plain-text details dump (generated, inline content).
    DetailsTxt,
    /// The per-song plain-text lyrics file (generated, inline content).
    LyricsTxt,
    /// The per-song untimed `.lrc` lyrics file (generated, inline content).
    Lrc,
    /// The per-song standalone music video, fetched from `video_url` (off by
    /// default). A large binary, removed only alongside its own audio.
    VideoMp4,
    /// The album folder's static cover (album-scoped, later phase).
    FolderJpg,
    /// The album folder's animated cover (album-scoped, later phase).
    FolderWebp,
    /// The album folder's raw animated cover: the same `video_cover_url` as
    /// [`FolderWebp`](ArtifactKind::FolderWebp), kept verbatim with no transcode
    /// (album-scoped, later phase).
    FolderMp4,
    /// A library-root `.m3u8` playlist (library-scoped, later phase).
    Playlist,
}

/// Audio format for downloaded clips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Mp3,
    #[default]
    Flac,
    Wav,
    Alac,
}

impl AudioFormat {
    /// The on-disk file extension for a clip in this format. Kept separate from
    /// the [`Display`](fmt::Display) token so a codec's container extension need
    /// not match its config name.
    pub fn ext(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Wav => "wav",
            Self::Alac => "m4a",
        }
    }

    /// Whether an animated WebP can be embedded as this format's front cover.
    ///
    /// FLAC, MP3, and WAV embed an `image/webp` picture; ALAC (`mp4ameta` `covr`)
    /// supports only JPEG/PNG/BMP artwork, so it always embeds the static JPEG.
    pub fn embeds_animated_cover(self) -> bool {
        !matches!(self, Self::Alac)
    }
}

impl FromStr for AudioFormat {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mp3" => Ok(Self::Mp3),
            "flac" => Ok(Self::Flac),
            "wav" => Ok(Self::Wav),
            "alac" => Ok(Self::Alac),
            other => Err(Error::Config(format!("unknown format '{other}'"))),
        }
    }
}

impl fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mp3 => f.write_str("mp3"),
            Self::Flac => f.write_str("flac"),
            Self::Wav => f.write_str("wav"),
            Self::Alac => f.write_str("alac"),
        }
    }
}

/// Container format for a downloaded stem.
///
/// Stems are stored RAW in their native container and are never transcoded, so
/// unlike [`AudioFormat`] there is no lossless-from-lossy render: WAV comes
/// straight from Suno's free `convert_wav` endpoint and MP3 straight from the
/// public CDN. FLAC is deliberately unrepresentable — a stem is never
/// re-encoded to FLAC, even when the song's own format is FLAC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StemFormat {
    /// Lossless WAV via the free `convert_wav` render, stored as delivered.
    #[default]
    Wav,
    /// The public CDN MP3, stored as delivered.
    Mp3,
}

impl StemFormat {
    /// The file extension for a stem stored in this format.
    pub fn ext(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
        }
    }
}

impl FromStr for StemFormat {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "wav" => Ok(Self::Wav),
            "mp3" => Ok(Self::Mp3),
            "flac" => Err(Error::Config(
                "stems cannot be stored as FLAC; use 'wav' or 'mp3'".to_string(),
            )),
            other => Err(Error::Config(format!("unknown stem format '{other}'"))),
        }
    }
}

impl fmt::Display for StemFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.ext())
    }
}

/// Which video-cover artifacts to retain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VideoCoverRetention {
    #[default]
    Neither,
    Webp,
    Mp4,
    Both,
}

impl VideoCoverRetention {
    pub fn keeps_webp(self) -> bool {
        matches!(self, Self::Webp | Self::Both)
    }

    pub fn keeps_mp4(self) -> bool {
        matches!(self, Self::Mp4 | Self::Both)
    }
}

impl FromStr for VideoCoverRetention {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "neither" => Ok(Self::Neither),
            "webp" => Ok(Self::Webp),
            "mp4" => Ok(Self::Mp4),
            "both" => Ok(Self::Both),
            other => Err(Error::Config(format!(
                "unknown video_cover_retention '{other}'"
            ))),
        }
    }
}

impl fmt::Display for VideoCoverRetention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Neither => f.write_str("neither"),
            Self::Webp => f.write_str("webp"),
            Self::Mp4 => f.write_str("mp4"),
            Self::Both => f.write_str("both"),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_format_parses_case_insensitively() {
        assert_eq!("FLAC".parse::<AudioFormat>().unwrap(), AudioFormat::Flac);
        assert_eq!("Mp3".parse::<AudioFormat>().unwrap(), AudioFormat::Mp3);
        assert_eq!("wav".parse::<AudioFormat>().unwrap(), AudioFormat::Wav);
        assert_eq!("alac".parse::<AudioFormat>().unwrap(), AudioFormat::Alac);
    }

    #[test]
    fn audio_format_rejects_unknown_without_panicking() {
        assert!(matches!(
            "ogg".parse::<AudioFormat>().unwrap_err(),
            Error::Config(_)
        ));
    }

    #[test]
    fn audio_format_default_is_flac() {
        assert_eq!(AudioFormat::default(), AudioFormat::Flac);
    }

    #[test]
    fn audio_format_ext_differs_from_display_for_alac() {
        assert_eq!(AudioFormat::Alac.ext(), "m4a");
        assert_eq!(AudioFormat::Alac.to_string(), "alac");
    }

    #[test]
    fn audio_format_display_round_trips_through_from_str() {
        for f in [
            AudioFormat::Mp3,
            AudioFormat::Flac,
            AudioFormat::Wav,
            AudioFormat::Alac,
        ] {
            assert_eq!(f.to_string().parse::<AudioFormat>().unwrap(), f);
        }
    }

    #[test]
    fn audio_format_embeds_animated_cover_except_alac() {
        assert!(AudioFormat::Flac.embeds_animated_cover());
        assert!(AudioFormat::Mp3.embeds_animated_cover());
        assert!(AudioFormat::Wav.embeds_animated_cover());
        assert!(!AudioFormat::Alac.embeds_animated_cover());
    }

    #[test]
    fn stem_format_parses_wav_and_mp3() {
        assert_eq!("WAV".parse::<StemFormat>().unwrap(), StemFormat::Wav);
        assert_eq!("mp3".parse::<StemFormat>().unwrap(), StemFormat::Mp3);
    }

    #[test]
    fn stem_format_rejects_flac_with_guidance() {
        match "flac".parse::<StemFormat>().unwrap_err() {
            Error::Config(msg) => assert!(msg.contains("FLAC")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn stem_format_rejects_unknown_without_panicking() {
        assert!(matches!(
            "ogg".parse::<StemFormat>().unwrap_err(),
            Error::Config(_)
        ));
    }

    #[test]
    fn stem_format_default_is_wav_and_display_matches_ext() {
        assert_eq!(StemFormat::default(), StemFormat::Wav);
        assert_eq!(StemFormat::Mp3.to_string(), StemFormat::Mp3.ext());
    }

    #[test]
    fn video_cover_retention_parses_all_variants() {
        assert_eq!(
            "neither".parse::<VideoCoverRetention>().unwrap(),
            VideoCoverRetention::Neither
        );
        assert_eq!(
            "WEBP".parse::<VideoCoverRetention>().unwrap(),
            VideoCoverRetention::Webp
        );
        assert_eq!(
            "mp4".parse::<VideoCoverRetention>().unwrap(),
            VideoCoverRetention::Mp4
        );
        assert_eq!(
            "both".parse::<VideoCoverRetention>().unwrap(),
            VideoCoverRetention::Both
        );
    }

    #[test]
    fn video_cover_retention_rejects_unknown_without_panicking() {
        assert!(matches!(
            "all".parse::<VideoCoverRetention>().unwrap_err(),
            Error::Config(_)
        ));
    }

    #[test]
    fn video_cover_retention_keeps_matrix() {
        assert!(!VideoCoverRetention::Neither.keeps_webp());
        assert!(!VideoCoverRetention::Neither.keeps_mp4());
        assert!(VideoCoverRetention::Webp.keeps_webp());
        assert!(!VideoCoverRetention::Webp.keeps_mp4());
        assert!(!VideoCoverRetention::Mp4.keeps_webp());
        assert!(VideoCoverRetention::Mp4.keeps_mp4());
        assert!(VideoCoverRetention::Both.keeps_webp());
        assert!(VideoCoverRetention::Both.keeps_mp4());
    }

    #[test]
    fn video_cover_retention_default_is_neither() {
        assert_eq!(VideoCoverRetention::default(), VideoCoverRetention::Neither);
    }

    #[test]
    fn webp_defaults_fit_the_flac_picture_ceiling() {
        let d = WebpEncodeSettings::default();
        assert_eq!(d.quality, 90);
        assert_eq!(d.max_fps, 24);
        assert_eq!(d.max_width, Some(640));
        assert!(!d.lossless);
        assert_eq!(d.compression_level, 4);
    }
}
