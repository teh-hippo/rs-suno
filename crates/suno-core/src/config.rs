//! Configuration model and precedence resolution.
//!
//! Parses a TOML string and merges in environment variables and CLI flag
//! overrides supplied by the caller. Performs no disk or environment IO.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

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

/// Global default settings applied when no account or source override applies.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Defaults {
    pub format: Option<AudioFormat>,
    pub concurrency: Option<u32>,
    pub retries: Option<u32>,
    pub min_newest: Option<u32>,
    pub animated_covers: Option<bool>,
}

/// Per-source overridable settings within an account.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SourceConfig {
    pub format: Option<AudioFormat>,
    pub concurrency: Option<u32>,
    pub retries: Option<u32>,
    pub min_newest: Option<u32>,
    pub animated_covers: Option<bool>,
}

/// Configuration for a single named account.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountConfig {
    pub token: Option<String>,
    pub root: Option<String>,
    pub format: Option<AudioFormat>,
    pub concurrency: Option<u32>,
    pub retries: Option<u32>,
    pub min_newest: Option<u32>,
    pub animated_covers: Option<bool>,
    #[serde(default)]
    pub sources: HashMap<String, SourceConfig>,
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

        let token = flags
            .token
            .clone()
            .or_else(|| env.get(&format!("SUNO_{label_env}_TOKEN")).cloned())
            .or_else(|| env.get("SUNO_TOKEN").cloned())
            .or_else(|| acc.token.clone());

        Ok(EffectiveSettings {
            token,
            format,
            concurrency,
            retries,
            min_newest,
            animated_covers,
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

/// Convert an account label to its environment variable prefix.
///
/// `my-lib` becomes `MY_LIB`.
fn label_to_env(label: &str) -> String {
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
}

/// Resolved effective settings for one account/source combination.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveSettings {
    pub token: Option<String>,
    pub format: AudioFormat,
    pub concurrency: u32,
    pub retries: u32,
    pub min_newest: u32,
    pub animated_covers: bool,
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
    fn parse_defaults_section() {
        let toml = r#"
            [defaults]
            format = "mp3"
            concurrency = 8
            retries = 5
            min_newest = 2
            animated_covers = true
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(cfg.defaults.format, Some(AudioFormat::Mp3));
        assert_eq!(cfg.defaults.concurrency, Some(8));
        assert_eq!(cfg.defaults.retries, Some(5));
        assert_eq!(cfg.defaults.min_newest, Some(2));
        assert_eq!(cfg.defaults.animated_covers, Some(true));
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
                format: AudioFormat::Flac,
                concurrency: 4,
                retries: 3,
                min_newest: 1,
                animated_covers: false,
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

        // flag overrides env
        let flags = FlagOverrides {
            token: Some("flag_tok".into()),
            ..Default::default()
        };
        let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
        assert_eq!(eff.token.as_deref(), Some("flag_tok"));
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
}
