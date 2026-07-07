//! Precedence resolution: layer the tiers (flag > per-account env > global
//! env > per-source file > per-account file > global defaults > compiled)
//! into [`EffectiveSettings`].

use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::naming::CharacterSet;
use crate::vocab::{AudioFormat, VideoCoverRetention, WebpEncodeSettings};

use super::effective::{EffectiveSettings, FlagOverrides};
use super::label_to_env;
use super::shape::Config;

impl Config {
    /// Compute effective settings for `account`, optionally scoped to `source`.
    ///
    /// The caller supplies the full environment map and any CLI flag overrides.
    /// Precedence per field: flag > per-account env > global env > per-source
    /// file > per-account file > global file defaults > compiled default.
    pub fn resolve(
        &self,
        account: &str,
        source: Option<&str>,
        env: &HashMap<String, String>,
        flags: &FlagOverrides,
    ) -> Result<EffectiveSettings> {
        let acc = self
            .accounts
            .get(account)
            .ok_or_else(|| Error::Config(format!("account '{account}' not found")))?;

        let src = source.and_then(|s| acc.sources.get(s));
        let label_env = label_to_env(account);

        // Look up per-account env first, falling back to global.
        let env_val = |suffix: &str| -> Option<&str> {
            env.get(&format!("SUNO_{label_env}_{suffix}"))
                .or_else(|| env.get(&format!("SUNO_{suffix}")))
                .map(String::as_str)
        };

        let format = resolve_enum(
            flags.settings.format,
            env_val("FORMAT"),
            src.and_then(|s| s.settings.format),
            acc.settings.format,
            self.defaults.settings.format,
            None,
            "FORMAT",
        )?
        .unwrap_or(AudioFormat::Flac);

        let concurrency = resolve_parsed(
            flags.settings.concurrency,
            env_val("CONCURRENCY"),
            src.and_then(|s| s.settings.concurrency),
            acc.settings.concurrency,
            self.defaults.settings.concurrency,
            4,
            "CONCURRENCY",
        )?;

        let retries = resolve_parsed(
            flags.settings.retries,
            env_val("RETRIES"),
            src.and_then(|s| s.settings.retries),
            acc.settings.retries,
            self.defaults.settings.retries,
            3,
            "RETRIES",
        )?;

        let min_newest = resolve_parsed(
            flags.settings.min_newest,
            env_val("MIN_NEWEST"),
            src.and_then(|s| s.settings.min_newest),
            acc.settings.min_newest,
            self.defaults.settings.min_newest,
            1,
            "MIN_NEWEST",
        )?;

        let animated_covers = resolve_parsed(
            flags.settings.animated_covers,
            env_val("ANIMATED_COVERS"),
            src.and_then(|s| s.settings.animated_covers),
            acc.settings.animated_covers,
            self.defaults.settings.animated_covers,
            false,
            "ANIMATED_COVERS",
        )?;

        let details_sidecar = resolve_parsed(
            flags.settings.details_sidecar,
            env_val("DETAILS_SIDECAR"),
            src.and_then(|s| s.settings.details_sidecar),
            acc.settings.details_sidecar,
            self.defaults.settings.details_sidecar,
            false,
            "DETAILS_SIDECAR",
        )?;

        let lyrics_sidecar = resolve_parsed(
            flags.settings.lyrics_sidecar,
            env_val("LYRICS_SIDECAR"),
            src.and_then(|s| s.settings.lyrics_sidecar),
            acc.settings.lyrics_sidecar,
            self.defaults.settings.lyrics_sidecar,
            false,
            "LYRICS_SIDECAR",
        )?;

        let lrc_sidecar = resolve_parsed(
            flags.settings.lrc_sidecar,
            env_val("LRC_SIDECAR"),
            src.and_then(|s| s.settings.lrc_sidecar),
            acc.settings.lrc_sidecar,
            self.defaults.settings.lrc_sidecar,
            false,
            "LRC_SIDECAR",
        )?;

        let video_mp4 = resolve_parsed(
            flags.settings.video_mp4,
            env_val("VIDEO_MP4"),
            src.and_then(|s| s.settings.video_mp4),
            acc.settings.video_mp4,
            self.defaults.settings.video_mp4,
            false,
            "VIDEO_MP4",
        )?;

        let download_stems = resolve_parsed(
            flags.settings.download_stems,
            env_val("DOWNLOAD_STEMS"),
            src.and_then(|s| s.settings.download_stems),
            acc.settings.download_stems,
            self.defaults.settings.download_stems,
            false,
            "DOWNLOAD_STEMS",
        )?;

        let stem_format = resolve_enum(
            flags.settings.stem_format,
            env_val("STEM_FORMAT"),
            src.and_then(|s| s.settings.stem_format),
            acc.settings.stem_format,
            self.defaults.settings.stem_format,
            None,
            "STEM_FORMAT",
        )?
        .unwrap_or_default();

        let video_cover_retention = resolve_enum(
            flags.settings.video_cover_retention,
            env_val("VIDEO_COVER_RETENTION"),
            src.and_then(|s| s.settings.video_cover_retention),
            acc.settings.video_cover_retention,
            self.defaults.settings.video_cover_retention,
            None,
            "VIDEO_COVER_RETENTION",
        )?;
        // `video_cover_retention` is the unified control for the album
        // video-cover artifacts: `webp`/`both` keep the transcoded `cover.webp`,
        // `mp4`/`both` the raw `cover.mp4`. The standalone music video
        // (`video_url`) keeps its own `video_mp4` toggle, untouched here.
        let (animated_covers, raw_animated_cover) = match video_cover_retention {
            Some(retention) => (retention.keeps_webp(), retention.keeps_mp4()),
            None => (animated_covers, false),
        };

        let defaults_webp = WebpEncodeSettings::default();
        let animated_cover_quality = resolve_u8_ranged(
            flags.settings.animated_cover_quality,
            env_val("ANIMATED_COVER_QUALITY"),
            src.and_then(|s| s.settings.animated_cover_quality),
            acc.settings.animated_cover_quality,
            self.defaults.settings.animated_cover_quality,
            defaults_webp.quality,
            "ANIMATED_COVER_QUALITY",
            0..=100,
        )?;
        let animated_cover_max_fps = resolve_parsed(
            flags.settings.animated_cover_max_fps,
            env_val("ANIMATED_COVER_MAX_FPS"),
            src.and_then(|s| s.settings.animated_cover_max_fps),
            acc.settings.animated_cover_max_fps,
            self.defaults.settings.animated_cover_max_fps,
            defaults_webp.max_fps,
            "ANIMATED_COVER_MAX_FPS",
        )?;
        let animated_cover_max_width = resolve_parsed_opt(
            flags.settings.animated_cover_max_width,
            env_val("ANIMATED_COVER_MAX_WIDTH"),
            src.and_then(|s| s.settings.animated_cover_max_width),
            acc.settings.animated_cover_max_width,
            self.defaults.settings.animated_cover_max_width,
            defaults_webp.max_width,
            "ANIMATED_COVER_MAX_WIDTH",
        )?;
        let animated_cover_compression_level = resolve_u8_ranged(
            flags.settings.animated_cover_compression_level,
            env_val("ANIMATED_COVER_COMPRESSION_LEVEL"),
            src.and_then(|s| s.settings.animated_cover_compression_level),
            acc.settings.animated_cover_compression_level,
            self.defaults.settings.animated_cover_compression_level,
            defaults_webp.compression_level,
            "ANIMATED_COVER_COMPRESSION_LEVEL",
            0..=4,
        )?;
        let animated_cover_lossless = resolve_parsed(
            flags.settings.animated_cover_lossless,
            env_val("ANIMATED_COVER_LOSSLESS"),
            src.and_then(|s| s.settings.animated_cover_lossless),
            acc.settings.animated_cover_lossless,
            self.defaults.settings.animated_cover_lossless,
            defaults_webp.lossless,
            "ANIMATED_COVER_LOSSLESS",
        )?;

        let naming_template = resolve_owned(
            flags.settings.naming_template.clone(),
            env_val("NAMING_TEMPLATE"),
            src.and_then(|s| s.settings.naming_template.clone()),
            acc.settings.naming_template.clone(),
            self.defaults.settings.naming_template.clone(),
        )
        .unwrap_or_else(|| crate::naming::DEFAULT_TEMPLATE.to_owned());

        let character_set = resolve_enum(
            flags.settings.character_set,
            env_val("CHARACTER_SET"),
            src.and_then(|s| s.settings.character_set),
            acc.settings.character_set,
            self.defaults.settings.character_set,
            None,
            "CHARACTER_SET",
        )?
        .unwrap_or(CharacterSet::Unicode);

        let number_singletons = resolve_parsed(
            flags.settings.number_singletons,
            env_val("NUMBER_SINGLETONS"),
            src.and_then(|s| s.settings.number_singletons),
            acc.settings.number_singletons,
            self.defaults.settings.number_singletons,
            true,
            "NUMBER_SINGLETONS",
        )?;

        let token = flags
            .token
            .clone()
            .or_else(|| env.get(&format!("SUNO_{label_env}_TOKEN")).cloned())
            .or_else(|| env.get("SUNO_TOKEN").cloned());

        let token_command = resolve_owned(
            None,
            env_val("TOKEN_COMMAND"),
            src.and_then(|s| s.settings.token_command.clone()),
            acc.settings.token_command.clone(),
            self.defaults.settings.token_command.clone(),
        );

        Ok(EffectiveSettings {
            token,
            stored_token: acc.token.clone(),
            token_command,
            account_id: acc.account_id.clone(),
            format,
            concurrency,
            retries,
            min_newest,
            animated_covers,
            raw_animated_cover,
            video_cover_retention: match (animated_covers, raw_animated_cover) {
                (false, false) => VideoCoverRetention::Neither,
                (true, false) => VideoCoverRetention::Webp,
                (false, true) => VideoCoverRetention::Mp4,
                (true, true) => VideoCoverRetention::Both,
            },
            animated_cover_webp: WebpEncodeSettings {
                quality: animated_cover_quality,
                max_fps: animated_cover_max_fps,
                max_width: animated_cover_max_width,
                lossless: animated_cover_lossless,
                compression_level: animated_cover_compression_level,
            },
            details_sidecar,
            lyrics_sidecar,
            lrc_sidecar,
            video_mp4,
            download_stems,
            stem_format,
            naming_template,
            character_set,
            areas: acc.areas.clone(),
            album_overrides: acc
                .albums
                .iter()
                .filter(|(_, name)| !name.trim().is_empty())
                .map(|(root_id, name)| (root_id.clone(), name.trim().to_owned()))
                .collect(),
            lead_tracks: acc
                .lead_tracks
                .iter()
                .map(|entry| entry.trim())
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned)
                .collect::<BTreeSet<String>>()
                .into_iter()
                .collect(),
            number_singletons,
        })
    }
}

fn resolve_parsed<T>(
    flag: Option<T>,
    env_str: Option<&str>,
    src: Option<T>,
    acc: Option<T>,
    defaults: Option<T>,
    compiled: T,
    name: &str,
) -> Result<T>
where
    T: FromStr + Copy,
{
    Ok(
        resolve_parsed_opt(flag, env_str, src, acc, defaults, Some(compiled), name)?
            .unwrap_or(compiled),
    )
}

/// Like [`resolve_parsed`], but the value stays optional at every tier including
/// the compiled default, so an unset knob resolves to `None` rather than a
/// scalar fallback. Used where "unset" is itself meaningful (e.g. a native width
/// with no cap).
fn resolve_parsed_opt<T>(
    flag: Option<T>,
    env_str: Option<&str>,
    src: Option<T>,
    acc: Option<T>,
    defaults: Option<T>,
    compiled: Option<T>,
    name: &str,
) -> Result<Option<T>>
where
    T: FromStr + Copy,
{
    if let Some(v) = flag {
        return Ok(Some(v));
    }
    if let Some(s) = env_str {
        return s
            .parse()
            .map(Some)
            .map_err(|_| Error::Config(format!("invalid {name}: '{s}'")));
    }
    Ok(src.or(acc).or(defaults).or(compiled))
}

#[allow(clippy::too_many_arguments)]
fn resolve_u8_ranged(
    flag: Option<u8>,
    env_str: Option<&str>,
    src: Option<u8>,
    acc: Option<u8>,
    defaults: Option<u8>,
    compiled: u8,
    name: &str,
    range: std::ops::RangeInclusive<u8>,
) -> Result<u8> {
    let value = if let Some(v) = flag {
        v
    } else if let Some(s) = env_str {
        s.parse()
            .map_err(|_| Error::Config(format!("invalid {name}: '{s}' (expected integer)")))?
    } else {
        src.or(acc).or(defaults).unwrap_or(compiled)
    };
    if range.contains(&value) {
        Ok(value)
    } else {
        Err(Error::Config(format!(
            "invalid {name}: '{value}' (expected {}..={})",
            range.start(),
            range.end()
        )))
    }
}

fn resolve_enum<T>(
    flag: Option<T>,
    env_str: Option<&str>,
    src: Option<T>,
    acc: Option<T>,
    defaults: Option<T>,
    compiled: Option<T>,
    name: &str,
) -> Result<Option<T>>
where
    T: FromStr<Err = Error> + Copy,
{
    if let Some(v) = flag {
        return Ok(Some(v));
    }
    if let Some(s) = env_str {
        return s
            .parse()
            .map(Some)
            .map_err(|err| Error::Config(format!("invalid {name}: '{s}' ({err})")));
    }
    Ok(src.or(acc).or(defaults).or(compiled))
}

/// Resolve an owned-`String` knob through the standard precedence. The env value
/// is taken verbatim (no parse), and the result stays optional so both a required
/// knob (`naming_template`, via `unwrap_or_else`) and an optional one
/// (`token_command`) share the one ladder. Pass `flag = None` for knobs with no
/// CLI flag.
fn resolve_owned(
    flag: Option<String>,
    env_str: Option<&str>,
    src: Option<String>,
    acc: Option<String>,
    defaults: Option<String>,
) -> Option<String> {
    flag.or_else(|| env_str.map(str::to_owned))
        .or(src)
        .or(acc)
        .or(defaults)
}

#[cfg(test)]
mod tests;
