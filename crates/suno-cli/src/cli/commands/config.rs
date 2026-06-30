//! `config init`, `config add-account`, and `config show`.
//!
//! The core `Config` type is deserialize-only, so writing is done by emitting
//! TOML text directly. `show` redacts every token.

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
        || d.playlists_as_albums.is_some()
    {
        out.push_str("[defaults]\n");
        push_opt(&mut out, "format", d.format.map(|f| f.to_string()));
        push_opt(
            &mut out,
            "concurrency",
            d.concurrency.map(|v| v.to_string()),
        );
        push_opt(&mut out, "retries", d.retries.map(|v| v.to_string()));
        push_opt(&mut out, "min_newest", d.min_newest.map(|v| v.to_string()));
        push_opt(
            &mut out,
            "playlists_as_albums",
            d.playlists_as_albums.map(|v| v.to_string()),
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
        push_opt(&mut out, "root", acc.root.clone());
        push_opt(&mut out, "format", acc.format.map(|f| f.to_string()));
        push_opt(
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
            "playlists_as_albums",
            acc.playlists_as_albums.map(|v| v.to_string()),
        );
        let mut sources: Vec<&String> = acc.sources.keys().collect();
        sources.sort();
        for name in sources {
            let src = &acc.sources[name];
            out.push_str(&format!("  [accounts.{label}.sources.{name}]\n"));
            push_opt(&mut out, "    format", src.format.map(|f| f.to_string()));
        }
        out.push('\n');
    }
    out
}

fn push_opt(out: &mut String, key: &str, value: Option<String>) {
    if let Some(value) = value {
        out.push_str(&format!("  {key} = {value}\n"));
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
    fn show_redacts_token() {
        let toml = "[accounts.alice]\ntoken = \"supersecret\"\nroot = \"/music\"\n";
        let config = Config::from_toml(toml).unwrap();
        let shown = render_show(&config);
        assert!(shown.contains("token = [redacted]"));
        assert!(!shown.contains("supersecret"));
        assert!(shown.contains("root = /music"));
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
    fn opt_blanks_become_none() {
        assert_eq!(opt("   "), None);
        assert_eq!(opt(" /music "), Some("/music"));
    }
}
