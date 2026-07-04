//! Configuration model and precedence resolution.
//!
//! Parses a TOML string and merges in environment variables and CLI flag
//! overrides supplied by the caller. Performs no disk or environment IO.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ffmpeg::WebpEncodeSettings;
use crate::naming::CharacterSet;
use crate::reconcile::SourceMode;

/// Audio format for downloaded clips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Mp3,
    #[default]
    Flac,
    Wav,
}

impl FromStr for AudioFormat {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mp3" => Ok(Self::Mp3),
            "flac" => Ok(Self::Flac),
            "wav" => Ok(Self::Wav),
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

/// Global default settings applied when no account or source override applies.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Defaults {
    pub format: Option<AudioFormat>,
    pub concurrency: Option<u32>,
    pub retries: Option<u32>,
    pub min_newest: Option<u32>,
    pub token_command: Option<String>,
    pub animated_covers: Option<bool>,
    pub video_cover_retention: Option<VideoCoverRetention>,
    pub animated_cover_quality: Option<u8>,
    pub animated_cover_max_fps: Option<u32>,
    pub animated_cover_max_width: Option<u32>,
    pub animated_cover_compression_level: Option<u8>,
    pub details_sidecar: Option<bool>,
    pub lyrics_sidecar: Option<bool>,
    pub lrc_sidecar: Option<bool>,
    pub video_mp4: Option<bool>,
    pub download_stems: Option<bool>,
    pub stem_format: Option<StemFormat>,
    pub naming_template: Option<String>,
    pub character_set: Option<CharacterSet>,
}

/// Per-source overridable settings within an account.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SourceConfig {
    pub format: Option<AudioFormat>,
    pub concurrency: Option<u32>,
    pub retries: Option<u32>,
    pub min_newest: Option<u32>,
    pub token_command: Option<String>,
    pub animated_covers: Option<bool>,
    pub video_cover_retention: Option<VideoCoverRetention>,
    pub animated_cover_quality: Option<u8>,
    pub animated_cover_max_fps: Option<u32>,
    pub animated_cover_max_width: Option<u32>,
    pub animated_cover_compression_level: Option<u8>,
    pub details_sidecar: Option<bool>,
    pub lyrics_sidecar: Option<bool>,
    pub lrc_sidecar: Option<bool>,
    pub video_mp4: Option<bool>,
    pub download_stems: Option<bool>,
    pub stem_format: Option<StemFormat>,
    pub naming_template: Option<String>,
    pub character_set: Option<CharacterSet>,
}

/// Configuration for a single named account.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountConfig {
    pub token: Option<String>,
    pub token_command: Option<String>,
    pub root: Option<String>,
    /// Optional Suno user id to assert this account authenticates as, refusing
    /// to run on a mismatch (a belt-and-braces check alongside the on-disk
    /// owner pin in the lineage store).
    pub account_id: Option<String>,
    pub format: Option<AudioFormat>,
    pub concurrency: Option<u32>,
    pub retries: Option<u32>,
    pub min_newest: Option<u32>,
    pub animated_covers: Option<bool>,
    pub video_cover_retention: Option<VideoCoverRetention>,
    pub animated_cover_quality: Option<u8>,
    pub animated_cover_max_fps: Option<u32>,
    pub animated_cover_max_width: Option<u32>,
    pub animated_cover_compression_level: Option<u8>,
    pub details_sidecar: Option<bool>,
    pub lyrics_sidecar: Option<bool>,
    pub lrc_sidecar: Option<bool>,
    pub video_mp4: Option<bool>,
    pub download_stems: Option<bool>,
    pub stem_format: Option<StemFormat>,
    pub naming_template: Option<String>,
    pub character_set: Option<CharacterSet>,
    #[serde(default)]
    pub sources: HashMap<String, SourceConfig>,
    /// Per-area mode selection (`sync` vs `copy`) for this account's library,
    /// liked feed, and playlists. Absent means the classic single-verb run.
    pub areas: Option<AreasConfig>,
    /// Manual album-name overrides, keyed by the album's stable lineage root id
    /// (`<root_id> = "Preferred Name"`). Album identity is the lineage root, so
    /// the override is account-wide (like lineage), never per-source: the
    /// derived title is unstable and is exactly what this replaces. An empty or
    /// whitespace-only value is ignored, so a stray key cannot blank an album.
    #[serde(default)]
    pub albums: HashMap<String, String>,
}

/// How a single area treats deletion, including the library-only `off` value.
///
/// `off` is expressible only for the library area: it deliberately arms deletion
/// of library-exclusive files by suppressing the implicit copy-protector, so a
/// typo can never silently disarm that safety. `copy` and `mirror` map straight
/// onto the matching [`SourceMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaMode {
    /// Suppress the implicit library copy-protector (arm library deletions).
    Off,
    /// Treat the area with the given [`SourceMode`].
    Mode(SourceMode),
}

impl<'de> Deserialize<'de> for AreaMode {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "off" => Ok(AreaMode::Off),
            "copy" => Ok(AreaMode::Mode(SourceMode::Copy)),
            "mirror" => Ok(AreaMode::Mode(SourceMode::Mirror)),
            other => Err(serde::de::Error::custom(format!(
                "unknown area mode '{other}', expected 'off', 'copy', or 'mirror'"
            ))),
        }
    }
}

/// Per-area mode selection for an account.
///
/// `library` accepts `off`/`copy`/`mirror`; `liked` and `playlists` accept
/// `copy`/`mirror`; `playlist` overrides individual playlists by canonical Suno
/// id. `deny_unknown_fields` turns a mistyped key (e.g. `libary`) into a parse
/// error rather than a silent no-op. The `playlist` map cannot carry
/// `deny_unknown_fields` (its keys are dynamic playlist ids), but every value is
/// a closed [`SourceMode`], so a bad mode string still errors at parse time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreasConfig {
    pub library: Option<AreaMode>,
    pub liked: Option<SourceMode>,
    pub playlists: Option<SourceMode>,
    #[serde(default)]
    pub playlist: HashMap<String, SourceMode>,
}

/// Top-level configuration parsed from a TOML file.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub accounts: HashMap<String, AccountConfig>,
}

impl Config {
    /// Parse `toml_str` and validate the result.
    ///
    /// Validation rejects any pair of accounts whose root directories nest
    /// inside one another. Duplicate account labels are rejected by the TOML
    /// parser itself.
    pub fn from_toml(toml_str: &str) -> Result<Self> {
        let config: Self = toml::from_str(toml_str).map_err(|e| {
            // Strip source-context lines (those containing " | ") to prevent
            // token values from being echoed in error messages.
            let raw = e.to_string();
            let msg = raw
                .lines()
                .filter(|l| !l.contains(" | "))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_owned();
            Error::Config(if msg.is_empty() {
                "parse error".into()
            } else {
                msg
            })
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let roots: Vec<(&str, &str)> = self
            .accounts
            .iter()
            .filter_map(|(label, acc)| acc.root.as_deref().map(|r| (label.as_str(), r)))
            .collect();

        for (i, (label_a, root_a)) in roots.iter().enumerate() {
            for (label_b, root_b) in roots.iter().skip(i + 1) {
                let a = Path::new(root_a);
                let b = Path::new(root_b);
                if a.starts_with(b) || b.starts_with(a) {
                    return Err(Error::Config(format!(
                        "account roots nest: '{label_a}' ({root_a}) and '{label_b}' ({root_b})"
                    )));
                }
            }
        }

        let mut prefix_seen: HashMap<String, &str> = HashMap::new();
        for label in self.accounts.keys() {
            let prefix = label_to_env(label);
            if let Some(other) = prefix_seen.get(&prefix) {
                return Err(Error::Config(format!(
                    "accounts '{label}' and '{other}' share env prefix '{prefix}'"
                )));
            }
            prefix_seen.insert(prefix, label.as_str());
        }

        Ok(())
    }

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

        let format_from_env = env_val("FORMAT")
            .map(str::parse::<AudioFormat>)
            .transpose()?;

        let format = flags
            .format
            .or(format_from_env)
            .or_else(|| src.and_then(|s| s.format))
            .or(acc.format)
            .or(self.defaults.format)
            .unwrap_or(AudioFormat::Flac);

        let concurrency = resolve_u32(
            flags.concurrency,
            env_val("CONCURRENCY"),
            src.and_then(|s| s.concurrency),
            acc.concurrency,
            self.defaults.concurrency,
            4,
            "CONCURRENCY",
        )?;

        let retries = resolve_u32(
            flags.retries,
            env_val("RETRIES"),
            src.and_then(|s| s.retries),
            acc.retries,
            self.defaults.retries,
            3,
            "RETRIES",
        )?;

        let min_newest = resolve_u32(
            flags.min_newest,
            env_val("MIN_NEWEST"),
            src.and_then(|s| s.min_newest),
            acc.min_newest,
            self.defaults.min_newest,
            1,
            "MIN_NEWEST",
        )?;

        let animated_covers = resolve_bool(
            flags.animated_covers,
            env_val("ANIMATED_COVERS"),
            src.and_then(|s| s.animated_covers),
            acc.animated_covers,
            self.defaults.animated_covers,
            false,
            "ANIMATED_COVERS",
        )?;

        let details_sidecar = resolve_bool(
            flags.details_sidecar,
            env_val("DETAILS_SIDECAR"),
            src.and_then(|s| s.details_sidecar),
            acc.details_sidecar,
            self.defaults.details_sidecar,
            false,
            "DETAILS_SIDECAR",
        )?;

        let lyrics_sidecar = resolve_bool(
            flags.lyrics_sidecar,
            env_val("LYRICS_SIDECAR"),
            src.and_then(|s| s.lyrics_sidecar),
            acc.lyrics_sidecar,
            self.defaults.lyrics_sidecar,
            false,
            "LYRICS_SIDECAR",
        )?;

        let lrc_sidecar = resolve_bool(
            flags.lrc_sidecar,
            env_val("LRC_SIDECAR"),
            src.and_then(|s| s.lrc_sidecar),
            acc.lrc_sidecar,
            self.defaults.lrc_sidecar,
            false,
            "LRC_SIDECAR",
        )?;

        let video_mp4 = resolve_bool(
            flags.video_mp4,
            env_val("VIDEO_MP4"),
            src.and_then(|s| s.video_mp4),
            acc.video_mp4,
            self.defaults.video_mp4,
            false,
            "VIDEO_MP4",
        )?;

        let download_stems = resolve_bool(
            flags.download_stems,
            env_val("DOWNLOAD_STEMS"),
            src.and_then(|s| s.download_stems),
            acc.download_stems,
            self.defaults.download_stems,
            false,
            "DOWNLOAD_STEMS",
        )?;

        let stem_format_from_env = env_val("STEM_FORMAT")
            .map(str::parse::<StemFormat>)
            .transpose()?;
        let stem_format = flags
            .stem_format
            .or(stem_format_from_env)
            .or_else(|| src.and_then(|s| s.stem_format))
            .or(acc.stem_format)
            .or(self.defaults.stem_format)
            .unwrap_or_default();

        let video_cover_retention = resolve_enum(
            flags.video_cover_retention,
            env_val("VIDEO_COVER_RETENTION"),
            src.and_then(|s| s.video_cover_retention),
            acc.video_cover_retention,
            self.defaults.video_cover_retention,
            None,
            "VIDEO_COVER_RETENTION",
        )?;
        // `video_cover_retention`, when set, is the unified control for the
        // album video-cover artifacts: `webp`/`both` keep the transcoded
        // `cover.webp` (and the per-song `.webp`), `mp4`/`both` keep the raw
        // `cover.mp4` (`video_cover_url` verbatim). The standalone music video
        // (`video_url`) is a different asset and stays on its own `video_mp4`
        // toggle, untouched here.
        let (animated_covers, raw_animated_cover) = match video_cover_retention {
            Some(retention) => (retention.keeps_webp(), retention.keeps_mp4()),
            None => (animated_covers, false),
        };

        let defaults_webp = WebpEncodeSettings::default();
        let animated_cover_quality = resolve_u8_ranged(
            flags.animated_cover_quality,
            env_val("ANIMATED_COVER_QUALITY"),
            src.and_then(|s| s.animated_cover_quality),
            acc.animated_cover_quality,
            self.defaults.animated_cover_quality,
            defaults_webp.quality,
            "ANIMATED_COVER_QUALITY",
            0..=100,
        )?;
        let animated_cover_max_fps = resolve_u32(
            flags.animated_cover_max_fps,
            env_val("ANIMATED_COVER_MAX_FPS"),
            src.and_then(|s| s.animated_cover_max_fps),
            acc.animated_cover_max_fps,
            self.defaults.animated_cover_max_fps,
            defaults_webp.max_fps,
            "ANIMATED_COVER_MAX_FPS",
        )?;
        let animated_cover_max_width_from_env = env_val("ANIMATED_COVER_MAX_WIDTH")
            .map(|s| {
                s.parse().map_err(|_| {
                    Error::Config(format!(
                        "invalid ANIMATED_COVER_MAX_WIDTH: '{s}' (expected integer)"
                    ))
                })
            })
            .transpose()?;
        let animated_cover_max_width = if let Some(v) = flags.animated_cover_max_width {
            Some(v)
        } else if let Some(v) = animated_cover_max_width_from_env {
            Some(v)
        } else {
            src.and_then(|s| s.animated_cover_max_width)
                .or(acc.animated_cover_max_width)
                .or(self.defaults.animated_cover_max_width)
                .or(defaults_webp.max_width)
        };
        let animated_cover_compression_level = resolve_u8_ranged(
            flags.animated_cover_compression_level,
            env_val("ANIMATED_COVER_COMPRESSION_LEVEL"),
            src.and_then(|s| s.animated_cover_compression_level),
            acc.animated_cover_compression_level,
            self.defaults.animated_cover_compression_level,
            defaults_webp.compression_level,
            "ANIMATED_COVER_COMPRESSION_LEVEL",
            0..=6,
        )?;

        let naming_template_from_env = env_val("NAMING_TEMPLATE").map(str::to_owned);
        let naming_template = flags
            .naming_template
            .clone()
            .or(naming_template_from_env)
            .or_else(|| src.and_then(|s| s.naming_template.clone()))
            .or_else(|| acc.naming_template.clone())
            .or_else(|| self.defaults.naming_template.clone())
            .unwrap_or_else(|| crate::naming::DEFAULT_TEMPLATE.to_owned());

        let character_set_from_env = env_val("CHARACTER_SET")
            .map(str::parse::<CharacterSet>)
            .transpose()?;
        let character_set = flags
            .character_set
            .or(character_set_from_env)
            .or_else(|| src.and_then(|s| s.character_set))
            .or(acc.character_set)
            .or(self.defaults.character_set)
            .unwrap_or(CharacterSet::Unicode);

        let token = flags
            .token
            .clone()
            .or_else(|| env.get(&format!("SUNO_{label_env}_TOKEN")).cloned())
            .or_else(|| env.get("SUNO_TOKEN").cloned());

        let token_command = env
            .get(&format!("SUNO_{label_env}_TOKEN_COMMAND"))
            .cloned()
            .or_else(|| env.get("SUNO_TOKEN_COMMAND").cloned())
            .or_else(|| src.and_then(|s| s.token_command.clone()))
            .or_else(|| acc.token_command.clone())
            .or_else(|| self.defaults.token_command.clone());

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
                lossless: defaults_webp.lossless,
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
        })
    }
}

fn resolve_u32(
    flag: Option<u32>,
    env_str: Option<&str>,
    src: Option<u32>,
    acc: Option<u32>,
    defaults: Option<u32>,
    compiled: u32,
    name: &str,
) -> Result<u32> {
    if let Some(v) = flag {
        return Ok(v);
    }
    if let Some(s) = env_str {
        return s
            .parse()
            .map_err(|_| Error::Config(format!("invalid {name}: '{s}'")));
    }
    Ok(src.or(acc).or(defaults).unwrap_or(compiled))
}

fn resolve_bool(
    flag: Option<bool>,
    env_str: Option<&str>,
    src: Option<bool>,
    acc: Option<bool>,
    defaults: Option<bool>,
    compiled: bool,
    name: &str,
) -> Result<bool> {
    if let Some(v) = flag {
        return Ok(v);
    }
    if let Some(s) = env_str {
        return s
            .parse()
            .map_err(|_| Error::Config(format!("invalid {name}: '{s}'")));
    }
    Ok(src.or(acc).or(defaults).unwrap_or(compiled))
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

/// Convert an account label to its environment variable prefix, mirroring the
/// per-account keys the resolver reads: `my-lib` becomes `MY_LIB` for lookups
/// like `SUNO_MY_LIB_TOKEN`.
pub fn label_to_env(label: &str) -> String {
    label.to_ascii_uppercase().replace('-', "_")
}

/// CLI flag overrides passed to [`Config::resolve`]. `None` means the flag
/// was not provided.
#[derive(Debug, Default)]
pub struct FlagOverrides {
    pub token: Option<String>,
    pub format: Option<AudioFormat>,
    pub concurrency: Option<u32>,
    pub retries: Option<u32>,
    pub min_newest: Option<u32>,
    pub animated_covers: Option<bool>,
    pub video_cover_retention: Option<VideoCoverRetention>,
    pub animated_cover_quality: Option<u8>,
    pub animated_cover_max_fps: Option<u32>,
    pub animated_cover_max_width: Option<u32>,
    pub animated_cover_compression_level: Option<u8>,
    pub details_sidecar: Option<bool>,
    pub lyrics_sidecar: Option<bool>,
    pub lrc_sidecar: Option<bool>,
    pub video_mp4: Option<bool>,
    pub download_stems: Option<bool>,
    pub stem_format: Option<StemFormat>,
    pub naming_template: Option<String>,
    pub character_set: Option<CharacterSet>,
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
}

impl EffectiveSettings {
    /// Returns `true` when these settings require ffmpeg to be on `PATH`.
    ///
    /// FLAC output transcodes WAV→FLAC, and an animated WebP cover transcodes
    /// MP4→WebP, so either needs ffmpeg. Keeping the raw MP4 alongside the WebP
    /// (the `both` retention) still produces the WebP, so `animated_covers`
    /// alone decides it; a raw-MP4-only run, or a plain MP3/WAV run with no
    /// animated covers, needs no ffmpeg.
    pub fn requires_ffmpeg(&self) -> bool {
        self.format == AudioFormat::Flac || self.animated_covers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env() -> HashMap<String, String> {
        HashMap::new()
    }

    fn no_flags() -> FlagOverrides {
        FlagOverrides::default()
    }

    #[test]
    fn parse_empty_toml() {
        let cfg = Config::from_toml("").unwrap();
        assert!(cfg.accounts.is_empty());
    }

    #[test]
    fn parse_basic_account() {
        let toml = r#"
            [accounts.alice]
            token = "tok"
            root = "/music"
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let acc = &cfg.accounts["alice"];
        assert_eq!(acc.token.as_deref(), Some("tok"));
        assert_eq!(acc.root.as_deref(), Some("/music"));
    }

    #[test]
    fn account_id_parses_and_resolves() {
        let toml = r#"
            [accounts.alice]
            token = "tok"
            root = "/music"
            account_id = "user_abc123"
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(
            cfg.accounts["alice"].account_id.as_deref(),
            Some("user_abc123")
        );
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(eff.account_id.as_deref(), Some("user_abc123"));
    }

    #[test]
    fn parse_defaults_section() {
        let toml = r#"
            [defaults]
            format = "mp3"
            concurrency = 8
            retries = 5
            min_newest = 2
            animated_covers = true
            video_cover_retention = "both"
            animated_cover_quality = 85
            animated_cover_max_fps = 18
            animated_cover_max_width = 720
            animated_cover_compression_level = 4
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(cfg.defaults.format, Some(AudioFormat::Mp3));
        assert_eq!(cfg.defaults.concurrency, Some(8));
        assert_eq!(cfg.defaults.retries, Some(5));
        assert_eq!(cfg.defaults.min_newest, Some(2));
        assert_eq!(cfg.defaults.animated_covers, Some(true));
        assert_eq!(
            cfg.defaults.video_cover_retention,
            Some(VideoCoverRetention::Both)
        );
        assert_eq!(cfg.defaults.animated_cover_quality, Some(85));
        assert_eq!(cfg.defaults.animated_cover_max_fps, Some(18));
        assert_eq!(cfg.defaults.animated_cover_max_width, Some(720));
        assert_eq!(cfg.defaults.animated_cover_compression_level, Some(4));
    }

    #[test]
    fn compiled_defaults_when_nothing_set() {
        let toml = "[accounts.alice]\n";
        let cfg = Config::from_toml(toml).unwrap();
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(
            eff,
            EffectiveSettings {
                token: None,
                stored_token: None,
                token_command: None,
                account_id: None,
                format: AudioFormat::Flac,
                concurrency: 4,
                retries: 3,
                min_newest: 1,
                animated_covers: false,
                raw_animated_cover: false,
                video_cover_retention: VideoCoverRetention::Neither,
                animated_cover_webp: WebpEncodeSettings::default(),
                details_sidecar: false,
                lyrics_sidecar: false,
                lrc_sidecar: false,
                video_mp4: false,
                download_stems: false,
                stem_format: StemFormat::Wav,
                naming_template: crate::naming::DEFAULT_TEMPLATE.to_owned(),
                character_set: CharacterSet::Unicode,
                areas: None,
                album_overrides: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn file_defaults_override_compiled() {
        let toml = r#"
            [defaults]
            format = "mp3"
            concurrency = 8

            [accounts.alice]
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(eff.format, AudioFormat::Mp3);
        assert_eq!(eff.concurrency, 8);
        assert_eq!(eff.retries, 3); // compiled default
    }

    #[test]
    fn account_settings_override_defaults() {
        let toml = r#"
            [defaults]
            format = "mp3"

            [accounts.alice]
            format = "wav"
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(eff.format, AudioFormat::Wav);
    }

    #[test]
    fn per_source_overrides_account() {
        let toml = r#"
            [accounts.alice]
            format = "flac"

            [accounts.alice.sources.liked]
            format = "mp3"
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let eff = cfg
            .resolve("alice", Some("liked"), &no_env(), &no_flags())
            .unwrap();
        assert_eq!(eff.format, AudioFormat::Mp3);
    }

    #[test]
    fn unknown_source_falls_back_to_account() {
        let toml = r#"
            [accounts.alice]
            format = "wav"
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let eff = cfg
            .resolve("alice", Some("nonexistent"), &no_env(), &no_flags())
            .unwrap();
        assert_eq!(eff.format, AudioFormat::Wav);
    }

    #[test]
    fn global_env_overrides_file() {
        let toml = r#"
            [accounts.alice]
            format = "flac"
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let env: HashMap<String, String> =
            [("SUNO_FORMAT".into(), "mp3".into())].into_iter().collect();
        let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.format, AudioFormat::Mp3);
    }

    #[test]
    fn per_account_env_overrides_global_env() {
        let toml = "[accounts.alice]\n";
        let cfg = Config::from_toml(toml).unwrap();
        let env: HashMap<String, String> = [
            ("SUNO_FORMAT".into(), "mp3".into()),
            ("SUNO_ALICE_FORMAT".into(), "wav".into()),
        ]
        .into_iter()
        .collect();
        let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.format, AudioFormat::Wav);
    }

    #[test]
    fn per_account_env_label_uppersnakedcase() {
        let toml = "[accounts.my-lib]\n";
        let cfg = Config::from_toml(toml).unwrap();
        let env: HashMap<String, String> = [("SUNO_MY_LIB_FORMAT".into(), "wav".into())]
            .into_iter()
            .collect();
        let eff = cfg.resolve("my-lib", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.format, AudioFormat::Wav);
    }

    #[test]
    fn flag_overrides_env_and_file() {
        let toml = r#"
            [accounts.alice]
            format = "flac"
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let env: HashMap<String, String> =
            [("SUNO_FORMAT".into(), "mp3".into())].into_iter().collect();
        let flags = FlagOverrides {
            format: Some(AudioFormat::Wav),
            ..Default::default()
        };
        let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
        assert_eq!(eff.format, AudioFormat::Wav);
    }

    #[test]
    fn token_precedence() {
        let toml = r#"
            [accounts.alice]
            token = "file_tok"
        "#;
        let cfg = Config::from_toml(toml).unwrap();

        // env overrides file
        let env: HashMap<String, String> = [("SUNO_TOKEN".into(), "env_tok".into())]
            .into_iter()
            .collect();
        let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.token.as_deref(), Some("env_tok"));
        assert_eq!(eff.stored_token.as_deref(), Some("file_tok"));

        // flag overrides env
        let flags = FlagOverrides {
            token: Some("flag_tok".into()),
            ..Default::default()
        };
        let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
        assert_eq!(eff.token.as_deref(), Some("flag_tok"));
        assert_eq!(eff.stored_token.as_deref(), Some("file_tok"));
    }

    #[test]
    fn stored_token_is_populated_from_config_when_no_override_exists() {
        let toml = r#"
            [accounts.alice]
            token = "file_tok"
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(eff.token, None);
        assert_eq!(eff.stored_token.as_deref(), Some("file_tok"));
        assert_eq!(eff.token_command, None);
    }

    #[test]
    fn per_account_token_env_overrides_global() {
        let toml = "[accounts.alice]\n";
        let cfg = Config::from_toml(toml).unwrap();
        let env: HashMap<String, String> = [
            ("SUNO_TOKEN".into(), "global".into()),
            ("SUNO_ALICE_TOKEN".into(), "per_account".into()),
        ]
        .into_iter()
        .collect();
        let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.token.as_deref(), Some("per_account"));
    }

    #[test]
    fn token_command_resolves_from_defaults_account_source_and_env() {
        let toml = r#"
            [defaults]
            token_command = "defaults"

            [accounts.alice]
            token_command = "account"

            [accounts.alice.sources.liked]
            token_command = "source"
        "#;
        let cfg = Config::from_toml(toml).unwrap();

        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(eff.token_command.as_deref(), Some("account"));

        let eff = cfg
            .resolve("alice", Some("liked"), &no_env(), &no_flags())
            .unwrap();
        assert_eq!(eff.token_command.as_deref(), Some("source"));

        let env: HashMap<String, String> = [("SUNO_TOKEN_COMMAND".into(), "global".into())]
            .into_iter()
            .collect();
        let eff = cfg
            .resolve("alice", Some("liked"), &env, &no_flags())
            .unwrap();
        assert_eq!(eff.token_command.as_deref(), Some("global"));

        let env: HashMap<String, String> = [
            ("SUNO_TOKEN_COMMAND".into(), "global".into()),
            ("SUNO_ALICE_TOKEN_COMMAND".into(), "per_account".into()),
        ]
        .into_iter()
        .collect();
        let eff = cfg
            .resolve("alice", Some("liked"), &env, &no_flags())
            .unwrap();
        assert_eq!(eff.token_command.as_deref(), Some("per_account"));
    }

    #[test]
    fn per_account_token_command_env_label_uppersnakedcase() {
        let cfg = Config::from_toml("[accounts.my-lib]\n").unwrap();
        let env: HashMap<String, String> = [("SUNO_MY_LIB_TOKEN_COMMAND".into(), "command".into())]
            .into_iter()
            .collect();
        let eff = cfg.resolve("my-lib", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.token_command.as_deref(), Some("command"));
    }

    #[test]
    fn invalid_env_u32_errors() {
        let toml = "[accounts.alice]\n";
        let cfg = Config::from_toml(toml).unwrap();
        let env: HashMap<String, String> = [("SUNO_CONCURRENCY".into(), "many".into())]
            .into_iter()
            .collect();
        assert!(cfg.resolve("alice", None, &env, &no_flags()).is_err());
    }

    #[test]
    fn animated_covers_defaults_off_and_follows_precedence() {
        // Compiled default is off.
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert!(!eff.animated_covers);

        // File default on; per-source off; env on; flag off — flag wins.
        let toml = r#"
            [defaults]
            animated_covers = true

            [accounts.alice.sources.liked]
            animated_covers = false
        "#;
        let cfg = Config::from_toml(toml).unwrap();

        // File default (defaults) turns it on for an unscoped resolve.
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert!(eff.animated_covers);

        // Per-source file setting overrides the file default.
        let eff = cfg
            .resolve("alice", Some("liked"), &no_env(), &no_flags())
            .unwrap();
        assert!(!eff.animated_covers);

        // Env overrides file (even the per-source off).
        let env: HashMap<String, String> = [("SUNO_ANIMATED_COVERS".into(), "true".into())]
            .into_iter()
            .collect();
        let eff = cfg
            .resolve("alice", Some("liked"), &env, &no_flags())
            .unwrap();
        assert!(eff.animated_covers);

        // Flag overrides env.
        let flags = FlagOverrides {
            animated_covers: Some(false),
            ..Default::default()
        };
        let eff = cfg.resolve("alice", Some("liked"), &env, &flags).unwrap();
        assert!(!eff.animated_covers);
    }

    #[test]
    fn video_mp4_defaults_off_and_follows_precedence() {
        // Compiled default is off.
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert!(!eff.video_mp4);

        // File default on; per-source off; env on; flag off — flag wins.
        let toml = r#"
            [defaults]
            video_mp4 = true

            [accounts.alice.sources.liked]
            video_mp4 = false
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert!(
            cfg.resolve("alice", None, &no_env(), &no_flags())
                .unwrap()
                .video_mp4
        );
        assert!(
            !cfg.resolve("alice", Some("liked"), &no_env(), &no_flags())
                .unwrap()
                .video_mp4
        );

        let env: HashMap<String, String> = [("SUNO_VIDEO_MP4".into(), "true".into())]
            .into_iter()
            .collect();
        assert!(
            cfg.resolve("alice", Some("liked"), &env, &no_flags())
                .unwrap()
                .video_mp4
        );

        let flags = FlagOverrides {
            video_mp4: Some(false),
            ..Default::default()
        };
        assert!(
            !cfg.resolve("alice", Some("liked"), &env, &flags)
                .unwrap()
                .video_mp4
        );
    }

    #[test]
    fn download_stems_defaults_off_and_follows_precedence() {
        // Compiled default is off (bulk stem mirroring never spends, but it is
        // opt-in so it never runs unless asked).
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
        assert!(
            !cfg.resolve("alice", None, &no_env(), &no_flags())
                .unwrap()
                .download_stems
        );

        // File default on; per-source off; env on; flag off — flag wins.
        let toml = r#"
            [defaults]
            download_stems = true

            [accounts.alice.sources.liked]
            download_stems = false
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert!(
            cfg.resolve("alice", None, &no_env(), &no_flags())
                .unwrap()
                .download_stems
        );
        assert!(
            !cfg.resolve("alice", Some("liked"), &no_env(), &no_flags())
                .unwrap()
                .download_stems
        );

        let env: HashMap<String, String> = [("SUNO_DOWNLOAD_STEMS".into(), "true".into())]
            .into_iter()
            .collect();
        assert!(
            cfg.resolve("alice", Some("liked"), &env, &no_flags())
                .unwrap()
                .download_stems
        );

        let flags = FlagOverrides {
            download_stems: Some(false),
            ..Default::default()
        };
        assert!(
            !cfg.resolve("alice", Some("liked"), &env, &flags)
                .unwrap()
                .download_stems
        );
    }

    #[test]
    fn stem_format_defaults_to_wav_and_follows_precedence() {
        // Compiled default is WAV (lossless, the safe default for stems).
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
        assert_eq!(
            cfg.resolve("alice", None, &no_env(), &no_flags())
                .unwrap()
                .stem_format,
            StemFormat::Wav
        );

        // File default mp3; per-source wav; env mp3; flag wav — flag wins.
        let toml = r#"
            [defaults]
            stem_format = "mp3"

            [accounts.alice.sources.liked]
            stem_format = "wav"
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(
            cfg.resolve("alice", None, &no_env(), &no_flags())
                .unwrap()
                .stem_format,
            StemFormat::Mp3
        );
        assert_eq!(
            cfg.resolve("alice", Some("liked"), &no_env(), &no_flags())
                .unwrap()
                .stem_format,
            StemFormat::Wav
        );

        let env: HashMap<String, String> = [("SUNO_STEM_FORMAT".into(), "mp3".into())]
            .into_iter()
            .collect();
        assert_eq!(
            cfg.resolve("alice", Some("liked"), &env, &no_flags())
                .unwrap()
                .stem_format,
            StemFormat::Mp3
        );

        let flags = FlagOverrides {
            stem_format: Some(StemFormat::Wav),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve("alice", Some("liked"), &env, &flags)
                .unwrap()
                .stem_format,
            StemFormat::Wav
        );
    }

    #[test]
    fn stem_format_rejects_flac_and_unknown() {
        // FLAC is deliberately unrepresentable for stems: parsing it is an error,
        // so a config or flag can never ask for a FLAC stem.
        assert!("flac".parse::<StemFormat>().is_err());
        assert!("aac".parse::<StemFormat>().is_err());
        assert_eq!("WAV".parse::<StemFormat>().unwrap(), StemFormat::Wav);
        assert_eq!("Mp3".parse::<StemFormat>().unwrap(), StemFormat::Mp3);
        // A FLAC stem_format in config is a config error, not a silent fallback.
        assert!(Config::from_toml("[defaults]\nstem_format = \"flac\"\n").is_err());
    }

    #[test]
    fn video_cover_retention_drives_cover_artifacts_not_the_music_video() {
        let resolve = |retention: &str| {
            let toml = format!("[accounts.alice]\nvideo_cover_retention = \"{retention}\"\n");
            Config::from_toml(&toml)
                .unwrap()
                .resolve("alice", None, &no_env(), &no_flags())
                .unwrap()
        };

        let neither = resolve("neither");
        assert!(!neither.animated_covers && !neither.raw_animated_cover);
        assert_eq!(neither.video_cover_retention, VideoCoverRetention::Neither);

        let webp = resolve("webp");
        assert!(webp.animated_covers && !webp.raw_animated_cover);
        assert_eq!(webp.video_cover_retention, VideoCoverRetention::Webp);

        // `mp4` keeps the raw album cover (`video_cover_url` verbatim); it does
        // NOT switch on the standalone music-video toggle (`video_url`).
        let mp4 = resolve("mp4");
        assert!(!mp4.animated_covers && mp4.raw_animated_cover);
        assert!(!mp4.video_mp4);
        assert_eq!(mp4.video_cover_retention, VideoCoverRetention::Mp4);

        let both = resolve("both");
        assert!(both.animated_covers && both.raw_animated_cover);
        assert!(!both.video_mp4);
        assert_eq!(both.video_cover_retention, VideoCoverRetention::Both);
    }

    #[test]
    fn video_mp4_is_independent_of_cover_retention() {
        // The standalone music video (`video_url`) has its own toggle and is
        // never implied by a `video_cover_retention` mode, nor vice versa.
        let toml = "[accounts.alice]\nvideo_mp4 = true\nvideo_cover_retention = \"webp\"\n";
        let cfg = Config::from_toml(toml).unwrap();
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert!(eff.video_mp4);
        assert!(eff.animated_covers);
        assert!(!eff.raw_animated_cover);
        assert_eq!(eff.video_cover_retention, VideoCoverRetention::Webp);
    }

    #[test]
    fn animated_cover_webp_knobs_follow_precedence_and_validate_ranges() {
        let toml = r#"
            [defaults]
            animated_cover_quality = 80
            animated_cover_max_fps = 20
            animated_cover_max_width = 640
            animated_cover_compression_level = 3

            [accounts.alice.sources.liked]
            animated_cover_quality = 75
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let eff = cfg
            .resolve("alice", Some("liked"), &no_env(), &no_flags())
            .unwrap();
        assert_eq!(eff.animated_cover_webp.quality, 75);
        assert_eq!(eff.animated_cover_webp.max_fps, 20);
        assert_eq!(eff.animated_cover_webp.max_width, Some(640));
        assert_eq!(eff.animated_cover_webp.compression_level, 3);

        let env: HashMap<String, String> = [("SUNO_ANIMATED_COVER_QUALITY".into(), "90".into())]
            .into_iter()
            .collect();
        let eff = cfg
            .resolve("alice", Some("liked"), &env, &no_flags())
            .unwrap();
        assert_eq!(eff.animated_cover_webp.quality, 90);

        let flags = FlagOverrides {
            animated_cover_quality: Some(95),
            animated_cover_max_width: Some(512),
            animated_cover_compression_level: Some(6),
            ..Default::default()
        };
        let eff = cfg.resolve("alice", Some("liked"), &env, &flags).unwrap();
        assert_eq!(eff.animated_cover_webp.quality, 95);
        assert_eq!(eff.animated_cover_webp.max_width, Some(512));
        assert_eq!(eff.animated_cover_webp.compression_level, 6);

        let bad_env: HashMap<String, String> =
            [("SUNO_ANIMATED_COVER_QUALITY".into(), "101".into())]
                .into_iter()
                .collect();
        assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());
    }

    #[test]
    fn video_cover_retention_parses_formats_and_reports_kept_artifacts() {
        // FromStr is case-insensitive across every variant.
        assert_eq!(
            "NEITHER".parse::<VideoCoverRetention>().unwrap(),
            VideoCoverRetention::Neither
        );
        assert_eq!(
            "WebP".parse::<VideoCoverRetention>().unwrap(),
            VideoCoverRetention::Webp
        );
        assert_eq!(
            "mp4".parse::<VideoCoverRetention>().unwrap(),
            VideoCoverRetention::Mp4
        );
        assert_eq!(
            "Both".parse::<VideoCoverRetention>().unwrap(),
            VideoCoverRetention::Both
        );
        // An unknown mode is a config error, not a silent fallback.
        assert!("mkv".parse::<VideoCoverRetention>().is_err());

        // Display round-trips back to a token FromStr accepts.
        for mode in [
            VideoCoverRetention::Neither,
            VideoCoverRetention::Webp,
            VideoCoverRetention::Mp4,
            VideoCoverRetention::Both,
        ] {
            assert_eq!(
                mode.to_string().parse::<VideoCoverRetention>().unwrap(),
                mode
            );
        }

        // keeps_webp / keeps_mp4 truth table.
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
    fn video_cover_retention_resolves_from_env_and_rejects_unknown() {
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();

        // A valid env value overrides the legacy toggles, just like a flag.
        let env: HashMap<String, String> = [("SUNO_VIDEO_COVER_RETENTION".into(), "both".into())]
            .into_iter()
            .collect();
        let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.video_cover_retention, VideoCoverRetention::Both);
        assert!(eff.animated_covers);
        assert!(eff.raw_animated_cover);

        // An unknown env value is a config error rather than a silent default.
        let bad_env: HashMap<String, String> =
            [("SUNO_VIDEO_COVER_RETENTION".into(), "mkv".into())]
                .into_iter()
                .collect();
        assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());
    }

    #[test]
    fn animated_cover_compression_level_enforces_zero_to_six() {
        // The top of the valid range is accepted from the config file.
        let cfg = Config::from_toml(
            "[defaults]\nanimated_cover_compression_level = 6\n[accounts.alice]\n",
        )
        .unwrap();
        assert_eq!(
            cfg.resolve("alice", None, &no_env(), &no_flags())
                .unwrap()
                .animated_cover_webp
                .compression_level,
            6
        );

        // One past the top is rejected.
        let cfg = Config::from_toml(
            "[defaults]\nanimated_cover_compression_level = 7\n[accounts.alice]\n",
        )
        .unwrap();
        assert!(cfg.resolve("alice", None, &no_env(), &no_flags()).is_err());

        // The same ceiling is enforced for an env override.
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
        let bad_env: HashMap<String, String> =
            [("SUNO_ANIMATED_COVER_COMPRESSION_LEVEL".into(), "7".into())]
                .into_iter()
                .collect();
        assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());

        // A non-integer env value is a config error, not a panic.
        let junk_env: HashMap<String, String> =
            [("SUNO_ANIMATED_COVER_MAX_FPS".into(), "abc".into())]
                .into_iter()
                .collect();
        assert!(cfg.resolve("alice", None, &junk_env, &no_flags()).is_err());
    }

    #[test]
    fn animated_cover_max_width_defaults_to_native() {
        // With nothing configured, the width cap is None (source width).
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
        assert_eq!(
            cfg.resolve("alice", None, &no_env(), &no_flags())
                .unwrap()
                .animated_cover_webp
                .max_width,
            None
        );
    }

    #[test]
    fn text_sidecars_default_off_and_follow_precedence() {
        // Both compiled defaults are off.
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert!(!eff.details_sidecar);
        assert!(!eff.lyrics_sidecar);

        let toml = r#"
            [defaults]
            details_sidecar = true

            [accounts.alice.sources.liked]
            details_sidecar = false
        "#;
        let cfg = Config::from_toml(toml).unwrap();

        // File default turns details on for an unscoped resolve; lyrics stays off.
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert!(eff.details_sidecar);
        assert!(!eff.lyrics_sidecar);

        // Per-source file setting overrides the file default.
        let eff = cfg
            .resolve("alice", Some("liked"), &no_env(), &no_flags())
            .unwrap();
        assert!(!eff.details_sidecar);

        // Env overrides file (both flags), and the flag overrides env.
        let env: HashMap<String, String> = [
            ("SUNO_DETAILS_SIDECAR".into(), "true".into()),
            ("SUNO_LYRICS_SIDECAR".into(), "true".into()),
        ]
        .into_iter()
        .collect();
        let eff = cfg
            .resolve("alice", Some("liked"), &env, &no_flags())
            .unwrap();
        assert!(eff.details_sidecar);
        assert!(eff.lyrics_sidecar);

        let flags = FlagOverrides {
            lyrics_sidecar: Some(false),
            ..Default::default()
        };
        let eff = cfg.resolve("alice", Some("liked"), &env, &flags).unwrap();
        assert!(eff.details_sidecar);
        assert!(!eff.lyrics_sidecar);
    }

    #[test]
    fn invalid_env_bool_errors() {
        let toml = "[accounts.alice]\n";
        let cfg = Config::from_toml(toml).unwrap();
        let env: HashMap<String, String> = [("SUNO_ANIMATED_COVERS".into(), "yes".into())]
            .into_iter()
            .collect();
        assert!(cfg.resolve("alice", None, &env, &no_flags()).is_err());
    }

    #[test]
    fn unknown_account_errors() {
        let cfg = Config::from_toml("").unwrap();
        assert!(cfg.resolve("nobody", None, &no_env(), &no_flags()).is_err());
    }

    #[test]
    fn validation_nested_roots() {
        let toml = r#"
            [accounts.alice]
            root = "/music"

            [accounts.bob]
            root = "/music/bob"
        "#;
        assert!(Config::from_toml(toml).is_err());
    }

    #[test]
    fn validation_non_nested_roots_ok() {
        let toml = r#"
            [accounts.alice]
            root = "/music/alice"

            [accounts.bob]
            root = "/music/bob"
        "#;
        assert!(Config::from_toml(toml).is_ok());
    }

    #[test]
    fn invalid_toml_errors() {
        assert!(Config::from_toml("not valid toml ][").is_err());
    }

    #[test]
    fn duplicate_account_label_errors() {
        // The TOML spec prohibits duplicate keys; the parser must reject this.
        let toml = "
            [accounts.alice]
            token = \"tok1\"

            [accounts.alice]
            token = \"tok2\"
        ";
        assert!(Config::from_toml(toml).is_err());
    }

    #[test]
    fn parse_error_does_not_echo_token() {
        // A malformed token line must not include the raw value in the error.
        let toml = "[accounts.alice]\ntoken = \"unterminated\n";
        let err = Config::from_toml(toml).unwrap_err().to_string();
        assert!(!err.contains("unterminated"), "error leaked token: {err}");
    }

    #[test]
    fn validation_env_prefix_collision_errors() {
        // 'my-lib' and 'my_lib' both map to SUNO_MY_LIB_* and must be rejected.
        let toml = "
            [accounts.my-lib]
            [accounts.my_lib]
        ";
        assert!(Config::from_toml(toml).is_err());
    }

    #[test]
    fn audio_format_display_roundtrip() {
        for fmt in [AudioFormat::Mp3, AudioFormat::Flac, AudioFormat::Wav] {
            let s = fmt.to_string();
            assert_eq!(s.parse::<AudioFormat>().unwrap(), fmt);
        }
    }

    #[test]
    fn naming_template_follows_precedence() {
        let toml = r#"
            [defaults]
            naming_template = "{title}"

            [accounts.alice]
            naming_template = "{creator}/{title}"

            [accounts.alice.sources.liked]
            naming_template = "{handle}/{title} [{id8}]"
        "#;
        let cfg = Config::from_toml(toml).unwrap();

        // Per-source wins over account.
        let eff = cfg
            .resolve("alice", Some("liked"), &no_env(), &no_flags())
            .unwrap();
        assert_eq!(eff.naming_template, "{handle}/{title} [{id8}]");

        // Account wins over defaults.
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(eff.naming_template, "{creator}/{title}");

        // Env overrides file.
        let env: HashMap<String, String> = [("SUNO_NAMING_TEMPLATE".into(), "{id}".into())]
            .into_iter()
            .collect();
        let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.naming_template, "{id}");

        // Flag overrides env.
        let flags = FlagOverrides {
            naming_template: Some("{title}/{id8}".into()),
            ..Default::default()
        };
        let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
        assert_eq!(eff.naming_template, "{title}/{id8}");
    }

    #[test]
    fn character_set_follows_precedence() {
        let toml = r#"
            [defaults]
            character_set = "ascii"

            [accounts.alice]
        "#;
        let cfg = Config::from_toml(toml).unwrap();

        // File default applies.
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(eff.character_set, CharacterSet::Ascii);

        // Env overrides file.
        let env: HashMap<String, String> = [("SUNO_CHARACTER_SET".into(), "unicode".into())]
            .into_iter()
            .collect();
        let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.character_set, CharacterSet::Unicode);

        // Flag overrides env.
        let flags = FlagOverrides {
            character_set: Some(CharacterSet::Ascii),
            ..Default::default()
        };
        let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
        assert_eq!(eff.character_set, CharacterSet::Ascii);
    }

    #[test]
    fn invalid_character_set_env_errors() {
        let toml = "[accounts.alice]\n";
        let cfg = Config::from_toml(toml).unwrap();
        let env: HashMap<String, String> = [("SUNO_CHARACTER_SET".into(), "utf8".into())]
            .into_iter()
            .collect();
        assert!(cfg.resolve("alice", None, &env, &no_flags()).is_err());
    }

    #[test]
    fn areas_parse_full_table() {
        let toml = r#"
            [accounts.alice]
            token = "t"
            [accounts.alice.areas]
            library = "off"
            liked = "copy"
            playlists = "mirror"
            [accounts.alice.areas.playlist]
            "pl_abc123" = "mirror"
            "pl_def456" = "copy"
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let areas = cfg.accounts["alice"].areas.as_ref().unwrap();
        assert_eq!(areas.library, Some(AreaMode::Off));
        assert_eq!(areas.liked, Some(SourceMode::Copy));
        assert_eq!(areas.playlists, Some(SourceMode::Mirror));
        assert_eq!(areas.playlist["pl_abc123"], SourceMode::Mirror);
        assert_eq!(areas.playlist["pl_def456"], SourceMode::Copy);
    }

    #[test]
    fn album_overrides_parse_and_resolve() {
        let toml = r#"
            [accounts.alice]
            token = "t"
            [accounts.alice.albums]
            "root_abc123" = "Preferred Name"
            "root_def456" = "Another Album"
            "root_blank" = "   "
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(
            cfg.accounts["alice"].albums["root_abc123"],
            "Preferred Name"
        );
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(eff.album_overrides["root_abc123"], "Preferred Name");
        assert_eq!(eff.album_overrides["root_def456"], "Another Album");
        // A blank value is dropped so it can never blank an album.
        assert!(!eff.album_overrides.contains_key("root_blank"));
    }

    #[test]
    fn album_overrides_absent_by_default() {
        let cfg = Config::from_toml("[accounts.alice]\ntoken = \"t\"\n").unwrap();
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert!(eff.album_overrides.is_empty());
    }

    #[test]
    fn areas_library_accepts_copy_and_mirror() {
        for (raw, expect) in [
            ("copy", AreaMode::Mode(SourceMode::Copy)),
            ("mirror", AreaMode::Mode(SourceMode::Mirror)),
        ] {
            let toml =
                format!("[accounts.a]\ntoken = \"t\"\n[accounts.a.areas]\nlibrary = \"{raw}\"\n");
            let cfg = Config::from_toml(&toml).unwrap();
            assert_eq!(
                cfg.accounts["a"].areas.as_ref().unwrap().library,
                Some(expect)
            );
        }
    }

    #[test]
    fn areas_bad_mode_errors() {
        let toml = "[accounts.a]\ntoken = \"t\"\n[accounts.a.areas]\nliked = \"miror\"\n";
        assert!(Config::from_toml(toml).is_err());
    }

    #[test]
    fn areas_bad_playlist_mode_errors() {
        let toml = "[accounts.a]\ntoken = \"t\"\n[accounts.a.areas.playlist]\n\"pl1\" = \"off\"\n";
        // `off` is a library-only value; a per-playlist entry must be copy/mirror.
        assert!(Config::from_toml(toml).is_err());
    }

    #[test]
    fn areas_unknown_field_errors() {
        // D7: a mistyped key (libary) is a parse error, not a silent no-op.
        let toml = "[accounts.a]\ntoken = \"t\"\n[accounts.a.areas]\nlibary = \"off\"\n";
        assert!(Config::from_toml(toml).is_err());
    }

    #[test]
    fn areas_absent_is_none() {
        let toml = "[accounts.a]\ntoken = \"t\"\n";
        assert!(
            Config::from_toml(toml).unwrap().accounts["a"]
                .areas
                .is_none()
        );
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
    fn requires_ffmpeg_mp3_needs_it_only_for_animated_webp() {
        let mut eff = base_settings(AudioFormat::Mp3);
        assert!(!eff.requires_ffmpeg(), "mp3 + no covers = no ffmpeg");
        eff.animated_covers = true;
        assert!(eff.requires_ffmpeg(), "mp3 + animated webp = needs ffmpeg");
        // `both` retention keeps the raw mp4 AND the transcoded webp, so ffmpeg
        // is still required to produce the webp.
        eff.raw_animated_cover = true;
        assert!(
            eff.requires_ffmpeg(),
            "mp3 + both (webp + raw mp4) = needs ffmpeg"
        );
        eff.animated_covers = false;
        assert!(!eff.requires_ffmpeg(), "mp3 + raw mp4 only = no ffmpeg");
    }

    #[test]
    fn requires_ffmpeg_wav_mirrors_mp3_logic() {
        let mut eff = base_settings(AudioFormat::Wav);
        assert!(!eff.requires_ffmpeg(), "wav + no covers = no ffmpeg");
        eff.animated_covers = true;
        assert!(eff.requires_ffmpeg(), "wav + animated webp = needs ffmpeg");
    }
}
