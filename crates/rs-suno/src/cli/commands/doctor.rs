//! `doctor`: diagnose environment, config, auth, and credits.

use std::collections::HashMap;

use anyhow::{Context, Result};
use suno_core::{ClerkAuth, Config, FlagOverrides, TOKEN_EXPIRY_WARN_DAYS};

use crate::cli::args::GlobalArgs;
use crate::cli::desired::ExitCode;
use crate::cli::expiry::token_expiry_message;
use crate::cli::run;
use crate::http::ReqwestHttp;

/// Run the `doctor` diagnostic command.
pub async fn run_doctor(global: &GlobalArgs) -> Result<ExitCode> {
    println!("=== suno doctor ===\n");

    print_env();
    let config = print_config(global);
    let code = check_auth_and_credits(global, config.as_ref()).await?;

    Ok(code)
}

/// Report relevant environment variables (values redacted for tokens).
fn print_env() {
    println!("[env]");
    let env_vars = [
        "SUNO_TOKEN",
        "SUNO_CONFIG",
        "SUNO_ACCOUNT",
        "SUNO_DRY_RUN",
        "SUNO_YES",
    ];
    for name in env_vars {
        match std::env::var(name) {
            Ok(value) => {
                if name.contains("TOKEN") {
                    println!("  {name} = [set, redacted]");
                } else {
                    println!("  {name} = {value}");
                }
            }
            Err(_) => println!("  {name} = [not set]"),
        }
    }
    for (key, _) in std::env::vars() {
        if key.starts_with("SUNO_") && key.ends_with("_TOKEN") && key != "SUNO_TOKEN" {
            println!("  {key} = [set, redacted]");
        }
    }
    println!();
}

/// Report config status and return the parsed config if available.
fn print_config(global: &GlobalArgs) -> Option<Config> {
    println!("[config]");
    let path = crate::cli::logs::config_path(global.config.as_deref());
    match &path {
        Some(p) if p.exists() => println!("  path: {}", p.display()),
        Some(p) => {
            println!("  path: {} (not found)", p.display());
            println!();
            return None;
        }
        None => {
            println!("  path: (none)");
            println!();
            return None;
        }
    }

    match run::load_config_reported(global.config.as_deref()) {
        Ok(Some(config)) => {
            let count = config.accounts.len();
            let mut labels: Vec<&String> = config.accounts.keys().collect();
            labels.sort();
            println!(
                "  accounts: {count} ({})",
                labels
                    .iter()
                    .map(|l| l.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("  parses: ok");
            println!();
            Some(config)
        }
        Ok(None) => {
            println!("  parses: no config file");
            println!();
            None
        }
        Err(_code) => {
            println!("  parses: ERROR (see above)");
            println!();
            None
        }
    }
}

/// A resolved account label and its token, ready for the auth/credits check.
#[derive(Debug)]
struct Target {
    label: String,
    token: Option<String>,
}

/// Check auth and credits for each resolved account.
async fn check_auth_and_credits(global: &GlobalArgs, config: Option<&Config>) -> Result<ExitCode> {
    println!("[auth]");
    let env: HashMap<String, String> = std::env::vars().collect();
    let flags = FlagOverrides {
        token: global.token.clone(),
        ..FlagOverrides::default()
    };

    let targets = match resolve_doctor_targets(config, global, &env, &flags) {
        Ok(targets) => targets,
        Err(message) => {
            println!("  {message}");
            println!();
            return Ok(ExitCode::Config);
        }
    };

    if targets.is_empty() {
        println!("  no accounts to check (no token available)");
        println!();
        return Ok(ExitCode::Config);
    }

    let http = ReqwestHttp::new().context("failed to build the HTTP client")?;
    let mut worst = ExitCode::Ok;

    for target in &targets {
        let label = &target.label;
        let Some(token) = &target.token else {
            println!("  [{label}] token: not set");
            worst = worse(worst, ExitCode::Config);
            continue;
        };

        let mut auth = ClerkAuth::new(token);

        // Token expiry check (doesn't require network).
        let now = run::now_secs() as i64;
        let window = TOKEN_EXPIRY_WARN_DAYS * 86_400;
        let expiry = auth.token_expiry(now, window);
        if let Some(msg) = token_expiry_message(label, expiry) {
            println!("  [{label}] {msg}");
        }

        // Auth check (network).
        match auth.authenticate(&http).await {
            Ok(user_id) => {
                let short_id = &user_id[..user_id.len().min(12)];
                println!(
                    "  [{label}] auth: ok (user: {}, id: {short_id}...)",
                    auth.display_name()
                );
            }
            Err(err) => {
                println!("  [{label}] auth: FAILED ({err})");
                worst = worse(worst, ExitCode::Auth);
                continue;
            }
        }

        // Credits check (network).
        let clock = crate::clock::TokioClock;
        let mut client = suno_core::SunoClient::new(auth, clock);
        match client.billing_info(&http).await {
            Ok(info) => {
                println!(
                    "  [{label}] credits: {} remaining (plan: {}, used: {}/{})",
                    info.total_credits_left, info.plan, info.monthly_usage, info.monthly_limit,
                );
            }
            Err(err) => {
                println!("  [{label}] credits: could not fetch ({err})");
            }
        }
    }

    println!();
    Ok(worst)
}

/// Resolve which accounts to diagnose.
fn resolve_doctor_targets(
    config: Option<&Config>,
    global: &GlobalArgs,
    env: &HashMap<String, String>,
    flags: &FlagOverrides,
) -> std::result::Result<Vec<Target>, String> {
    if let Some(account) = &global.account {
        let cfg = config.ok_or_else(|| format!("account '{account}' not found: no config file"))?;
        if !cfg.accounts.contains_key(account) {
            return Err(format!("account '{account}' not found in config"));
        }
        let settings = cfg
            .resolve(account, None, env, flags)
            .map_err(|err| err.to_string())?;
        return Ok(vec![Target {
            label: account.clone(),
            token: settings.token,
        }]);
    }
    if global.all {
        let cfg = config.ok_or_else(|| "--all requires a config file".to_owned())?;
        let mut labels: Vec<String> = cfg.accounts.keys().cloned().collect();
        labels.sort();
        if labels.is_empty() {
            return Err("no accounts are configured".to_owned());
        }
        return labels
            .into_iter()
            .map(|label| {
                cfg.resolve(&label, None, env, flags)
                    .map(|settings| Target {
                        label,
                        token: settings.token,
                    })
                    .map_err(|err| err.to_string())
            })
            .collect();
    }
    // Default: try all accounts in config, or fall back to env/flags token.
    if let Some(cfg) = config
        && !cfg.accounts.is_empty()
    {
        let mut labels: Vec<String> = cfg.accounts.keys().cloned().collect();
        labels.sort();
        return labels
            .into_iter()
            .map(|label| {
                cfg.resolve(&label, None, env, flags)
                    .map(|settings| Target {
                        label,
                        token: settings.token,
                    })
                    .map_err(|err| err.to_string())
            })
            .collect();
    }
    // No config accounts: try env/flags token alone.
    let token = flags
        .token
        .clone()
        .or_else(|| env.get("SUNO_TOKEN").cloned());
    if token.is_some() {
        return Ok(vec![Target {
            label: "(env)".to_owned(),
            token,
        }]);
    }
    Ok(vec![])
}

fn worse(a: ExitCode, b: ExitCode) -> ExitCode {
    if b.code() >= a.code() { b } else { a }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HashMap<String, String> {
        HashMap::new()
    }

    fn global() -> GlobalArgs {
        GlobalArgs::default()
    }

    #[test]
    fn no_config_no_env_yields_empty_targets() {
        let targets =
            resolve_doctor_targets(None, &global(), &env(), &FlagOverrides::default()).unwrap();
        assert!(targets.is_empty());
    }

    #[test]
    fn flags_token_yields_env_target() {
        let flags = FlagOverrides {
            token: Some("tok".to_owned()),
            ..FlagOverrides::default()
        };
        let targets = resolve_doctor_targets(None, &global(), &env(), &flags).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "(env)");
        assert_eq!(targets[0].token.as_deref(), Some("tok"));
    }

    #[test]
    fn env_token_yields_env_target() {
        let mut e = env();
        e.insert("SUNO_TOKEN".to_owned(), "envtok".to_owned());
        let targets =
            resolve_doctor_targets(None, &global(), &e, &FlagOverrides::default()).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "(env)");
        assert_eq!(targets[0].token.as_deref(), Some("envtok"));
    }

    #[test]
    fn config_accounts_all_resolved() {
        let config =
            Config::from_toml("[accounts.alice]\ntoken=\"a\"\n[accounts.bob]\ntoken=\"b\"\n")
                .unwrap();
        let targets =
            resolve_doctor_targets(Some(&config), &global(), &env(), &FlagOverrides::default())
                .unwrap();
        let labels: Vec<&str> = targets.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["alice", "bob"]);
    }

    #[test]
    fn named_account_resolves() {
        let config = Config::from_toml("[accounts.alice]\ntoken=\"a\"\n").unwrap();
        let g = GlobalArgs {
            account: Some("alice".to_owned()),
            ..Default::default()
        };
        let targets =
            resolve_doctor_targets(Some(&config), &g, &env(), &FlagOverrides::default()).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "alice");
        assert_eq!(targets[0].token.as_deref(), Some("a"));
    }

    #[test]
    fn unknown_named_account_errors() {
        let config = Config::from_toml("[accounts.alice]\ntoken=\"a\"\n").unwrap();
        let g = GlobalArgs {
            account: Some("bob".to_owned()),
            ..Default::default()
        };
        let err = resolve_doctor_targets(Some(&config), &g, &env(), &FlagOverrides::default())
            .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn all_without_config_errors() {
        let g = GlobalArgs {
            all: true,
            ..Default::default()
        };
        let err = resolve_doctor_targets(None, &g, &env(), &FlagOverrides::default()).unwrap_err();
        assert!(err.contains("--all requires"));
    }
}
