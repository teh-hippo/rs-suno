//! `config init`, `config add-account`, and `config show`.
//!
//! The core `Config` type is deserialize-only, so writing is done by emitting
//! TOML text directly. `show` redacts every token-bearing setting.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use suno_core::Config;

use crate::cli::args::{ConfigAddAccountArgs, ConfigArgs, ConfigCommand, GlobalArgs};
use crate::cli::desired::ExitCode;
use crate::cli::logs;
use crate::download::write_atomic;

/// Run a `config` subcommand.
pub fn run_config(global: &GlobalArgs, args: &ConfigArgs) -> Result<ExitCode> {
    match &args.command {
        ConfigCommand::Init => init(global),
        ConfigCommand::AddAccount(add) => add_account(global, add),
        ConfigCommand::Show => show(global),
    }
}

fn init(global: &GlobalArgs) -> Result<ExitCode> {
    let Some(path) = logs::config_path(global.config.as_deref()) else {
        eprintln!("error: could not determine a config path; pass --config <PATH>");
        return Ok(ExitCode::Config);
    };
    if path.exists() && !global.yes {
        eprintln!(
            "error: config already exists at {}; pass --yes to overwrite",
            path.display()
        );
        return Ok(ExitCode::Config);
    }

    let label = prompt_with_default("Account label", "default")?;
    let token = prompt("Suno __client token")?;
    if token.is_empty() {
        eprintln!("error: a token is required");
        return Ok(ExitCode::Config);
    }
    let root = prompt("Library root (optional, blank to set later)")?;

    let body = with_schema_directive(&account_block(&label, &token, opt(&root)));
    write_config(&path, &body)?;
    eprintln!("Wrote config to {}", path.display());
    Ok(ExitCode::Ok)
}

fn add_account(global: &GlobalArgs, add: &ConfigAddAccountArgs) -> Result<ExitCode> {
    let Some(path) = logs::config_path(global.config.as_deref()) else {
        eprintln!("error: could not determine a config path; pass --config <PATH>");
        return Ok(ExitCode::Config);
    };
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "error: no config at {}; run 'suno config init' first",
                path.display()
            );
            return Ok(ExitCode::Config);
        }
        Err(err) => return Err(err).context(format!("could not read {}", path.display())),
    };

    let config = match Config::from_toml(&existing) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("error: {}: {err}", path.display());
            return Ok(ExitCode::Config);
        }
    };

    let label = match &add.label {
        Some(label) => label.clone(),
        None => prompt("Account label")?,
    };
    if label.is_empty() {
        eprintln!("error: an account label is required");
        return Ok(ExitCode::Config);
    }
    if config.accounts.contains_key(&label) {
        eprintln!(
            "error: account '{label}' already exists in {}",
            path.display()
        );
        return Ok(ExitCode::Config);
    }

    let token = match &add.token {
        Some(token) => token.clone(),
        None => prompt("Suno __client token")?,
    };
    if token.is_empty() {
        eprintln!("error: a token is required");
        return Ok(ExitCode::Config);
    }
    let root = prompt("Library root (optional, blank to set later)")?;

    let mut body = existing;
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push('\n');
    body.push_str(&account_block(&label, &token, opt(&root)));
    write_config(&path, &body)?;
    eprintln!("Added account '{label}' to {}", path.display());
    Ok(ExitCode::Ok)
}

fn show(global: &GlobalArgs) -> Result<ExitCode> {
    let path = logs::config_path(global.config.as_deref());
    let config = match crate::cli::config_load::load_config_reported(global.config.as_deref()) {
        Ok(Some(config)) => config,
        Ok(None) => {
            let where_ = path
                .as_deref()
                .map(|p| format!(" at {}", p.display()))
                .unwrap_or_default();
            eprintln!("error: no config file found{where_}");
            return Ok(ExitCode::Config);
        }
        Err(code) => return Ok(code),
    };

    println!("{CONFIG_SCHEMA_DIRECTIVE}");
    if let Some(path) = &path {
        println!("# {}", path.display());
    }
    print!("{}", render_show(&config));
    Ok(ExitCode::Ok)
}

/// Render a redacted, human-readable view of the config (tokens hidden).
fn render_show(config: &Config) -> String {
    let mut out = String::new();
    let d = &config.defaults;
    if d.settings.format.is_some()
        || d.settings.concurrency.is_some()
        || d.settings.retries.is_some()
        || d.settings.min_newest.is_some()
        || d.settings.token_command.is_some()
        || d.settings.animated_covers.is_some()
        || d.settings.video_cover_retention.is_some()
        || d.settings.animated_cover_quality.is_some()
        || d.settings.animated_cover_max_fps.is_some()
        || d.settings.animated_cover_max_width.is_some()
        || d.settings.animated_cover_compression_level.is_some()
        || d.settings.animated_cover_lossless.is_some()
        || d.settings.naming_template.is_some()
        || d.settings.character_set.is_some()
    {
        out.push_str("[defaults]\n");
        push_opt(&mut out, "format", d.settings.format.map(|f| f.to_string()));
        push_reserved(
            &mut out,
            "concurrency",
            d.settings.concurrency.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "retries",
            d.settings.retries.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "min_newest",
            d.settings.min_newest.map(|v| v.to_string()),
        );
        push_redacted(
            &mut out,
            "token_command",
            d.settings.token_command.as_deref(),
        );
        push_opt(
            &mut out,
            "animated_covers",
            d.settings.animated_covers.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "video_cover_retention",
            d.settings.video_cover_retention.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_quality",
            d.settings.animated_cover_quality.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_max_fps",
            d.settings.animated_cover_max_fps.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_max_width",
            d.settings.animated_cover_max_width.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_compression_level",
            d.settings
                .animated_cover_compression_level
                .map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_lossless",
            d.settings.animated_cover_lossless.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "naming_template",
            d.settings.naming_template.clone(),
        );
        push_opt(
            &mut out,
            "character_set",
            d.settings.character_set.map(|v| v.to_string()),
        );
        out.push('\n');
    }

    let mut labels: Vec<&String> = config.accounts.keys().collect();
    labels.sort();
    for label in labels {
        let acc = &config.accounts[label];
        out.push_str(&format!("[accounts.{label}]\n"));
        out.push_str(match acc.token {
            Some(_) => "  token = [redacted]\n",
            None => "  token = [not set]\n",
        });
        push_redacted(
            &mut out,
            "token_command",
            acc.settings.token_command.as_deref(),
        );
        push_opt(&mut out, "root", acc.root.clone());
        push_opt(
            &mut out,
            "format",
            acc.settings.format.map(|f| f.to_string()),
        );
        push_reserved(
            &mut out,
            "concurrency",
            acc.settings.concurrency.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "retries",
            acc.settings.retries.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "min_newest",
            acc.settings.min_newest.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_covers",
            acc.settings.animated_covers.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "video_cover_retention",
            acc.settings.video_cover_retention.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_quality",
            acc.settings.animated_cover_quality.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_max_fps",
            acc.settings.animated_cover_max_fps.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_max_width",
            acc.settings.animated_cover_max_width.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_compression_level",
            acc.settings
                .animated_cover_compression_level
                .map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_lossless",
            acc.settings.animated_cover_lossless.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "naming_template",
            acc.settings.naming_template.clone(),
        );
        push_opt(
            &mut out,
            "character_set",
            acc.settings.character_set.map(|v| v.to_string()),
        );
        let mut sources: Vec<&String> = acc.sources.keys().collect();
        sources.sort();
        for name in sources {
            let src = &acc.sources[name];
            out.push_str(&format!("  [accounts.{label}.sources.{name}]\n"));
            push_redacted(
                &mut out,
                "    token_command",
                src.settings.token_command.as_deref(),
            );
            push_opt(
                &mut out,
                "    format",
                src.settings.format.map(|f| f.to_string()),
            );
            push_opt(
                &mut out,
                "    naming_template",
                src.settings.naming_template.clone(),
            );
            push_opt(
                &mut out,
                "    character_set",
                src.settings.character_set.map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    video_cover_retention",
                src.settings.video_cover_retention.map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    animated_cover_quality",
                src.settings.animated_cover_quality.map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    animated_cover_max_fps",
                src.settings.animated_cover_max_fps.map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    animated_cover_max_width",
                src.settings.animated_cover_max_width.map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    animated_cover_compression_level",
                src.settings
                    .animated_cover_compression_level
                    .map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    animated_cover_lossless",
                src.settings.animated_cover_lossless.map(|v| v.to_string()),
            );
        }
        if let Some(areas) = &acc.areas {
            render_areas(&mut out, label, areas);
        }
        out.push('\n');
    }
    out
}

/// Render an account's `[areas]` selection table.
///
/// Playlist ids are shown with a trailing `# (unknown)` comment: `config show`
/// runs offline, so it cannot resolve an id to its live playlist name.
fn render_areas(out: &mut String, label: &str, areas: &suno_core::AreasConfig) {
    out.push_str(&format!("  [accounts.{label}.areas]\n"));
    if let Some(library) = areas.library {
        out.push_str(&format!("    library = {}\n", area_mode_str(library)));
    }
    if let Some(liked) = areas.liked {
        out.push_str(&format!("    liked = {}\n", source_mode_str(liked)));
    }
    if let Some(playlists) = areas.playlists {
        out.push_str(&format!("    playlists = {}\n", source_mode_str(playlists)));
    }
    if !areas.playlist.is_empty() {
        out.push_str(&format!("  [accounts.{label}.areas.playlist]\n"));
        let mut ids: Vec<&String> = areas.playlist.keys().collect();
        ids.sort();
        for id in ids {
            let mode = source_mode_str(areas.playlist[id]);
            out.push_str(&format!("    \"{id}\" = {mode}  # (unknown)\n"));
        }
    }
}

/// The TOML keyword for a [`SourceMode`].
fn source_mode_str(mode: suno_core::SourceMode) -> &'static str {
    match mode {
        suno_core::SourceMode::Mirror => "mirror",
        suno_core::SourceMode::Copy => "copy",
    }
}

/// The TOML keyword for an [`AreaMode`], including the library-only `off`.
fn area_mode_str(mode: suno_core::AreaMode) -> &'static str {
    match mode {
        suno_core::AreaMode::Off => "off",
        suno_core::AreaMode::Mode(mode) => source_mode_str(mode),
    }
}

fn push_opt(out: &mut String, key: &str, value: Option<String>) {
    if let Some(value) = value {
        out.push_str(&format!("  {key} = {value}\n"));
    }
}

/// Like [`push_opt`], but marks the value as a reserved knob that has no effect
/// yet, so `config show` does not advertise an inert setting as active.
fn push_reserved(out: &mut String, key: &str, value: Option<String>) {
    if let Some(value) = value {
        out.push_str(&format!(
            "  {key} = {value}  # reserved; downloads are sequential\n"
        ));
    }
}

fn push_redacted(out: &mut String, key: &str, value: Option<&str>) {
    if value.is_some() {
        out.push_str(&format!("  {key} = [redacted]\n"));
    }
}

/// The published JSON Schema, referenced by a Taplo `#:schema` header directive
/// so editors (the Even Better TOML extension) validate and autocomplete the
/// config. See `docs/src/config.schema.json`, published to GitHub Pages.
const CONFIG_SCHEMA_DIRECTIVE: &str =
    "#:schema https://teh-hippo.github.io/rs-suno/config.schema.json";

/// Prefix freshly generated config text with the schema header directive. The
/// directive is a TOML comment, so the file still parses unchanged.
fn with_schema_directive(body: &str) -> String {
    format!("{CONFIG_SCHEMA_DIRECTIVE}\n\n{body}")
}

/// Build a `[accounts.<label>]` TOML block.
fn account_block(label: &str, token: &str, root: Option<&str>) -> String {
    let mut block = format!("[accounts.{label}]\ntoken = \"{}\"\n", toml_escape(token));
    if let Some(root) = root {
        block.push_str(&format!("root = \"{}\"\n", toml_escape(root)));
    }
    block
}

/// Write config text atomically, creating the parent directory.
///
/// The config can hold a token, but it is written in plaintext with the
/// platform's default permissions: std has no portable owner-only primitive and
/// there is no lightweight cross-platform crate for one. Keep secrets out of the
/// file by using `token_command` with a secret manager, or restrict the file
/// yourself (for example `chmod 600`).
fn write_config(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    write_atomic(path, body.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))
}

/// Escape a string for a double-quoted TOML value.
fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Trim a prompt answer, returning `None` when blank.
fn opt(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Prompt on stderr and read a trimmed line from stdin.
fn prompt(label: &str) -> Result<String> {
    eprint!("{label}: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("could not read input")?;
    Ok(line.trim().to_owned())
}

/// Prompt with a default applied when the answer is blank.
fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    let answer = prompt(&format!("{label} [{default}]"))?;
    Ok(if answer.is_empty() {
        default.to_owned()
    } else {
        answer
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_block_quotes_and_escapes() {
        let block = account_block("alice", "to\"ken\\x", Some("/music"));
        assert!(block.contains("[accounts.alice]"));
        assert!(block.contains("token = \"to\\\"ken\\\\x\""));
        assert!(block.contains("root = \"/music\""));
    }

    #[test]
    fn account_block_omits_blank_root() {
        let block = account_block("alice", "tok", None);
        assert!(!block.contains("root ="));
    }

    #[test]
    fn block_roundtrips_through_core_parser() {
        let block = account_block("my-lib", "secret", Some("/m"));
        let config = Config::from_toml(&block).unwrap();
        let acc = &config.accounts["my-lib"];
        assert_eq!(acc.token.as_deref(), Some("secret"));
        assert_eq!(acc.root.as_deref(), Some("/m"));
    }

    #[test]
    fn init_body_carries_schema_directive_and_still_parses() {
        let body = with_schema_directive(&account_block("alice", "t", Some("/m")));
        assert!(body.starts_with("#:schema https://"));
        assert!(body.contains("config.schema.json"));
        // The directive is a TOML comment, so the generated file still parses.
        let config = Config::from_toml(&body).unwrap();
        assert_eq!(config.accounts["alice"].token.as_deref(), Some("t"));
    }

    #[test]
    fn show_redacts_token() {
        let toml = "[accounts.alice]\ntoken = \"supersecret\"\nroot = \"/music\"\n";
        let config = Config::from_toml(toml).unwrap();
        let shown = render_show(&config);
        assert!(shown.contains("token = [redacted]"));
        assert!(!shown.contains("supersecret"));
        assert!(shown.contains("root = /music"));
    }

    #[test]
    fn show_redacts_token_command() {
        let toml = r#"
            [defaults]
            token_command = "printf 'default-secret\n'"

            [accounts.alice]
            token_command = "printf 'account-secret\n'"

            [accounts.alice.sources.liked]
            token_command = "printf 'source-secret\n'"
        "#;
        let config = Config::from_toml(toml).unwrap();
        let shown = render_show(&config);
        assert!(shown.contains("token_command = [redacted]"));
        assert!(!shown.contains("default-secret"));
        assert!(!shown.contains("account-secret"));
        assert!(!shown.contains("source-secret"));
    }

    #[test]
    fn show_marks_missing_token() {
        let toml = "[accounts.alice]\nroot = \"/music\"\n";
        let config = Config::from_toml(toml).unwrap();
        let shown = render_show(&config);
        assert!(shown.contains("token = [not set]"));
    }

    #[test]
    fn show_renders_defaults_and_sources() {
        let toml = "
            [defaults]
            format = \"mp3\"

            [accounts.alice]
            token = \"t\"

            [accounts.alice.sources.liked]
            format = \"wav\"
        ";
        let config = Config::from_toml(toml).unwrap();
        let shown = render_show(&config);
        assert!(shown.contains("[defaults]"));
        assert!(shown.contains("format = mp3"));
        assert!(shown.contains("[accounts.alice.sources.liked]"));
        assert!(shown.contains("format = wav"));
    }

    #[test]
    fn show_marks_concurrency_reserved() {
        let toml = "[defaults]\nconcurrency = 8\n";
        let config = Config::from_toml(toml).unwrap();
        let shown = render_show(&config);
        assert!(shown.contains("concurrency = 8"));
        assert!(shown.contains("reserved; downloads are sequential"));
    }

    #[test]
    fn show_renders_animated_covers() {
        let toml =
            "[defaults]\nanimated_covers = true\n\n[accounts.alice]\nanimated_covers = false\n";
        let config = Config::from_toml(toml).unwrap();
        let shown = render_show(&config);
        assert!(shown.contains("animated_covers = true"));
        assert!(shown.contains("animated_covers = false"));
    }

    #[test]
    fn opt_blanks_become_none() {
        assert_eq!(opt("   "), None);
        assert_eq!(opt(" /music "), Some("/music"));
    }

    #[test]
    fn show_renders_areas_block_with_playlist_id_and_name_comment() {
        let toml = "
            [accounts.alice]
            token = \"t\"

            [accounts.alice.areas]
            library = \"off\"
            liked = \"copy\"
            playlists = \"mirror\"

            [accounts.alice.areas.playlist]
            abc123 = \"copy\"
        ";
        let config = Config::from_toml(toml).unwrap();
        let shown = render_show(&config);
        assert!(shown.contains("[accounts.alice.areas]"));
        assert!(shown.contains("library = off"));
        assert!(shown.contains("liked = copy"));
        assert!(shown.contains("playlists = mirror"));
        assert!(shown.contains("[accounts.alice.areas.playlist]"));
        // The playlist id is rendered with a resolved-name comment placeholder
        // (the offline command cannot resolve the live name).
        assert!(shown.contains("\"abc123\" = copy  # (unknown)"));
    }

    #[test]
    fn write_config_creates_parent_and_writes_body() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = Path::new("target").join(format!("config-write-{}-{stamp}", std::process::id()));
        let path = dir.join("suno/config.toml");
        let body = "[accounts.default]\ntoken = \"x\"\n";
        write_config(&path, body).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn show_renders_naming_template_and_character_set() {
        let toml = "[defaults]\nnaming_template = \"{title}/{id8}\"\ncharacter_set = \"ascii\"\n\n[accounts.alice]\nnaming_template = \"{creator}/{title}\"\ncharacter_set = \"unicode\"\n";
        let config = Config::from_toml(toml).unwrap();
        let shown = render_show(&config);
        assert!(shown.contains("naming_template = {title}/{id8}"));
        assert!(shown.contains("character_set = ascii"));
        assert!(shown.contains("naming_template = {creator}/{title}"));
        assert!(shown.contains("character_set = unicode"));
    }
}
