//! The resolved output types produced by [`Config::resolve`].

use std::collections::BTreeMap;

use crate::naming::CharacterSet;
use crate::vocab::{AudioFormat, StemFormat, VideoCoverRetention, WebpEncodeSettings};

use super::shape::{AreasConfig, Settings};

/// CLI flag overrides passed to [`Config::resolve`](crate::config::Config::resolve). `None` means the flag
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
    /// The optional configured account id assertion (see [`AccountConfig`](crate::config::AccountConfig)).
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
    /// (a [`BTreeMap`]) and pre-trimmed of empty values by [`Config::resolve`](crate::config::Config::resolve).
    pub album_overrides: BTreeMap<String, String>,
    /// Lead-track flags from `[accounts.<label>].lead_tracks`: clip ids (or
    /// unique prefixes) each promoted to track 1 of their lineage album.
    /// Trimmed, de-duplicated, and deterministically ordered by
    /// [`Config::resolve`](crate::config::Config::resolve).
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

/// Whether an explicit `--animated-covers` request was silently overridden by a
/// `video_cover_retention` that keeps no animated WebP cover.
///
/// `--animated-covers` maps to `flag == Some(true)` (it is never `Some(false)`);
/// when `video_cover_retention` is unset the flag value survives resolution, so a
/// resolved `effective_animated == false` alongside `Some(true)` can only mean a
/// `neither`/`mp4` retention dropped it. Pure, so the rule is unit-tested beside
/// the resolver rather than in the CLI; the CLI only decides whether to print the
/// note. The documented precedence is unchanged: this reports the override, it
/// does not reverse it.
pub fn animated_covers_flag_overridden(flag: Option<bool>, effective_animated: bool) -> bool {
    flag == Some(true) && !effective_animated
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

    #[test]
    fn flag_override_detected_for_neither_and_mp4() {
        // `--animated-covers` was passed (`Some(true)`) but a `neither`/`mp4`
        // retention keeps no animated WebP, so the resolver dropped it to
        // `false`: the flag was silently overridden.
        assert!(animated_covers_flag_overridden(Some(true), false));
    }

    #[test]
    fn no_override_for_webp_or_both() {
        // `webp`/`both` keep the animated cover, so the resolved value is `true`
        // and the flag was honoured: no note.
        assert!(!animated_covers_flag_overridden(Some(true), true));
    }

    #[test]
    fn no_override_when_flag_absent() {
        // No `--animated-covers`: never a flag override, whatever the retention
        // resolved the effective value to.
        assert!(!animated_covers_flag_overridden(None, false));
        assert!(!animated_covers_flag_overridden(None, true));
    }
}
