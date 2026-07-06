//! Account and target resolution: decide which accounts run and where they
//! mirror, plus the single-account resolver for the token-only commands.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use suno_core::{Config, EffectiveSettings, FlagOverrides};

use crate::cli::args::GlobalArgs;
use crate::cli::token;

/// One planned run target: an account label and the directory it mirrors into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSpec {
    pub label: String,
    pub dest: PathBuf,
    /// True when there is no configured account; settings come from flags/env.
    pub implicit: bool,
}

/// The inputs that decide which accounts run and where.
#[derive(Debug, Clone, Copy)]
pub struct Selection<'a> {
    pub all: bool,
    pub account: Option<&'a str>,
    pub dest: Option<&'a Path>,
    pub token_available: bool,
}

/// Decide the run targets from config and the selection flags (pure).
///
/// Returns a config-error message string on any ambiguous or impossible
/// selection, which the caller surfaces as exit code 3.
pub fn plan_targets(
    config: Option<&Config>,
    sel: &Selection<'_>,
) -> std::result::Result<Vec<TargetSpec>, String> {
    if sel.all {
        let cfg = config.ok_or("--all needs a config file with at least one account")?;
        if cfg.accounts.is_empty() {
            return Err("--all: no accounts are configured".to_owned());
        }
        if sel.dest.is_some() {
            return Err(
                "--all cannot be combined with a DEST; each account uses its configured root"
                    .to_owned(),
            );
        }
        let mut labels: Vec<&String> = cfg.accounts.keys().collect();
        labels.sort();
        return labels
            .into_iter()
            .map(|label| {
                account_root(cfg, label).map(|dest| TargetSpec {
                    label: label.clone(),
                    dest,
                    implicit: false,
                })
            })
            .collect();
    }

    if let Some(account) = sel.account {
        let cfg = config.ok_or_else(|| format!("account '{account}' not found: no config file"))?;
        if !cfg.accounts.contains_key(account) {
            return Err(unknown_account_message(cfg, account));
        }
        let dest = dest_for(cfg, account, sel.dest)?;
        return Ok(vec![TargetSpec {
            label: account.to_owned(),
            dest,
            implicit: false,
        }]);
    }

    match config {
        Some(cfg) if cfg.accounts.len() == 1 => {
            let label = cfg.accounts.keys().next().expect("one account").clone();
            let dest = dest_for(cfg, &label, sel.dest)?;
            Ok(vec![TargetSpec {
                label,
                dest,
                implicit: false,
            }])
        }
        Some(cfg) if cfg.accounts.len() > 1 => {
            let mut labels: Vec<&str> = cfg.accounts.keys().map(String::as_str).collect();
            labels.sort_unstable();
            Err(format!(
                "multiple accounts configured ({}); pass --account <label> or --all",
                labels.join(", ")
            ))
        }
        _ => {
            if !sel.token_available {
                return Err(
                    "no account configured and no token provided; pass --token, set SUNO_TOKEN_COMMAND, or run 'suno config init'"
                        .to_owned(),
                );
            }
            let dest = sel
                .dest
                .map(Path::to_path_buf)
                .ok_or("a destination directory is required")?;
            Ok(vec![TargetSpec {
                label: "default".to_owned(),
                dest,
                implicit: true,
            }])
        }
    }
}

fn account_root(cfg: &Config, label: &str) -> std::result::Result<PathBuf, String> {
    cfg.accounts
        .get(label)
        .and_then(|acc| acc.root.as_deref())
        .map(PathBuf::from)
        .ok_or_else(|| format!("account '{label}' has no configured root and no DEST was given"))
}

fn dest_for(
    cfg: &Config,
    label: &str,
    dest: Option<&Path>,
) -> std::result::Result<PathBuf, String> {
    if let Some(dest) = dest {
        return Ok(dest.to_path_buf());
    }
    account_root(cfg, label)
}

fn unknown_account_message(cfg: &Config, account: &str) -> String {
    let mut labels: Vec<&str> = cfg.accounts.keys().map(String::as_str).collect();
    labels.sort_unstable();
    if labels.is_empty() {
        format!("account '{account}' not found; no accounts are configured")
    } else {
        format!(
            "account '{account}' not found in config\n\nConfigured accounts: {}",
            labels.join(", ")
        )
    }
}

/// Resolve a single account's effective settings for the token-only commands
/// (`ls`, `lsjson`, `fetch`, `auth refresh`). Pure given the loaded config.
pub(crate) fn single_account(
    config: Option<&Config>,
    global: &GlobalArgs,
    flags: &FlagOverrides,
    env: &HashMap<String, String>,
) -> std::result::Result<(String, EffectiveSettings), String> {
    let token_available = token::token_available(global, env);
    let (label, implicit) = if global.all {
        return Err(
            "this command runs a single account; pass --account instead of --all".to_owned(),
        );
    } else if let Some(account) = global.account.as_deref() {
        let cfg = config.ok_or_else(|| format!("account '{account}' not found: no config file"))?;
        if !cfg.accounts.contains_key(account) {
            return Err(unknown_account_message(cfg, account));
        }
        (account.to_owned(), false)
    } else {
        match config {
            Some(cfg) if cfg.accounts.len() == 1 => (
                cfg.accounts.keys().next().expect("one account").clone(),
                false,
            ),
            Some(cfg) if cfg.accounts.len() > 1 => {
                let mut labels: Vec<&str> = cfg.accounts.keys().map(String::as_str).collect();
                labels.sort_unstable();
                return Err(format!(
                    "multiple accounts configured ({}); pass --account <label>",
                    labels.join(", ")
                ));
            }
            _ => {
                if !token_available {
                    return Err(
                        "no account configured and no token provided; pass --token or set SUNO_TOKEN_COMMAND"
                            .to_owned()
                    );
                }
                ("default".to_owned(), true)
            }
        }
    };
    let settings = if implicit {
        synthetic_config().resolve("default", None, env, flags)
    } else {
        config
            .expect("non-implicit account has config")
            .resolve(&label, None, env, flags)
    }
    .map_err(|err| err.to_string())?;
    Ok((label, settings))
}

/// Resolve the accounts a fan-out command (`auth refresh --all`, `doctor --all`)
/// touches, or the single account otherwise, mapping each resolved account to
/// the caller's per-target shape via `make`.
///
/// `--all` lists every configured account in sorted order and errors on an empty
/// set; without it, [`single_account`] picks the sole/`--account`/token target.
/// This is the one home for that fan-out policy so `auth` and `doctor` cannot
/// drift.
pub(crate) fn resolve_all_or_single<T>(
    config: Option<&Config>,
    global: &GlobalArgs,
    flags: &FlagOverrides,
    env: &HashMap<String, String>,
    mut make: impl FnMut(String, EffectiveSettings) -> T,
) -> std::result::Result<Vec<T>, String> {
    if global.all {
        let cfg = config.ok_or_else(|| "--all requires a valid config file".to_owned())?;
        let mut labels: Vec<String> = cfg.accounts.keys().cloned().collect();
        labels.sort();
        if labels.is_empty() {
            return Err("no accounts are configured".to_owned());
        }
        return labels
            .into_iter()
            .map(|label| {
                cfg.resolve(&label, None, env, flags)
                    .map(|settings| make(label, settings))
                    .map_err(|err| err.to_string())
            })
            .collect();
    }
    let (label, settings) = single_account(config, global, flags, env)?;
    Ok(vec![make(label, settings)])
}

/// A one-account config used when running purely from `--token`/env.
pub(crate) fn synthetic_config() -> Config {
    let mut config = Config::default();
    config
        .accounts
        .insert("default".to_owned(), suno_core::AccountConfig::default());
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(accounts: &[(&str, Option<&str>)]) -> Config {
        let mut cfg = Config::default();
        for (label, root) in accounts {
            let acc = suno_core::AccountConfig {
                root: root.map(str::to_owned),
                ..Default::default()
            };
            cfg.accounts.insert((*label).to_owned(), acc);
        }
        cfg
    }

    fn sel<'a>(
        all: bool,
        account: Option<&'a str>,
        dest: Option<&'a Path>,
        token: bool,
    ) -> Selection<'a> {
        Selection {
            all,
            account,
            dest,
            token_available: token,
        }
    }

    #[test]
    fn implicit_target_needs_token_and_dest() {
        let dest = PathBuf::from("/music");
        let s = sel(false, None, Some(&dest), true);
        let targets = plan_targets(None, &s).unwrap();
        assert_eq!(targets.len(), 1);
        assert!(targets[0].implicit);
        assert_eq!(targets[0].dest, dest);
    }

    #[test]
    fn implicit_without_token_errors() {
        let dest = PathBuf::from("/music");
        let s = sel(false, None, Some(&dest), false);
        assert!(plan_targets(None, &s).is_err());
    }

    #[test]
    fn implicit_without_dest_errors() {
        let s = sel(false, None, None, true);
        assert!(plan_targets(None, &s).is_err());
    }

    #[test]
    fn single_account_accepts_implicit_token_command_env() {
        let global = GlobalArgs::default();
        let env: HashMap<String, String> =
            [("SUNO_TOKEN_COMMAND".to_owned(), "printf token".to_owned())]
                .into_iter()
                .collect();
        let (label, settings) =
            single_account(None, &global, &FlagOverrides::default(), &env).unwrap();
        assert_eq!(label, "default");
        assert_eq!(settings.token_command.as_deref(), Some("printf token"));
    }

    #[test]
    fn account_uses_dest_then_root() {
        let cfg = config_with(&[("alice", Some("/lib/alice"))]);
        let dest = PathBuf::from("/override");
        let with_dest =
            plan_targets(Some(&cfg), &sel(false, Some("alice"), Some(&dest), true)).unwrap();
        assert_eq!(with_dest[0].dest, dest);
        let from_root = plan_targets(Some(&cfg), &sel(false, Some("alice"), None, true)).unwrap();
        assert_eq!(from_root[0].dest, PathBuf::from("/lib/alice"));
    }

    #[test]
    fn account_without_dest_or_root_errors() {
        let cfg = config_with(&[("alice", None)]);
        assert!(plan_targets(Some(&cfg), &sel(false, Some("alice"), None, true)).is_err());
    }

    #[test]
    fn unknown_account_errors_with_listing() {
        let cfg = config_with(&[("alice", Some("/a")), ("bob", Some("/b"))]);
        let err = plan_targets(Some(&cfg), &sel(false, Some("carol"), None, true)).unwrap_err();
        assert!(err.contains("carol"));
        assert!(err.contains("alice"));
        assert!(err.contains("bob"));
    }

    #[test]
    fn all_runs_every_account_from_roots() {
        let cfg = config_with(&[("alice", Some("/a")), ("bob", Some("/b"))]);
        let targets = plan_targets(Some(&cfg), &sel(true, None, None, true)).unwrap();
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().all(|t| !t.implicit));
        // Sorted by label for determinism.
        assert_eq!(targets[0].label, "alice");
        assert_eq!(targets[1].label, "bob");
    }

    #[test]
    fn all_rejects_dest() {
        let cfg = config_with(&[("alice", Some("/a"))]);
        let dest = PathBuf::from("/x");
        assert!(plan_targets(Some(&cfg), &sel(true, None, Some(&dest), true)).is_err());
    }

    #[test]
    fn all_requires_roots() {
        let cfg = config_with(&[("alice", None)]);
        assert!(plan_targets(Some(&cfg), &sel(true, None, None, true)).is_err());
    }

    #[test]
    fn all_without_config_errors() {
        assert!(plan_targets(None, &sel(true, None, None, true)).is_err());
    }

    #[test]
    fn single_account_config_is_used_implicitly() {
        let cfg = config_with(&[("solo", Some("/solo"))]);
        let targets = plan_targets(Some(&cfg), &sel(false, None, None, false)).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "solo");
        assert!(!targets[0].implicit);
    }

    #[test]
    fn multiple_accounts_need_selection() {
        let cfg = config_with(&[("alice", Some("/a")), ("bob", Some("/b"))]);
        let err = plan_targets(Some(&cfg), &sel(false, None, None, true)).unwrap_err();
        assert!(err.contains("--account"));
        assert!(err.contains("--all"));
    }

    #[test]
    fn resolve_all_or_single_all_sorted() {
        let cfg = config_with(&[("charlie", None), ("alice", None), ("bob", None)]);
        let global = GlobalArgs {
            all: true,
            ..Default::default()
        };
        let labels = resolve_all_or_single(
            Some(&cfg),
            &global,
            &FlagOverrides::default(),
            &HashMap::new(),
            |label, _settings| label,
        )
        .unwrap();
        assert_eq!(labels, vec!["alice", "bob", "charlie"]);
    }

    #[test]
    fn resolve_all_or_single_all_without_config_errors() {
        let global = GlobalArgs {
            all: true,
            ..Default::default()
        };
        let err = resolve_all_or_single(
            None,
            &global,
            &FlagOverrides::default(),
            &HashMap::new(),
            |label, _settings| label,
        )
        .unwrap_err();
        assert!(err.contains("--all requires"));
    }

    #[test]
    fn resolve_all_or_single_all_empty_errors() {
        let cfg = Config::default();
        let global = GlobalArgs {
            all: true,
            ..Default::default()
        };
        let err = resolve_all_or_single(
            Some(&cfg),
            &global,
            &FlagOverrides::default(),
            &HashMap::new(),
            |label, _settings| label,
        )
        .unwrap_err();
        assert_eq!(err, "no accounts are configured");
    }

    #[test]
    fn resolve_all_or_single_single_fallback() {
        let cfg = config_with(&[("alice", None)]);
        let targets = resolve_all_or_single(
            Some(&cfg),
            &GlobalArgs::default(),
            &FlagOverrides::default(),
            &HashMap::new(),
            |label, _settings| label,
        )
        .unwrap();
        assert_eq!(targets, vec!["alice"]);
    }
}
