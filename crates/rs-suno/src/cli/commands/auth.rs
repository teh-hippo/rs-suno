//! `auth refresh`: re-mint an account's JWT to confirm its stored token still
//! works, for one account, a named account, or every account with `--all`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use suno_core::{ClerkAuth, Config, EffectiveSettings, FlagOverrides};

use crate::cli::args::{AuthArgs, AuthCommand, AuthRefreshArgs, GlobalArgs};
use crate::cli::desired::{ExitCode, worse};
use crate::cli::run;
use crate::http::ReqwestHttp;

/// Run an `auth` subcommand.
pub async fn run_auth(global: &GlobalArgs, args: &AuthArgs) -> Result<ExitCode> {
    match &args.command {
        AuthCommand::Refresh(refresh) => refresh_accounts(global, refresh).await,
    }
}

async fn refresh_accounts(global: &GlobalArgs, refresh: &AuthRefreshArgs) -> Result<ExitCode> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let flags = FlagOverrides {
        token: global.token.clone(),
        ..FlagOverrides::default()
    };

    let config = match run::load_config_reported(global.config.as_deref()) {
        Ok(config) => config,
        Err(code) => return Ok(code),
    };

    let resolved = match resolve_targets(config.as_ref(), global, refresh, &env, &flags) {
        Ok(resolved) => resolved,
        Err(message) => {
            eprintln!("error: {message}");
            return Ok(ExitCode::Config);
        }
    };

    let http = ReqwestHttp::new().context("failed to build the HTTP client")?;
    let mut worst = ExitCode::Ok;
    for (label, settings) in resolved {
        let token = match run::resolve_token(&label, &settings).await {
            Ok(Some(token)) => token,
            Ok(None) => {
                eprintln!(
                    "error: no token for account '{label}'; pass --token, set SUNO_TOKEN or SUNO_TOKEN_COMMAND, or set token/token_command in config"
                );
                worst = worse(worst, ExitCode::Config);
                continue;
            }
            Err(err) => {
                eprintln!("error: {err}");
                worst = worse(worst, ExitCode::Config);
                continue;
            }
        };
        let auth = ClerkAuth::new(&token);
        match auth.authenticate(&http).await {
            Ok(_) => {
                crate::cli::expiry::warn_token_expiry(&label, &auth, global.verbosity());
                if global.verbosity() >= -1 {
                    eprintln!("Re-authenticated '{label}' as {}", auth.display_name());
                }
            }
            Err(err) => worst = worse(worst, run::report_auth_failure(&label, &err)),
        }
    }
    Ok(worst)
}

/// Decide which accounts to refresh and resolve each one's settings.
fn resolve_targets(
    config: Option<&Config>,
    global: &GlobalArgs,
    refresh: &AuthRefreshArgs,
    env: &HashMap<String, String>,
    flags: &FlagOverrides,
) -> std::result::Result<Vec<(String, EffectiveSettings)>, String> {
    if let Some(account) = &refresh.account {
        let settings = resolve_named(config, account, env, flags)?;
        return Ok(vec![(account.clone(), settings)]);
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
                    .map(|settings| (label, settings))
                    .map_err(|err| err.to_string())
            })
            .collect();
    }
    let resolved = run::single_account(config, global, flags, env)?;
    Ok(vec![resolved])
}

/// Resolve a named account, erroring when no config holds it.
fn resolve_named(
    config: Option<&Config>,
    label: &str,
    env: &HashMap<String, String>,
    flags: &FlagOverrides,
) -> std::result::Result<EffectiveSettings, String> {
    let cfg = config.ok_or_else(|| format!("account '{label}' not found: no config file"))?;
    if !cfg.accounts.contains_key(label) {
        let mut labels: Vec<&str> = cfg.accounts.keys().map(String::as_str).collect();
        labels.sort_unstable();
        return Err(format!(
            "account '{label}' not found in config (configured: {})",
            labels.join(", ")
        ));
    }
    cfg.resolve(label, None, env, flags)
        .map_err(|err| err.to_string())
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
    fn named_account_resolves_from_config() {
        let config = Config::from_toml("[accounts.alice]\ntoken = \"t\"\n").unwrap();
        let refresh = AuthRefreshArgs {
            account: Some("alice".to_owned()),
        };
        let targets = resolve_targets(
            Some(&config),
            &global(),
            &refresh,
            &env(),
            &FlagOverrides::default(),
        )
        .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "alice");
        assert_eq!(targets[0].1.stored_token.as_deref(), Some("t"));
    }

    #[test]
    fn unknown_named_account_errors() {
        let config = Config::from_toml("[accounts.alice]\ntoken = \"t\"\n").unwrap();
        let refresh = AuthRefreshArgs {
            account: Some("bob".to_owned()),
        };
        let err = resolve_targets(
            Some(&config),
            &global(),
            &refresh,
            &env(),
            &FlagOverrides::default(),
        )
        .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn all_resolves_every_account_sorted() {
        let config =
            Config::from_toml("[accounts.bob]\ntoken=\"b\"\n[accounts.alice]\ntoken=\"a\"\n")
                .unwrap();
        let refresh = AuthRefreshArgs { account: None };
        let global = GlobalArgs {
            all: true,
            ..Default::default()
        };
        let targets = resolve_targets(
            Some(&config),
            &global,
            &refresh,
            &env(),
            &FlagOverrides::default(),
        )
        .unwrap();
        let labels: Vec<&str> = targets.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["alice", "bob"]);
    }

    #[test]
    fn all_without_config_errors() {
        let refresh = AuthRefreshArgs { account: None };
        let global = GlobalArgs {
            all: true,
            ..Default::default()
        };
        let err = resolve_targets(None, &global, &refresh, &env(), &FlagOverrides::default())
            .unwrap_err();
        assert!(err.contains("--all requires"));
    }
}
