//! `doctor`: diagnose environment, config, live auth, and remaining credits.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use suno_core::{
    ClerkAuth, Config, EffectiveSettings, FlagOverrides, SunoClient, TOKEN_EXPIRY_WARN_DAYS,
    TokenExpiry,
};

use crate::cli::args::GlobalArgs;
use crate::cli::commands::version;
use crate::cli::desired::ExitCode;
use crate::cli::logs;
use crate::cli::run;
use crate::clock::TokioClock;
use crate::http::ReqwestHttp;

const SECS_PER_DAY: i64 = 86_400;

/// Run `doctor`.
pub async fn run_doctor(global: &GlobalArgs) -> Result<ExitCode> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let flags = FlagOverrides {
        token: global.token.clone(),
        ..FlagOverrides::default()
    };
    let config_path = logs::config_path(global.config.as_deref());
    let config_diag = inspect_config(
        config_path.clone(),
        global.config.is_some() || env.contains_key("SUNO_CONFIG"),
    )?;

    let mut out = String::new();
    let mut worst = config_diag.exit_code.unwrap_or(ExitCode::Ok);
    writeln!(
        out,
        "suno {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("SUNO_TARGET")
    )
    .ok();
    let ffmpeg = version::ffmpeg_version();
    match &ffmpeg {
        Some((found, path)) => writeln!(out, "ffmpeg: {found} (detected at {path})").ok(),
        None => writeln!(out, "ffmpeg: not found on PATH").ok(),
    };
    render_env_section(&mut out, &env);
    render_config_section(&mut out, &config_diag);

    match resolve_targets(config_diag.config.as_ref(), global, &env, &flags) {
        Ok(targets) if targets.is_empty() => {}
        Ok(targets) => {
            let http = ReqwestHttp::new().context("failed to build the HTTP client")?;
            for target in targets {
                render_account_header(&mut out, &target.label);
                render_account_env(&mut out, &env, &target.label);
                render_account_config(&mut out, target.root.as_deref());
                render_resolved_settings(&mut out, &target.settings);
                if target.settings.requires_ffmpeg() && ffmpeg.is_none() {
                    writeln!(
                        out,
                        "  ffmpeg: required for {} output but not found on PATH",
                        target.settings.format
                    )
                    .ok();
                    worst = max_exit_code(worst, ExitCode::Config);
                }
                match target.settings.token.as_deref() {
                    Some(token) => {
                        let live = inspect_live(token, &http).await;
                        render_live_status(&mut out, &live);
                        worst = max_exit_code(worst, live.exit_code);
                    }
                    None => {
                        writeln!(out, "  auth: skipped (no token resolved)").ok();
                        writeln!(out, "  credits: skipped (auth unavailable)").ok();
                        worst = max_exit_code(worst, ExitCode::Config);
                    }
                }
            }
        }
        Err(message) => {
            writeln!(out).ok();
            writeln!(out, "account selection: {message}").ok();
            worst = max_exit_code(worst, ExitCode::Config);
        }
    }

    print!("{out}");
    Ok(worst)
}

struct ConfigDiagnostic {
    path: Option<PathBuf>,
    config: Option<Config>,
    status: String,
    exit_code: Option<ExitCode>,
}

fn inspect_config(path: Option<PathBuf>, explicit: bool) -> Result<ConfigDiagnostic> {
    let Some(path) = path else {
        return Ok(ConfigDiagnostic {
            path: None,
            config: None,
            status: "unavailable (no config directory could be determined)".to_owned(),
            exit_code: None,
        });
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match Config::from_toml(&text) {
            Ok(config) => {
                let status = format!("parsed ({} account(s))", config.accounts.len());
                Ok(ConfigDiagnostic {
                    path: Some(path),
                    config: Some(config),
                    status,
                    exit_code: None,
                })
            }
            Err(err) => Ok(ConfigDiagnostic {
                path: Some(path),
                config: None,
                status: format!("invalid ({err})"),
                exit_code: Some(ExitCode::Config),
            }),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ConfigDiagnostic {
            path: Some(path),
            config: None,
            status: "not found".to_owned(),
            exit_code: explicit.then_some(ExitCode::Config),
        }),
        Err(err) => Ok(ConfigDiagnostic {
            path: Some(path),
            config: None,
            status: format!("unreadable ({err})"),
            exit_code: Some(ExitCode::Config),
        }),
    }
}

struct DoctorTarget {
    label: String,
    settings: EffectiveSettings,
    root: Option<String>,
}

fn resolve_targets(
    config: Option<&Config>,
    global: &GlobalArgs,
    env: &HashMap<String, String>,
    flags: &FlagOverrides,
) -> std::result::Result<Vec<DoctorTarget>, String> {
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
                let settings = cfg
                    .resolve(&label, None, env, flags)
                    .map_err(|err| err.to_string())?;
                let root = cfg
                    .accounts
                    .get(&label)
                    .and_then(|account| account.root.clone());
                Ok(DoctorTarget {
                    label,
                    settings,
                    root,
                })
            })
            .collect();
    }

    let (label, settings) = run::single_account(config, global, flags, env)?;
    let root = config
        .and_then(|cfg| cfg.accounts.get(&label))
        .and_then(|account| account.root.clone());
    Ok(vec![DoctorTarget {
        label,
        settings,
        root,
    }])
}

struct LiveDiagnostic {
    auth_line: String,
    credits_line: String,
    exit_code: ExitCode,
}

async fn inspect_live(token: &str, http: &ReqwestHttp) -> LiveDiagnostic {
    let mut auth = ClerkAuth::new(token);
    let now = i64::try_from(run::now_secs()).unwrap_or(i64::MAX);
    let expiry = auth.token_expiry(now, TOKEN_EXPIRY_WARN_DAYS * SECS_PER_DAY);
    match auth.authenticate(http).await {
        Ok(user_id) => {
            let display_name = auth.display_name().to_owned();
            let mut client = SunoClient::new(auth, TokioClock);
            match client.get_billing_info(http).await {
                Ok(billing) => LiveDiagnostic {
                    auth_line: format!(
                        "ok as {display_name} ({user_id}); token expiry: {}",
                        describe_expiry(expiry)
                    ),
                    credits_line: format!("{} remaining", billing.total_credits_left),
                    exit_code: ExitCode::Ok,
                },
                Err(err) => LiveDiagnostic {
                    auth_line: format!(
                        "ok as {display_name} ({user_id}); token expiry: {}",
                        describe_expiry(expiry)
                    ),
                    credits_line: format!("unavailable ({err})"),
                    exit_code: exit_code_for_core_error(&err),
                },
            }
        }
        Err(err) => LiveDiagnostic {
            auth_line: format!("failed ({err}); token expiry: {}", describe_expiry(expiry)),
            credits_line: "skipped (auth failed)".to_owned(),
            exit_code: exit_code_for_core_error(&err),
        },
    }
}

fn render_env_section(out: &mut String, env: &HashMap<String, String>) {
    writeln!(out).ok();
    writeln!(out, "env:").ok();
    match env.get("SUNO_CONFIG") {
        Some(path) => writeln!(out, "  SUNO_CONFIG: set ({path})").ok(),
        None => writeln!(out, "  SUNO_CONFIG: unset").ok(),
    };
    writeln!(
        out,
        "  SUNO_TOKEN: {}",
        present(env.contains_key("SUNO_TOKEN"))
    )
    .ok();
}

fn render_config_section(out: &mut String, config: &ConfigDiagnostic) {
    writeln!(out).ok();
    writeln!(out, "config:").ok();
    match &config.path {
        Some(path) => writeln!(out, "  path: {}", path.display()).ok(),
        None => writeln!(out, "  path: (none)").ok(),
    };
    writeln!(out, "  status: {}", config.status).ok();
    if let Some(config) = &config.config {
        let mut labels: Vec<&str> = config.accounts.keys().map(String::as_str).collect();
        labels.sort_unstable();
        writeln!(out, "  accounts: {}", labels.join(", ")).ok();
    }
}

fn render_account_header(out: &mut String, label: &str) {
    writeln!(out).ok();
    writeln!(out, "account '{label}':").ok();
}

fn render_account_env(out: &mut String, env: &HashMap<String, String>, label: &str) {
    let name = format!("SUNO_{}_TOKEN", label_to_env(label));
    writeln!(out, "  env:").ok();
    writeln!(out, "    {name}: {}", present(env.contains_key(&name))).ok();
}

fn render_account_config(out: &mut String, root: Option<&str>) {
    writeln!(out, "  config:").ok();
    match root {
        Some(root) => writeln!(out, "    root: {root}").ok(),
        None => writeln!(out, "    root: [not set]").ok(),
    };
}

fn render_resolved_settings(out: &mut String, settings: &EffectiveSettings) {
    writeln!(out, "  resolved:").ok();
    writeln!(
        out,
        "    token: {}",
        if settings.token.is_some() {
            "[redacted]"
        } else {
            "[not set]"
        }
    )
    .ok();
    writeln!(
        out,
        "    account_id: {}",
        settings.account_id.as_deref().unwrap_or("[not set]")
    )
    .ok();
    writeln!(out, "    format: {}", settings.format).ok();
    writeln!(out, "    concurrency: {}", settings.concurrency).ok();
    writeln!(out, "    retries: {}", settings.retries).ok();
    writeln!(out, "    min_newest: {}", settings.min_newest).ok();
    writeln!(out, "    animated_covers: {}", settings.animated_covers).ok();
    writeln!(out, "    details_sidecar: {}", settings.details_sidecar).ok();
    writeln!(out, "    lyrics_sidecar: {}", settings.lyrics_sidecar).ok();
    writeln!(out, "    lrc_sidecar: {}", settings.lrc_sidecar).ok();
    writeln!(out, "    video_mp4: {}", settings.video_mp4).ok();
    writeln!(out, "    download_stems: {}", settings.download_stems).ok();
    writeln!(out, "    stem_format: {}", settings.stem_format).ok();
    writeln!(out, "    naming_template: {}", settings.naming_template).ok();
    writeln!(out, "    character_set: {}", settings.character_set).ok();
    writeln!(
        out,
        "    areas: {}",
        if settings.areas.is_some() {
            "configured"
        } else {
            "[not set]"
        }
    )
    .ok();
}

fn render_live_status(out: &mut String, live: &LiveDiagnostic) {
    writeln!(out, "  auth: {}", live.auth_line).ok();
    writeln!(out, "  credits: {}", live.credits_line).ok();
}

fn describe_expiry(expiry: TokenExpiry) -> String {
    match expiry {
        TokenExpiry::Fresh => "fresh".to_owned(),
        TokenExpiry::Unknown => "unknown".to_owned(),
        TokenExpiry::Expiring { days } => format!("expiring in {days} day(s)"),
        TokenExpiry::Expired => "expired".to_owned(),
    }
}

fn exit_code_for_core_error(err: &suno_core::Error) -> ExitCode {
    match err {
        suno_core::Error::Auth(_) => ExitCode::Auth,
        suno_core::Error::Config(_) => ExitCode::Config,
        suno_core::Error::Connection(_) | suno_core::Error::RateLimited { .. } => {
            ExitCode::Transient
        }
        _ => ExitCode::General,
    }
}

fn label_to_env(label: &str) -> String {
    label.to_ascii_uppercase().replace('-', "_")
}

fn present(yes: bool) -> &'static str {
    if yes { "set" } else { "unset" }
}

fn max_exit_code(a: ExitCode, b: ExitCode) -> ExitCode {
    if b.code() >= a.code() { b } else { a }
}

#[cfg(test)]
mod tests {
    use super::*;
    use suno_core::{AudioFormat, CharacterSet};

    fn settings() -> EffectiveSettings {
        EffectiveSettings {
            token: Some("eyJsupersecret".to_owned()),
            stored_token: None,
            token_command: None,
            account_id: Some("acct-123".to_owned()),
            format: AudioFormat::Flac,
            concurrency: 4,
            retries: 3,
            min_newest: 1,
            animated_covers: true,
            raw_animated_cover: true,
            video_cover_retention: suno_core::VideoCoverRetention::Both,
            animated_cover_webp: suno_core::WebpEncodeSettings::default(),
            details_sidecar: false,
            lyrics_sidecar: true,
            lrc_sidecar: false,
            video_mp4: true,
            download_stems: false,
            stem_format: suno_core::StemFormat::Wav,
            naming_template: "{title}/{id8}".to_owned(),
            character_set: CharacterSet::Ascii,
            areas: None,
            album_overrides: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn render_resolved_settings_redacts_token() {
        let mut out = String::new();
        render_resolved_settings(&mut out, &settings());
        assert!(out.contains("token: [redacted]"));
        assert!(!out.contains("eyJsupersecret"));
        assert!(out.contains("format: flac"));
        assert!(out.contains("character_set: ascii"));
    }

    #[test]
    fn describe_expiry_covers_every_state() {
        assert_eq!(describe_expiry(TokenExpiry::Fresh), "fresh");
        assert_eq!(describe_expiry(TokenExpiry::Unknown), "unknown");
        assert_eq!(
            describe_expiry(TokenExpiry::Expiring { days: 2 }),
            "expiring in 2 day(s)"
        );
        assert_eq!(describe_expiry(TokenExpiry::Expired), "expired");
    }

    #[test]
    fn inspect_config_marks_default_missing_as_non_fatal() {
        let stamp = format!("doctor-missing-{}", std::process::id());
        let path = std::env::temp_dir().join(stamp);
        let diag = inspect_config(Some(path.clone()), false).unwrap();
        assert_eq!(diag.path.as_deref(), Some(path.as_path()));
        assert_eq!(diag.status, "not found");
        assert_eq!(diag.exit_code, None);
    }

    #[test]
    fn inspect_config_marks_explicit_missing_as_config_error() {
        let stamp = format!("doctor-explicit-missing-{}", std::process::id());
        let path = std::env::temp_dir().join(stamp);
        let diag = inspect_config(Some(path), true).unwrap();
        assert_eq!(diag.status, "not found");
        assert_eq!(diag.exit_code, Some(ExitCode::Config));
    }

    #[test]
    fn max_exit_code_selects_higher() {
        assert_eq!(
            max_exit_code(ExitCode::Ok, ExitCode::Config),
            ExitCode::Config
        );
        assert_eq!(
            max_exit_code(ExitCode::Auth, ExitCode::Config),
            ExitCode::Auth
        );
        assert_eq!(max_exit_code(ExitCode::Ok, ExitCode::Ok), ExitCode::Ok);
    }

    #[test]
    fn ffmpeg_required_flac_no_ffmpeg_raises_config() {
        let mut s = settings();
        s.format = AudioFormat::Flac;
        s.animated_covers = false;
        // Simulate ffmpeg absent: if requires_ffmpeg() and no binary, exit = Config.
        let ffmpeg: Option<(String, String)> = None;
        let mut worst = ExitCode::Ok;
        if s.requires_ffmpeg() && ffmpeg.is_none() {
            worst = max_exit_code(worst, ExitCode::Config);
        }
        assert_eq!(worst, ExitCode::Config);
    }

    #[test]
    fn ffmpeg_not_required_mp3_no_ffmpeg_stays_ok() {
        let mut s = settings();
        s.format = AudioFormat::Mp3;
        s.animated_covers = false;
        s.raw_animated_cover = false;
        let ffmpeg: Option<(String, String)> = None;
        let mut worst = ExitCode::Ok;
        if s.requires_ffmpeg() && ffmpeg.is_none() {
            worst = max_exit_code(worst, ExitCode::Config);
        }
        assert_eq!(worst, ExitCode::Ok);
    }
}
