//! `config init`, `config add-account`, and `config show`.
//!
//! The core `Config` type is deserialize-only, so writing is done by emitting
//! TOML text directly. `show` redacts every token-bearing setting.

use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use anyhow::{Context, Result};
use suno_core::Config;

use crate::cli::args::{ConfigAddAccountArgs, ConfigArgs, ConfigCommand, GlobalArgs};
use crate::cli::desired::ExitCode;
use crate::cli::logs;
#[cfg(unix)]
use crate::download::set_permissions_or_remove;
use crate::download::write_atomic_private;

const PRIVATE_CONFIG_FILE_MODE: u32 = 0o600;
const PRIVATE_CONFIG_DIR_MODE: u32 = 0o700;

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

    let body = account_block(&label, &token, opt(&root));
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
    let config = match crate::cli::run::load_config_reported(global.config.as_deref()) {
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
    if d.format.is_some()
        || d.concurrency.is_some()
        || d.retries.is_some()
        || d.min_newest.is_some()
        || d.token_command.is_some()
        || d.animated_covers.is_some()
        || d.video_cover_retention.is_some()
        || d.animated_cover_quality.is_some()
        || d.animated_cover_max_fps.is_some()
        || d.animated_cover_max_width.is_some()
        || d.animated_cover_compression_level.is_some()
        || d.animated_cover_lossless.is_some()
        || d.naming_template.is_some()
        || d.character_set.is_some()
    {
        out.push_str("[defaults]\n");
        push_opt(&mut out, "format", d.format.map(|f| f.to_string()));
        push_reserved(
            &mut out,
            "concurrency",
            d.concurrency.map(|v| v.to_string()),
        );
        push_opt(&mut out, "retries", d.retries.map(|v| v.to_string()));
        push_opt(&mut out, "min_newest", d.min_newest.map(|v| v.to_string()));
        push_redacted(&mut out, "token_command", d.token_command.as_deref());
        push_opt(
            &mut out,
            "animated_covers",
            d.animated_covers.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "video_cover_retention",
            d.video_cover_retention.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_quality",
            d.animated_cover_quality.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_max_fps",
            d.animated_cover_max_fps.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_max_width",
            d.animated_cover_max_width.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_compression_level",
            d.animated_cover_compression_level.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_lossless",
            d.animated_cover_lossless.map(|v| v.to_string()),
        );
        push_opt(&mut out, "naming_template", d.naming_template.clone());
        push_opt(
            &mut out,
            "character_set",
            d.character_set.map(|v| v.to_string()),
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
        push_redacted(&mut out, "token_command", acc.token_command.as_deref());
        push_opt(&mut out, "root", acc.root.clone());
        push_opt(&mut out, "format", acc.format.map(|f| f.to_string()));
        push_reserved(
            &mut out,
            "concurrency",
            acc.concurrency.map(|v| v.to_string()),
        );
        push_opt(&mut out, "retries", acc.retries.map(|v| v.to_string()));
        push_opt(
            &mut out,
            "min_newest",
            acc.min_newest.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_covers",
            acc.animated_covers.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "video_cover_retention",
            acc.video_cover_retention.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_quality",
            acc.animated_cover_quality.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_max_fps",
            acc.animated_cover_max_fps.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_max_width",
            acc.animated_cover_max_width.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_compression_level",
            acc.animated_cover_compression_level.map(|v| v.to_string()),
        );
        push_opt(
            &mut out,
            "animated_cover_lossless",
            acc.animated_cover_lossless.map(|v| v.to_string()),
        );
        push_opt(&mut out, "naming_template", acc.naming_template.clone());
        push_opt(
            &mut out,
            "character_set",
            acc.character_set.map(|v| v.to_string()),
        );
        let mut sources: Vec<&String> = acc.sources.keys().collect();
        sources.sort();
        for name in sources {
            let src = &acc.sources[name];
            out.push_str(&format!("  [accounts.{label}.sources.{name}]\n"));
            push_redacted(&mut out, "    token_command", src.token_command.as_deref());
            push_opt(&mut out, "    format", src.format.map(|f| f.to_string()));
            push_opt(&mut out, "    naming_template", src.naming_template.clone());
            push_opt(
                &mut out,
                "    character_set",
                src.character_set.map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    video_cover_retention",
                src.video_cover_retention.map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    animated_cover_quality",
                src.animated_cover_quality.map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    animated_cover_max_fps",
                src.animated_cover_max_fps.map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    animated_cover_max_width",
                src.animated_cover_max_width.map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    animated_cover_compression_level",
                src.animated_cover_compression_level.map(|v| v.to_string()),
            );
            push_opt(
                &mut out,
                "    animated_cover_lossless",
                src.animated_cover_lossless.map(|v| v.to_string()),
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

/// Build a `[accounts.<label>]` TOML block.
fn account_block(label: &str, token: &str, root: Option<&str>) -> String {
    let mut block = format!("[accounts.{label}]\ntoken = \"{}\"\n", toml_escape(token));
    if let Some(root) = root {
        block.push_str(&format!("root = \"{}\"\n", toml_escape(root)));
    }
    block
}

/// Write config text atomically, creating the parent directory.
fn write_config(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        set_private_permissions(parent, PRIVATE_CONFIG_DIR_MODE)?;
    }
    write_atomic_private(path, body.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))?;
    secure_private_file(path, PRIVATE_CONFIG_FILE_MODE)
}

#[cfg(unix)]
fn set_private_permissions(path: &Path, mode: u32) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("could not set permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn secure_private_file(path: &Path, mode: u32) -> Result<()> {
    set_permissions_or_remove(path, mode)
        .with_context(|| format!("could not set permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn secure_private_file(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
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

    #[cfg(unix)]
    #[test]
    fn write_config_sets_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            Path::new("target").join(format!("config-private-{}-{stamp}", std::process::id()));
        let path = dir.join("suno/config.toml");
        write_config(&path, "[accounts.default]\ntoken = \"x\"\n").unwrap();

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);

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
