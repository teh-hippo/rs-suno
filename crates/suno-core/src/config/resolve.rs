//! Precedence resolution: layer the tiers (flag > per-account env > global
//! env > per-source file > per-account file > global defaults > compiled)
//! into [`EffectiveSettings`].

use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::naming::CharacterSet;
use crate::vocab::{AudioFormat, LyricsTiming, VideoCoverRetention, WebpEncodeSettings};

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
            Layers {
                flag: flags.settings.format,
                env: env_val("FORMAT"),
                src: src.and_then(|s| s.settings.format),
                acc: acc.settings.format,
                defaults: self.defaults.settings.format,
                name: "FORMAT",
            },
            None,
        )?
        .unwrap_or(AudioFormat::Flac);

        let concurrency = resolve_parsed(
            Layers {
                flag: flags.settings.concurrency,
                env: env_val("CONCURRENCY"),
                src: src.and_then(|s| s.settings.concurrency),
                acc: acc.settings.concurrency,
                defaults: self.defaults.settings.concurrency,
                name: "CONCURRENCY",
            },
            4,
        )?;

        let retries = resolve_parsed(
            Layers {
                flag: flags.settings.retries,
                env: env_val("RETRIES"),
                src: src.and_then(|s| s.settings.retries),
                acc: acc.settings.retries,
                defaults: self.defaults.settings.retries,
                name: "RETRIES",
            },
            3,
        )?;

        let min_newest = resolve_parsed(
            Layers {
                flag: flags.settings.min_newest,
                env: env_val("MIN_NEWEST"),
                src: src.and_then(|s| s.settings.min_newest),
                acc: acc.settings.min_newest,
                defaults: self.defaults.settings.min_newest,
                name: "MIN_NEWEST",
            },
            1,
        )?;

        let animated_covers = resolve_parsed(
            Layers {
                flag: flags.settings.animated_covers,
                env: env_val("ANIMATED_COVERS"),
                src: src.and_then(|s| s.settings.animated_covers),
                acc: acc.settings.animated_covers,
                defaults: self.defaults.settings.animated_covers,
                name: "ANIMATED_COVERS",
            },
            false,
        )?;

        let details_sidecar = resolve_parsed(
            Layers {
                flag: flags.settings.details_sidecar,
                env: env_val("DETAILS_SIDECAR"),
                src: src.and_then(|s| s.settings.details_sidecar),
                acc: acc.settings.details_sidecar,
                defaults: self.defaults.settings.details_sidecar,
                name: "DETAILS_SIDECAR",
            },
            false,
        )?;

        let lyrics_sidecar = resolve_parsed(
            Layers {
                flag: flags.settings.lyrics_sidecar,
                env: env_val("LYRICS_SIDECAR"),
                src: src.and_then(|s| s.settings.lyrics_sidecar),
                acc: acc.settings.lyrics_sidecar,
                defaults: self.defaults.settings.lyrics_sidecar,
                name: "LYRICS_SIDECAR",
            },
            false,
        )?;

        let lrc_sidecar = resolve_parsed(
            Layers {
                flag: flags.settings.lrc_sidecar,
                env: env_val("LRC_SIDECAR"),
                src: src.and_then(|s| s.settings.lrc_sidecar),
                acc: acc.settings.lrc_sidecar,
                defaults: self.defaults.settings.lrc_sidecar,
                name: "LRC_SIDECAR",
            },
            false,
        )?;

        let lyrics_timing = resolve_enum(
            Layers {
                flag: flags.settings.lyrics_timing,
                env: env_val("LYRICS_TIMING"),
                src: src.and_then(|s| s.settings.lyrics_timing),
                acc: acc.settings.lyrics_timing,
                defaults: self.defaults.settings.lyrics_timing,
                name: "LYRICS_TIMING",
            },
            None,
        )?
        .unwrap_or(LyricsTiming::Line);

        let video_mp4 = resolve_parsed(
            Layers {
                flag: flags.settings.video_mp4,
                env: env_val("VIDEO_MP4"),
                src: src.and_then(|s| s.settings.video_mp4),
                acc: acc.settings.video_mp4,
                defaults: self.defaults.settings.video_mp4,
                name: "VIDEO_MP4",
            },
            false,
        )?;

        let download_stems = resolve_parsed(
            Layers {
                flag: flags.settings.download_stems,
                env: env_val("DOWNLOAD_STEMS"),
                src: src.and_then(|s| s.settings.download_stems),
                acc: acc.settings.download_stems,
                defaults: self.defaults.settings.download_stems,
                name: "DOWNLOAD_STEMS",
            },
            false,
        )?;

        let stem_format = resolve_enum(
            Layers {
                flag: flags.settings.stem_format,
                env: env_val("STEM_FORMAT"),
                src: src.and_then(|s| s.settings.stem_format),
                acc: acc.settings.stem_format,
                defaults: self.defaults.settings.stem_format,
                name: "STEM_FORMAT",
            },
            None,
        )?
        .unwrap_or_default();

        let video_cover_retention = resolve_enum(
            Layers {
                flag: flags.settings.video_cover_retention,
                env: env_val("VIDEO_COVER_RETENTION"),
                src: src.and_then(|s| s.settings.video_cover_retention),
                acc: acc.settings.video_cover_retention,
                defaults: self.defaults.settings.video_cover_retention,
                name: "VIDEO_COVER_RETENTION",
            },
            None,
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
            Layers {
                flag: flags.settings.animated_cover_quality,
                env: env_val("ANIMATED_COVER_QUALITY"),
                src: src.and_then(|s| s.settings.animated_cover_quality),
                acc: acc.settings.animated_cover_quality,
                defaults: self.defaults.settings.animated_cover_quality,
                name: "ANIMATED_COVER_QUALITY",
            },
            defaults_webp.quality,
            0..=100,
        )?;
        let animated_cover_max_fps = resolve_parsed(
            Layers {
                flag: flags.settings.animated_cover_max_fps,
                env: env_val("ANIMATED_COVER_MAX_FPS"),
                src: src.and_then(|s| s.settings.animated_cover_max_fps),
                acc: acc.settings.animated_cover_max_fps,
                defaults: self.defaults.settings.animated_cover_max_fps,
                name: "ANIMATED_COVER_MAX_FPS",
            },
            defaults_webp.max_fps,
        )?;
        let animated_cover_max_width = resolve_parsed_opt(
            Layers {
                flag: flags.settings.animated_cover_max_width,
                env: env_val("ANIMATED_COVER_MAX_WIDTH"),
                src: src.and_then(|s| s.settings.animated_cover_max_width),
                acc: acc.settings.animated_cover_max_width,
                defaults: self.defaults.settings.animated_cover_max_width,
                name: "ANIMATED_COVER_MAX_WIDTH",
            },
            defaults_webp.max_width,
        )?;
        let animated_cover_compression_level = resolve_u8_ranged(
            Layers {
                flag: flags.settings.animated_cover_compression_level,
                env: env_val("ANIMATED_COVER_COMPRESSION_LEVEL"),
                src: src.and_then(|s| s.settings.animated_cover_compression_level),
                acc: acc.settings.animated_cover_compression_level,
                defaults: self.defaults.settings.animated_cover_compression_level,
                name: "ANIMATED_COVER_COMPRESSION_LEVEL",
            },
            defaults_webp.compression_level,
            0..=4,
        )?;
        let animated_cover_lossless = resolve_parsed(
            Layers {
                flag: flags.settings.animated_cover_lossless,
                env: env_val("ANIMATED_COVER_LOSSLESS"),
                src: src.and_then(|s| s.settings.animated_cover_lossless),
                acc: acc.settings.animated_cover_lossless,
                defaults: self.defaults.settings.animated_cover_lossless,
                name: "ANIMATED_COVER_LOSSLESS",
            },
            defaults_webp.lossless,
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
            Layers {
                flag: flags.settings.character_set,
                env: env_val("CHARACTER_SET"),
                src: src.and_then(|s| s.settings.character_set),
                acc: acc.settings.character_set,
                defaults: self.defaults.settings.character_set,
                name: "CHARACTER_SET",
            },
            None,
        )?
        .unwrap_or(CharacterSet::Unicode);

        let number_singletons = resolve_parsed(
            Layers {
                flag: flags.settings.number_singletons,
                env: env_val("NUMBER_SINGLETONS"),
                src: src.and_then(|s| s.settings.number_singletons),
                acc: acc.settings.number_singletons,
                defaults: self.defaults.settings.number_singletons,
                name: "NUMBER_SINGLETONS",
            },
            true,
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
            lyrics_timing,
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

/// The layered sources for one setting, in precedence order: a CLI flag beats
/// the environment, which beats the `[sources.*]` table, which beats the
/// account, which beats `[defaults]`. `name` labels the knob in error messages
/// and is also its environment-variable suffix.
///
/// Grouped into a struct rather than passed positionally because the five tiers
/// are adjacent same-typed `Option`s: transposing `src` and `acc` in a call
/// compiles cleanly and silently inverts the precedence. Naming them at the call
/// site makes that class of mistake visible. The compiled fallback stays a
/// separate argument because its type varies by resolver (`T`, `Option<T>`, `u8`).
struct Layers<'a, T> {
    flag: Option<T>,
    env: Option<&'a str>,
    src: Option<T>,
    acc: Option<T>,
    defaults: Option<T>,
    name: &'a str,
}

fn resolve_parsed<T>(layers: Layers<'_, T>, compiled: T) -> Result<T>
where
    T: FromStr + Copy,
{
    Ok(resolve_parsed_opt(layers, Some(compiled))?.unwrap_or(compiled))
}

/// Like [`resolve_parsed`], but the value stays optional at every tier including
/// the compiled default, so an unset knob resolves to `None` rather than a
/// scalar fallback. Used where "unset" is itself meaningful (e.g. a native width
/// with no cap).
fn resolve_parsed_opt<T>(layers: Layers<'_, T>, compiled: Option<T>) -> Result<Option<T>>
where
    T: FromStr + Copy,
{
    let Layers {
        flag,
        env,
        src,
        acc,
        defaults,
        name,
    } = layers;
    if let Some(v) = flag {
        return Ok(Some(v));
    }
    if let Some(s) = env {
        return s
            .parse()
            .map(Some)
            .map_err(|_| Error::Config(format!("invalid {name}: '{s}'")));
    }
    Ok(src.or(acc).or(defaults).or(compiled))
}

fn resolve_u8_ranged(
    layers: Layers<'_, u8>,
    compiled: u8,
    range: std::ops::RangeInclusive<u8>,
) -> Result<u8> {
    let Layers {
        flag,
        env,
        src,
        acc,
        defaults,
        name,
    } = layers;
    let value = if let Some(v) = flag {
        v
    } else if let Some(s) = env {
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

fn resolve_enum<T>(layers: Layers<'_, T>, compiled: Option<T>) -> Result<Option<T>>
where
    T: FromStr<Err = Error> + Copy,
{
    let Layers {
        flag,
        env,
        src,
        acc,
        defaults,
        name,
    } = layers;
    if let Some(v) = flag {
        return Ok(Some(v));
    }
    if let Some(s) = env {
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
