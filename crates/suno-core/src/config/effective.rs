//! The resolved output types produced by [`Config::resolve`].

use std::collections::BTreeMap;

use crate::naming::CharacterSet;
use crate::vocab::{AudioFormat, StemFormat, VideoCoverRetention, WebpEncodeSettings};

use super::shape::{AreasConfig, Settings};

/// CLI flag overrides passed to [`Config::resolve`]. `None` means the flag
/// was not provided.
///
/// Only `token` is carried top-level (the one field with a global `--token`
/// flag); the rest nest in [`Settings`]. There is no `--token-command` flag, so
/// `settings.token_command` is never populated from the CLI.
#[derive(Debug, Default)]
pub struct FlagOverrides {
    pub token: Option<String>,
    pub settings: Settings,
}

/// Resolved effective settings for one account/source combination.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveSettings {
    /// A direct token from `--token` or `SUNO_*_TOKEN`.
    pub token: Option<String>,
    /// A stored token from `[accounts.<label>].token`.
    pub stored_token: Option<String>,
    /// A command to run for the token when no direct token was supplied.
    pub token_command: Option<String>,
    /// The optional configured account id assertion (see [`AccountConfig`]).
    pub account_id: Option<String>,
    pub format: AudioFormat,
    pub concurrency: u32,
    pub retries: u32,
    pub min_newest: u32,
    pub animated_covers: bool,
    /// Keep the raw album `cover.mp4` (`video_cover_url` verbatim, no transcode).
    /// Driven by [`VideoCoverRetention::keeps_mp4`]; independent of `video_mp4`.
    pub raw_animated_cover: bool,
    pub video_cover_retention: VideoCoverRetention,
    pub animated_cover_webp: WebpEncodeSettings,
    pub details_sidecar: bool,
    pub lyrics_sidecar: bool,
    pub lrc_sidecar: bool,
    pub video_mp4: bool,
    pub download_stems: bool,
    pub stem_format: StemFormat,
    pub naming_template: String,
    pub character_set: CharacterSet,
    /// The per-account `[areas]` selection table, if configured.
    pub areas: Option<AreasConfig>,
    /// Manual album-name overrides, keyed by lineage root id, resolved from the
    /// account's `[accounts.<label>.albums]` table. Deterministically ordered
    /// (a [`BTreeMap`]) and pre-trimmed of empty values by [`Config::resolve`].
    pub album_overrides: BTreeMap<String, String>,
    /// Lead-track flags from `[accounts.<label>].lead_tracks`: clip ids (or
    /// unique prefixes) each promoted to track 1 of their lineage album.
    /// Trimmed, de-duplicated, and deterministically ordered by
    /// [`Config::resolve`].
    pub lead_tracks: Vec<String>,
    /// Whether a lone-track album is numbered (defaults to `true`). `false`
    /// leaves singletons unnumbered.
    pub number_singletons: bool,
}

impl EffectiveSettings {
    /// Returns `true` when these settings require ffmpeg to be on `PATH`.
    ///
    /// Lossless output (FLAC or ALAC) transcodes from the WAV render and an
    /// animated WebP cover transcodes MP4→WebP, so either needs ffmpeg. A raw
    /// MP4 alone, or MP3/WAV with no animated covers, does not.
    pub fn requires_ffmpeg(&self) -> bool {
        matches!(self.format, AudioFormat::Flac | AudioFormat::Alac) || self.animated_covers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::fixtures::{no_env, no_flags};

    #[test]
    fn audio_format_display_roundtrip() {
        for fmt in [
            AudioFormat::Mp3,
            AudioFormat::Flac,
            AudioFormat::Wav,
            AudioFormat::Alac,
        ] {
            let s = fmt.to_string();
            assert_eq!(s.parse::<AudioFormat>().unwrap(), fmt);
        }
    }

    #[test]
    fn audio_format_ext() {
        assert_eq!(AudioFormat::Mp3.ext(), "mp3");
        assert_eq!(AudioFormat::Flac.ext(), "flac");
        assert_eq!(AudioFormat::Wav.ext(), "wav");
        assert_eq!(AudioFormat::Alac.ext(), "m4a");
    }

    fn base_settings(format: AudioFormat) -> EffectiveSettings {
        let toml = "[accounts.a]\n";
        let cfg = Config::from_toml(toml).unwrap();
        let mut eff = cfg.resolve("a", None, &no_env(), &no_flags()).unwrap();
        eff.format = format;
        eff
    }

    #[test]
    fn requires_ffmpeg_flac_always_needs_it() {
        let mut eff = base_settings(AudioFormat::Flac);
        eff.animated_covers = false;
        assert!(eff.requires_ffmpeg());
        eff.animated_covers = true;
        assert!(eff.requires_ffmpeg());
    }

    #[test]
    fn requires_ffmpeg_alac_always_needs_it() {
        let mut eff = base_settings(AudioFormat::Alac);
        eff.animated_covers = false;
        assert!(eff.requires_ffmpeg(), "alac transcodes, so needs ffmpeg");
    }

    #[test]
    fn requires_ffmpeg_mp3_needs_it_only_for_animated_webp() {
        let mut eff = base_settings(AudioFormat::Mp3);
        assert!(!eff.requires_ffmpeg(), "mp3 + no covers = no ffmpeg");
        eff.animated_covers = true;
        assert!(eff.requires_ffmpeg(), "mp3 + animated webp = needs ffmpeg");
        eff.raw_animated_cover = true;
        assert!(
            eff.requires_ffmpeg(),
            "mp3 + both (webp + raw mp4) = needs ffmpeg"
        );
        eff.animated_covers = false;
        assert!(!eff.requires_ffmpeg(), "mp3 + raw mp4 only = no ffmpeg");
    }
}
