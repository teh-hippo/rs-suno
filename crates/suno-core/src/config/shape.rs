//! The TOML input shape and its parse-time validation.
//!
//! These are the deserialisation targets; [`Config`] is the top-level file,
//! and [`Config::from_toml`] parses and validates it (no IO).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

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
    #[cfg_attr(feature = "schema", schemars(range(max = u32::MAX)))]
    pub concurrency: Option<u32>,
    #[cfg_attr(feature = "schema", schemars(range(max = u32::MAX)))]
    pub retries: Option<u32>,
    #[cfg_attr(feature = "schema", schemars(range(max = u32::MAX)))]
    pub min_newest: Option<u32>,
    /// The command whose stdout mints a token. Resolved from the
    /// `SUNO_[<LABEL>_]TOKEN_COMMAND` env tiers then the config keys. There is
    /// deliberately no `--token-command` flag, so it is never read from
    /// [`FlagOverrides`]; set it in config or the environment.
    pub token_command: Option<String>,
    pub animated_covers: Option<bool>,
    pub video_cover_retention: Option<VideoCoverRetention>,
    #[cfg_attr(feature = "schema", schemars(range(min = 0, max = 100)))]
    pub animated_cover_quality: Option<u8>,
    #[cfg_attr(feature = "schema", schemars(range(max = u32::MAX)))]
    pub animated_cover_max_fps: Option<u32>,
    #[cfg_attr(feature = "schema", schemars(range(max = u32::MAX)))]
    pub animated_cover_max_width: Option<u32>,
    #[cfg_attr(feature = "schema", schemars(range(min = 0, max = 4)))]
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

/// The TOML keys of every [`Settings`] field, in struct order.
///
/// Kept in lockstep with [`Settings`] above: a new knob must be added here too.
/// `#[serde(flatten)]` embeds `Settings` into each precedence tier, which
/// disables serde's own `deny_unknown_fields`, so a mistyped knob is otherwise
/// silently dropped. [`reject_unknown_keys`] uses this set to reject typos the
/// way `[areas]` already does. The `settings_keys_match_struct` test guards the
/// mirror against drift.
const SETTINGS_KEYS: &[&str] = &[
    "format",
    "concurrency",
    "retries",
    "min_newest",
    "token_command",
    "animated_covers",
    "video_cover_retention",
    "animated_cover_quality",
    "animated_cover_max_fps",
    "animated_cover_max_width",
    "animated_cover_compression_level",
    "animated_cover_lossless",
    "details_sidecar",
    "lyrics_sidecar",
    "lrc_sidecar",
    "video_mp4",
    "download_stems",
    "stem_format",
    "naming_template",
    "character_set",
    "number_singletons",
];

/// The structural (non-[`Settings`]) keys of an `[accounts.<label>]` table.
const ACCOUNT_STRUCTURAL_KEYS: &[&str] = &[
    "token",
    "root",
    "account_id",
    "sources",
    "areas",
    "albums",
    "lead_tracks",
];

/// The keys accepted at the top level of the config document.
const TOP_LEVEL_KEYS: &[&str] = &["defaults", "accounts"];

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
        let config: Self = toml::from_str(toml_str).map_err(redact_toml_error)?;
        reject_unknown_keys(toml_str)?;
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
                // Compare lexically normalised roots so `./music` and `music`
                // (the same directory) are not treated as disjoint, which would
                // punch a cross-account deletion-overlap hole through the guard.
                let a = normalise_root(root_a);
                let b = normalise_root(root_b);
                if a.starts_with(&b) || b.starts_with(&a) {
                    return Err(Error::Config(format!(
                        "account roots nest: '{label_a}' ({root_a}) and '{label_b}' ({root_b})"
                    )));
                }
            }
        }

        // Reject an empty or whitespace-only naming template at any tier:
        // resolution would otherwise carry the empty string through rather than
        // falling back to the default, yielding blank path components. A numeric
        // zero (e.g. `min_newest = 0`) stays legal; this guards only the string
        // template.
        let templates = std::iter::once(&self.defaults.settings).chain(
            self.accounts.values().flat_map(|acc| {
                std::iter::once(&acc.settings).chain(acc.sources.values().map(|s| &s.settings))
            }),
        );
        for settings in templates {
            if let Some(template) = settings.naming_template.as_deref()
                && template.trim().is_empty()
            {
                return Err(Error::Config(
                    "naming_template must not be empty or whitespace-only".into(),
                ));
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

/// Convert a `toml` parse error into an [`Error::Config`], stripping the
/// source-context lines (those containing `" | "`) so a token value on the
/// offending line is never echoed back.
fn redact_toml_error(e: toml::de::Error) -> Error {
    let msg = e
        .to_string()
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
}

/// Reject any key not recognised at the top level or at a settings tier.
///
/// `#[serde(flatten)]` embeds [`Settings`] into [`Defaults`], [`AccountConfig`],
/// and [`SourceConfig`], which silently disables serde's `deny_unknown_fields`,
/// so a mistyped knob (e.g. `min_newst`) would be dropped and its setting revert
/// to the compiled default. That is unsafe for the deletion floor (`min_newest`)
/// and misleading for every other knob, so re-parse the document generically and
/// fail loudly, mirroring the hard error `[areas]` already gives. `[areas]`,
/// `[...albums]`, and `lead_tracks` carry dynamic keys and are left to their own
/// typed validation.
fn reject_unknown_keys(toml_str: &str) -> Result<()> {
    let doc: toml::Table = toml::from_str(toml_str).map_err(redact_toml_error)?;

    check_known_keys(&doc, "the top-level table", |k| TOP_LEVEL_KEYS.contains(&k))?;

    if let Some(defaults) = doc.get("defaults").and_then(toml::Value::as_table) {
        check_known_keys(defaults, "[defaults]", |k| SETTINGS_KEYS.contains(&k))?;
    }

    if let Some(accounts) = doc.get("accounts").and_then(toml::Value::as_table) {
        for (label, account) in accounts {
            let Some(account) = account.as_table() else {
                continue;
            };
            let section = format!("[accounts.{label}]");
            check_known_keys(account, &section, |k| {
                ACCOUNT_STRUCTURAL_KEYS.contains(&k) || SETTINGS_KEYS.contains(&k)
            })?;

            if let Some(sources) = account.get("sources").and_then(toml::Value::as_table) {
                for (name, source) in sources {
                    let Some(source) = source.as_table() else {
                        continue;
                    };
                    let section = format!("[accounts.{label}.sources.{name}]");
                    check_known_keys(source, &section, |k| SETTINGS_KEYS.contains(&k))?;
                }
            }
        }
    }

    Ok(())
}

/// Fail with a clear [`Error::Config`] naming the first key in `table` the
/// `allowed` predicate rejects, reported against `section`.
fn check_known_keys(
    table: &toml::Table,
    section: &str,
    allowed: impl Fn(&str) -> bool,
) -> Result<()> {
    for key in table.keys() {
        if !allowed(key.as_str()) {
            return Err(Error::Config(format!("unknown key '{key}' in {section}")));
        }
    }
    Ok(())
}

/// Lexically normalise a configured root for the nesting comparison: strip a
/// leading `./`, collapse repeated separators, drop a trailing separator, and
/// resolve `.`/`..` segments purely textually. This never touches the
/// filesystem (suno-core is IO-free), so it neither follows symlinks nor folds
/// case. Case-insensitive-filesystem folding (Windows, macOS), where `Music`
/// and `music` are the same directory, is a genuine FS concern and is
/// deliberately left as a follow-up.
fn normalise_root(root: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for component in Path::new(root).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // Cannot ascend above a root or drive prefix; drop the `..`.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                // A leading `..` on a relative path is preserved verbatim.
                _ => out.push(component.as_os_str()),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
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

    // --- cfg-6: unknown settings keys are rejected, not silently dropped. ---

    #[test]
    fn misspelled_min_newest_key_is_rejected() {
        // The deletion-floor safety case: a typo must not silently revert
        // `min_newest` to the default of 1. `#[serde(flatten)]` would drop it;
        // the sweep names the offending key instead.
        let err = Config::from_toml("[defaults]\nmin_newst = 50\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("min_newst"),
            "error should name the key: {err}"
        );
        assert!(
            err.contains("[defaults]"),
            "error should name the tier: {err}"
        );
    }

    #[test]
    fn unknown_defaults_key_is_rejected() {
        assert!(Config::from_toml("[defaults]\nbogus = true\n").is_err());
    }

    #[test]
    fn unknown_account_key_is_rejected() {
        let err = Config::from_toml("[accounts.alice]\nroout = \"/music\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("roout"), "error should name the key: {err}");
        assert!(
            err.contains("[accounts.alice]"),
            "error names the tier: {err}"
        );
    }

    #[test]
    fn unknown_source_key_is_rejected() {
        let toml = "[accounts.alice.sources.liked]\nformta = \"mp3\"\n";
        let err = Config::from_toml(toml).unwrap_err().to_string();
        assert!(err.contains("formta"), "error should name the key: {err}");
        assert!(
            err.contains("[accounts.alice.sources.liked]"),
            "error should name the source tier: {err}"
        );
    }

    #[test]
    fn unknown_top_level_table_is_rejected() {
        // A misspelt `[defalts]` drops every default (including the deletion
        // floor), so it too must fail loudly rather than parse to nothing.
        let err = Config::from_toml("[defalts]\nmin_newest = 50\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("defalts"), "error should name the key: {err}");
    }

    #[test]
    fn every_settings_key_is_accepted() {
        // Precision guard: no legitimate knob is rejected by the sweep.
        let toml = r#"
            [defaults]
            format = "flac"
            concurrency = 4
            retries = 3
            min_newest = 1
            token_command = "echo tok"
            animated_covers = true
            video_cover_retention = "both"
            animated_cover_quality = 90
            animated_cover_max_fps = 24
            animated_cover_max_width = 640
            animated_cover_compression_level = 4
            animated_cover_lossless = false
            details_sidecar = true
            lyrics_sidecar = true
            lrc_sidecar = true
            video_mp4 = true
            download_stems = true
            stem_format = "wav"
            naming_template = "{title}"
            character_set = "unicode"
            number_singletons = false
        "#;
        assert!(Config::from_toml(toml).is_ok());
    }

    #[test]
    fn known_structural_account_keys_are_accepted() {
        let toml = r#"
            [accounts.alice]
            token = "t"
            root = "/music/alice"
            account_id = "user_abc"
            format = "mp3"
            lead_tracks = ["abc123"]
            [accounts.alice.areas]
            library = "off"
            [accounts.alice.albums]
            root_xyz = "Greatest Hits"
            [accounts.alice.sources.liked]
            format = "flac"
        "#;
        assert!(Config::from_toml(toml).is_ok());
    }

    #[cfg(feature = "schema")]
    #[test]
    fn settings_keys_mirror_the_struct() {
        // The generated JSON schema enumerates every real `Settings` field, so
        // it is the source of truth the hand-maintained `SETTINGS_KEYS` mirror
        // must track. Guards both directions: a new knob added to the struct but
        // forgotten here (which would wrongly reject a legitimate key), or a key
        // left here after the field is removed.
        let schema = serde_json::to_value(schemars::schema_for!(Settings)).unwrap();
        let schema_keys: std::collections::BTreeSet<&str> = schema["properties"]
            .as_object()
            .expect("Settings schema exposes properties")
            .keys()
            .map(String::as_str)
            .collect();
        let known: std::collections::BTreeSet<&str> = SETTINGS_KEYS.iter().copied().collect();
        assert_eq!(
            schema_keys, known,
            "SETTINGS_KEYS is out of sync with Settings"
        );
    }

    // --- cfg-path: roots are lexically normalised before the nesting check. ---

    #[test]
    fn nested_roots_with_dot_prefix_and_bare_are_rejected() {
        // `./music` and `music` are the same directory; the raw `starts_with`
        // guard missed this, leaving a cross-account deletion-overlap hole.
        let toml = "[accounts.alice]\nroot = \"./music\"\n\n[accounts.bob]\nroot = \"music\"\n";
        assert!(Config::from_toml(toml).is_err());
    }

    #[test]
    fn nested_roots_with_trailing_slash_are_rejected() {
        let toml =
            "[accounts.alice]\nroot = \"/music/\"\n\n[accounts.bob]\nroot = \"/music/alice\"\n";
        assert!(Config::from_toml(toml).is_err());
    }

    #[test]
    fn nested_roots_via_parent_segments_are_rejected() {
        // `sub/../shared` resolves lexically to `shared`, nesting with `shared`.
        let toml =
            "[accounts.alice]\nroot = \"sub/../shared\"\n\n[accounts.bob]\nroot = \"shared\"\n";
        assert!(Config::from_toml(toml).is_err());
    }

    #[test]
    fn disjoint_relative_roots_are_accepted() {
        let toml = "[accounts.alice]\nroot = \"./alice\"\n\n[accounts.bob]\nroot = \"bob\"\n";
        assert!(Config::from_toml(toml).is_ok());
    }

    // --- cfg-7: an empty naming template is rejected; numeric zeros stay legal. ---

    #[test]
    fn empty_naming_template_is_rejected() {
        assert!(Config::from_toml("[defaults]\nnaming_template = \"\"\n").is_err());
    }

    #[test]
    fn whitespace_only_naming_template_is_rejected() {
        assert!(Config::from_toml("[defaults]\nnaming_template = \"   \"\n").is_err());
    }

    #[test]
    fn empty_naming_template_in_account_is_rejected() {
        assert!(Config::from_toml("[accounts.alice]\nnaming_template = \"\"\n").is_err());
    }

    #[test]
    fn empty_naming_template_in_source_is_rejected() {
        let toml = "[accounts.alice.sources.liked]\nnaming_template = \"\"\n";
        assert!(Config::from_toml(toml).is_err());
    }

    #[test]
    fn non_empty_naming_template_is_accepted() {
        let toml = "[defaults]\nnaming_template = \"{creator}/{title}\"\n";
        assert!(Config::from_toml(toml).is_ok());
    }

    #[test]
    fn min_newest_zero_stays_legal() {
        // A deletion floor of 0 is a valid explicit opt-out for additive/copy
        // runs; the empty-template guard must never reject a numeric zero.
        let cfg = Config::from_toml("[defaults]\nmin_newest = 0\n").unwrap();
        assert_eq!(cfg.defaults.settings.min_newest, Some(0));
    }
}
