//! The TOML input shape and its parse-time validation.
//!
//! These are the deserialisation targets; [`Config`] is the top-level file,
//! and [`Config::from_toml`] parses and validates it (no IO).

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::naming::CharacterSet;
use crate::vocab::{AudioFormat, SourceMode, StemFormat, VideoCoverRetention};

use super::label_to_env;

/// The overridable settings block, shared verbatim by every precedence tier.
///
/// A new knob is added here once and every tier that flattens it ([`Defaults`],
/// [`AccountConfig`], [`SourceConfig`], and the CLI [`FlagOverrides`]) gains it,
/// rather than being mirrored where forgetting one would silently drop it.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Settings {
    pub format: Option<AudioFormat>,
    pub concurrency: Option<u32>,
    pub retries: Option<u32>,
    pub min_newest: Option<u32>,
    /// The command whose stdout mints a token. Resolved from the
    /// `SUNO_[<LABEL>_]TOKEN_COMMAND` env tiers then the config keys. There is
    /// deliberately no `--token-command` flag, so it is never read from
    /// [`FlagOverrides`]; set it in config or the environment.
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
    /// Whether a single-track (lone) lineage album is given a track number. When
    /// unset it defaults to `true`; `false` leaves singletons unnumbered so a
    /// `{track2}` prefix does not decorate a standalone song.
    pub number_singletons: Option<bool>,
}

/// Global default settings applied when no account or source override applies.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Defaults {
    #[serde(flatten)]
    pub settings: Settings,
}

/// Per-source overridable settings within an account.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SourceConfig {
    #[serde(flatten)]
    pub settings: Settings,
}

/// Configuration for a single named account.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// (`<root_id> = "Preferred Name"`). Account-wide, never per-source, since
    /// album identity is the lineage root. An empty or whitespace-only value is
    /// ignored, so a stray key cannot blank an album.
    #[serde(default)]
    pub albums: HashMap<String, String>,
    /// Clip ids (or unique id prefixes, e.g. the 8-char code from a filename)
    /// flagged as their lineage album's lead: each is promoted to track 1,
    /// shifting the rest down. Account-wide; a clip's album is inferred from its
    /// resolved root, so the album is never named here.
    #[serde(default)]
    pub lead_tracks: Vec<String>,
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

#[cfg(feature = "schema")]
impl schemars::JsonSchema for AreaMode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AreaMode".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "enum": ["off", "copy", "mirror"],
            "description": "Deletion mode for the library area: 'off' arms deletion of \
                library-exclusive files, 'copy' is additive, 'mirror' deletes.",
        })
    }
}

/// Per-area mode selection for an account.
///
/// `library` accepts `off`/`copy`/`mirror`; `liked` and `playlists` accept
/// `copy`/`mirror`; `playlist` overrides individual playlists by Suno id.
/// `deny_unknown_fields` turns a mistyped key into a parse error rather than a
/// silent no-op; the `playlist` map's dynamic ids can't use it, but its closed
/// [`SourceMode`] values still reject a bad mode at parse time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
