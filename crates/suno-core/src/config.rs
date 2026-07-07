//! Configuration model and precedence resolution.
//!
//! Parses a TOML string and merges in environment variables and CLI flag
//! overrides supplied by the caller. Performs no disk or environment IO.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::str::FromStr;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::naming::CharacterSet;
use crate::vocab::{AudioFormat, SourceMode, StemFormat, VideoCoverRetention, WebpEncodeSettings};

/// The overridable settings block, shared verbatim by every precedence tier.
///
/// One declaration is the whole point: a new knob is added here once and every
/// tier that flattens it (global [`Defaults`], per-account [`AccountConfig`],
/// per-source [`SourceConfig`], and the CLI [`FlagOverrides`]) gains it, instead
/// of being mirrored across the structs where forgetting one silently drops the
/// setting from a tier.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Settings {
    pub format: Option<AudioFormat>,
    pub concurrency: Option<u32>,
    pub retries: Option<u32>,
    pub min_newest: Option<u32>,
    /// The command whose stdout mints a token. Resolved from the
    /// `SUNO_[<LABEL>_]TOKEN_COMMAND` env tiers then the per-source, per-account,
    /// and global `token_command` config keys. There is deliberately no
    /// `--token-command` flag, so setting this on [`FlagOverrides::settings`] is
    /// intentionally never read by [`Config::resolve`]; configure it in
    /// `[defaults]`/`[accounts.<label>]`/`[sources.<name>]` or the environment.
    pub token_command: Option<String>,
    pub animated_covers: Option<bool>,
    pub video_cover_retention: Option<VideoCoverRetention>,
    pub animated_cover_quality: Option<u8>,
    pub animated_cover_max_fps: Option<u32>,
    pub animated_cover_max_width: Option<u32>,
    pub animated_cover_compression_level: Option<u8>,
    pub animated_cover_lossless: Option<bool>,
    pub details_sidecar: Option<bool>,
    pub lyrics_sidecar: Option<bool>,
    pub lrc_sidecar: Option<bool>,
    pub video_mp4: Option<bool>,
    pub download_stems: Option<bool>,
    pub stem_format: Option<StemFormat>,
    pub naming_template: Option<String>,
    pub character_set: Option<CharacterSet>,
}

/// Global default settings applied when no account or source override applies.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Defaults {
    #[serde(flatten)]
    pub settings: Settings,
}

/// Per-source overridable settings within an account.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SourceConfig {
    #[serde(flatten)]
    pub settings: Settings,
}

/// Configuration for a single named account.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountConfig {
    pub token: Option<String>,
    pub root: Option<String>,
    /// Optional Suno user id to assert this account authenticates as, refusing
    /// to run on a mismatch (a belt-and-braces check alongside the on-disk
    /// owner pin in the lineage store).
    pub account_id: Option<String>,
    #[serde(flatten)]
    pub settings: Settings,
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

/// Convert an account label to its environment variable prefix, mirroring the
/// per-account keys the resolver reads: `my-lib` becomes `MY_LIB` for lookups
/// like `SUNO_MY_LIB_TOKEN`.
pub fn label_to_env(label: &str) -> String {
    label.to_ascii_uppercase().replace('-', "_")
}

/// CLI flag overrides passed to [`Config::resolve`]. `None` means the flag
/// was not provided.
///
/// The shared [`Settings`] block is nested rather than mirrored; only `token`
/// is carried top-level, because it is the one identity field with a global
/// `--token` flag. Note there is no `--token-command` flag, so
/// `settings.token_command` is never populated from the CLI (see [`Settings`]).
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
}

impl EffectiveSettings {
    /// Returns `true` when these settings require ffmpeg to be on `PATH`.
    ///
    /// Lossless output (FLAC or ALAC) transcodes from the WAV render, and an
    /// animated WebP cover transcodes MP4→WebP, so either needs ffmpeg. Keeping
    /// the raw MP4 alongside the WebP (the `both` retention) still produces the
    /// WebP, so `animated_covers` alone decides it; a raw-MP4-only run, or a
    /// plain MP3/WAV run with no animated covers, needs no ffmpeg.
    pub fn requires_ffmpeg(&self) -> bool {
        matches!(self.format, AudioFormat::Flac | AudioFormat::Alac) || self.animated_covers
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
        assert_eq!(cfg.defaults.settings.format, Some(AudioFormat::Mp3));
        assert_eq!(cfg.defaults.settings.concurrency, Some(8));
        assert_eq!(cfg.defaults.settings.retries, Some(5));
        assert_eq!(cfg.defaults.settings.min_newest, Some(2));
        assert_eq!(cfg.defaults.settings.animated_covers, Some(true));
        assert_eq!(
            cfg.defaults.settings.video_cover_retention,
            Some(VideoCoverRetention::Both)
        );
        assert_eq!(cfg.defaults.settings.animated_cover_quality, Some(85));
        assert_eq!(cfg.defaults.settings.animated_cover_max_fps, Some(18));
        assert_eq!(cfg.defaults.settings.animated_cover_max_width, Some(720));
        assert_eq!(
            cfg.defaults.settings.animated_cover_compression_level,
            Some(4)
        );
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

    /// Guards that every [`Settings`] field is threaded through
    /// [`Config::resolve`]. The `sentinel` literal has no `..Default::default()`,
    /// so adding a `Settings` field is a compile error here until it is given a
    /// distinct, non-compiled-default value — forcing the author to this test.
    /// Resolving with empty account/source/flags then asserts each
    /// [`EffectiveSettings`] scalar reflects the sentinel.
    ///
    /// The per-field `assert_eq!`s below are maintained by hand: this test proves
    /// a new field is *named* in the sentinel, not that it is *asserted*.
    /// `EffectiveSettings` deriving no `Default` compile-forces `resolve` to
    /// populate every output field, so the residual gap is only a field that is
    /// resolved and constructed but left unasserted here.
    /// `animated_covers`/`video_cover_retention` are coupled (retention, when
    /// set, drives both `animated_covers` and `raw_animated_cover`); their
    /// precedence is proven by the dedicated tests.
    #[test]
    fn resolve_reflects_every_settings_field() {
        let sentinel = Settings {
            format: Some(AudioFormat::Mp3),
            concurrency: Some(99),
            retries: Some(98),
            min_newest: Some(42),
            token_command: Some("sentinel-token-cmd".into()),
            animated_covers: Some(true),
            video_cover_retention: Some(VideoCoverRetention::Both),
            animated_cover_quality: Some(77),
            animated_cover_max_fps: Some(13),
            animated_cover_max_width: Some(333),
            animated_cover_compression_level: Some(3),
            animated_cover_lossless: Some(true),
            details_sidecar: Some(true),
            lyrics_sidecar: Some(true),
            lrc_sidecar: Some(true),
            video_mp4: Some(true),
            download_stems: Some(true),
            stem_format: Some(StemFormat::Mp3),
            naming_template: Some("SENTINEL/{id}".into()),
            character_set: Some(CharacterSet::Ascii),
        };
        let cfg = Config {
            defaults: Defaults { settings: sentinel },
            accounts: HashMap::from([("alice".to_owned(), AccountConfig::default())]),
        };

        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();

        assert_eq!(eff.format, AudioFormat::Mp3);
        assert_eq!(eff.concurrency, 99);
        assert_eq!(eff.retries, 98);
        assert_eq!(eff.min_newest, 42);
        assert_eq!(eff.token_command.as_deref(), Some("sentinel-token-cmd"));
        // Retention `both` drives both webp and mp4 retention.
        assert!(eff.animated_covers);
        assert!(eff.raw_animated_cover);
        assert_eq!(eff.video_cover_retention, VideoCoverRetention::Both);
        assert_eq!(eff.animated_cover_webp.quality, 77);
        assert_eq!(eff.animated_cover_webp.max_fps, 13);
        assert_eq!(eff.animated_cover_webp.max_width, Some(333));
        assert_eq!(eff.animated_cover_webp.compression_level, 3);
        assert!(eff.animated_cover_webp.lossless);
        assert!(eff.details_sidecar);
        assert!(eff.lyrics_sidecar);
        assert!(eff.lrc_sidecar);
        assert!(eff.video_mp4);
        assert!(eff.download_stems);
        assert_eq!(eff.stem_format, StemFormat::Mp3);
        assert_eq!(eff.naming_template, "SENTINEL/{id}");
        assert_eq!(eff.character_set, CharacterSet::Ascii);
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
            settings: Settings {
                format: Some(AudioFormat::Wav),
                ..Default::default()
            },
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
            settings: Settings {
                animated_covers: Some(false),
                ..Default::default()
            },
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
            settings: Settings {
                video_mp4: Some(false),
                ..Default::default()
            },
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
            settings: Settings {
                download_stems: Some(false),
                ..Default::default()
            },
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
            settings: Settings {
                stem_format: Some(StemFormat::Wav),
                ..Default::default()
            },
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
            settings: Settings {
                animated_cover_quality: Some(95),
                animated_cover_max_width: Some(512),
                animated_cover_compression_level: Some(4),
                ..Default::default()
            },
            ..Default::default()
        };
        let eff = cfg.resolve("alice", Some("liked"), &env, &flags).unwrap();
        assert_eq!(eff.animated_cover_webp.quality, 95);
        assert_eq!(eff.animated_cover_webp.max_width, Some(512));
        assert_eq!(eff.animated_cover_webp.compression_level, 4);

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
    fn animated_cover_compression_level_enforces_zero_to_four() {
        // The top of the valid range is accepted from the config file. Effort is
        // capped at 4 because level 6 costs many times the time for no size gain.
        let cfg = Config::from_toml(
            "[defaults]\nanimated_cover_compression_level = 4\n[accounts.alice]\n",
        )
        .unwrap();
        assert_eq!(
            cfg.resolve("alice", None, &no_env(), &no_flags())
                .unwrap()
                .animated_cover_webp
                .compression_level,
            4
        );

        // One past the top is rejected.
        let cfg = Config::from_toml(
            "[defaults]\nanimated_cover_compression_level = 5\n[accounts.alice]\n",
        )
        .unwrap();
        assert!(cfg.resolve("alice", None, &no_env(), &no_flags()).is_err());

        // The same ceiling is enforced for an env override.
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
        let bad_env: HashMap<String, String> =
            [("SUNO_ANIMATED_COVER_COMPRESSION_LEVEL".into(), "5".into())]
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
    fn animated_cover_lossless_defaults_off_and_follows_precedence() {
        // The compiled default is a bounded lossy encode that fits the FLAC
        // picture cap: quality 90, effort 4, lossy (not lossless).
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(eff.animated_cover_webp.quality, 90);
        assert_eq!(eff.animated_cover_webp.compression_level, 4);
        assert!(!eff.animated_cover_webp.lossless);

        // Config opts in; an env value and a flag each override in turn.
        let cfg =
            Config::from_toml("[defaults]\nanimated_cover_lossless = true\n[accounts.alice]\n")
                .unwrap();
        assert!(
            cfg.resolve("alice", None, &no_env(), &no_flags())
                .unwrap()
                .animated_cover_webp
                .lossless
        );
        let env: HashMap<String, String> =
            [("SUNO_ANIMATED_COVER_LOSSLESS".into(), "false".into())]
                .into_iter()
                .collect();
        assert!(
            !cfg.resolve("alice", None, &env, &no_flags())
                .unwrap()
                .animated_cover_webp
                .lossless
        );
        let flags = FlagOverrides {
            settings: Settings {
                animated_cover_lossless: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(
            cfg.resolve("alice", None, &env, &flags)
                .unwrap()
                .animated_cover_webp
                .lossless
        );

        // A non-boolean value is a config error, not a silent default.
        let bad_env: HashMap<String, String> =
            [("SUNO_ANIMATED_COVER_LOSSLESS".into(), "maybe".into())]
                .into_iter()
                .collect();
        assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());
    }

    #[test]
    fn animated_cover_max_width_defaults_to_bounded() {
        // With nothing configured, the width cap is the bounded default (640 px)
        // so the embedded animated cover reliably fits the FLAC picture cap.
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
        assert_eq!(
            cfg.resolve("alice", None, &no_env(), &no_flags())
                .unwrap()
                .animated_cover_webp
                .max_width,
            Some(640)
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
            settings: Settings {
                lyrics_sidecar: Some(false),
                ..Default::default()
            },
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

    #[test]
    fn format_follows_precedence() {
        let toml = r#"
            [defaults]
            format = "wav"

            [accounts.alice]
            format = "mp3"

            [accounts.alice.sources.liked]
            format = "alac"
        "#;
        let cfg = Config::from_toml(toml).unwrap();

        // Compiled default (FLAC) when nothing is set.
        let bare = Config::from_toml("[accounts.bob]\n").unwrap();
        assert_eq!(
            bare.resolve("bob", None, &no_env(), &no_flags())
                .unwrap()
                .format,
            AudioFormat::Flac
        );

        // Per-source wins over account and defaults.
        let eff = cfg
            .resolve("alice", Some("liked"), &no_env(), &no_flags())
            .unwrap();
        assert_eq!(eff.format, AudioFormat::Alac);

        // Account wins over defaults.
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(eff.format, AudioFormat::Mp3);

        // Global env overrides file.
        let env: HashMap<String, String> =
            [("SUNO_FORMAT".into(), "wav".into())].into_iter().collect();
        let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.format, AudioFormat::Wav);

        // Per-account env overrides global env.
        let env: HashMap<String, String> = [
            ("SUNO_FORMAT".into(), "wav".into()),
            ("SUNO_ALICE_FORMAT".into(), "flac".into()),
        ]
        .into_iter()
        .collect();
        let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.format, AudioFormat::Flac);

        // Flag overrides env.
        let flags = FlagOverrides {
            settings: Settings {
                format: Some(AudioFormat::Alac),
                ..Default::default()
            },
            ..Default::default()
        };
        let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
        assert_eq!(eff.format, AudioFormat::Alac);

        // An unknown env value is a config error, never a silent default.
        let bad_env: HashMap<String, String> = [("SUNO_FORMAT".into(), "aiff".into())]
            .into_iter()
            .collect();
        assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());
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
            settings: Settings {
                naming_template: Some("{title}/{id8}".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
        assert_eq!(eff.naming_template, "{title}/{id8}");
    }

    #[test]
    fn min_newest_follows_precedence() {
        // `min_newest` is the deletion-safety floor: every tier boundary in its
        // precedence (flag > per-account env > global env > source > account >
        // defaults > compiled 1) must hold exactly.
        let toml = r#"
            [defaults]
            min_newest = 5

            [accounts.alice]
            min_newest = 7

            [accounts.alice.sources.liked]
            min_newest = 9
        "#;
        let cfg = Config::from_toml(toml).unwrap();

        // Compiled default when nothing is set.
        let bare = Config::from_toml("[accounts.bob]\n").unwrap();
        assert_eq!(
            bare.resolve("bob", None, &no_env(), &no_flags())
                .unwrap()
                .min_newest,
            1
        );

        // Per-source wins over account and defaults.
        let eff = cfg
            .resolve("alice", Some("liked"), &no_env(), &no_flags())
            .unwrap();
        assert_eq!(eff.min_newest, 9);

        // Account wins over defaults.
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert_eq!(eff.min_newest, 7);

        // Global env overrides file.
        let env: HashMap<String, String> = [("SUNO_MIN_NEWEST".into(), "11".into())]
            .into_iter()
            .collect();
        let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.min_newest, 11);

        // Per-account env overrides global env.
        let env: HashMap<String, String> = [
            ("SUNO_MIN_NEWEST".into(), "11".into()),
            ("SUNO_ALICE_MIN_NEWEST".into(), "13".into()),
        ]
        .into_iter()
        .collect();
        let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
        assert_eq!(eff.min_newest, 13);

        // Flag overrides env.
        let flags = FlagOverrides {
            settings: Settings {
                min_newest: Some(15),
                ..Default::default()
            },
            ..Default::default()
        };
        let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
        assert_eq!(eff.min_newest, 15);

        // An invalid env value is a config error, never a silently lowered floor.
        let bad_env: HashMap<String, String> = [("SUNO_MIN_NEWEST".into(), "notnum".into())]
            .into_iter()
            .collect();
        assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());
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
            settings: Settings {
                character_set: Some(CharacterSet::Ascii),
                ..Default::default()
            },
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
}
