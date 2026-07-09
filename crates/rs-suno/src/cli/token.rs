//! Token resolution and `token_command` subprocess spawning.
//!
//! [`resolve_token`] prefers a direct `--token`/env token, then a
//! `token_command` (run through the platform shell so it can pipe and expand),
//! then a stored token. [`token_available`] answers the pure question of whether
//! any token source is present, for the account-selection logic. The shell is
//! the only platform-specific part, isolated in [`token_command_process`].

use std::collections::HashMap;
use std::process::{Command, ExitStatus};

use crate::cli::args::GlobalArgs;

pub(crate) async fn resolve_token(
    label: &str,
    settings: &suno_core::EffectiveSettings,
) -> std::result::Result<Option<String>, String> {
    if let Some(token) = settings.token.clone() {
        return Ok(Some(token));
    }
    if let Some(command) = settings.token_command.as_deref() {
        return run_token_command(label, command).await.map(Some);
    }
    Ok(settings.stored_token.clone())
}

async fn run_token_command(label: &str, command: &str) -> std::result::Result<String, String> {
    let command = command.to_owned();
    // Run the child process off the async runtime so a slow token_command never
    // stalls other tasks; tokio lacks the `process` feature here, so this wraps
    // the blocking `std::process::Command` rather than using `tokio::process`.
    let output = tokio::task::spawn_blocking(move || token_command_process(&command).output())
        .await
        .map_err(|err| format!("token_command for account '{label}' did not complete: {err}"))?
        .map_err(|err| format!("could not run token_command for account '{label}': {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "token_command for account '{label}' failed with {}",
            exit_status_summary(output.status)
        ));
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| {
        format!(
            "token_command for account '{label}' produced non-UTF-8 output; stdout must be UTF-8"
        )
    })?;
    let token = stdout.trim();
    if token.is_empty() {
        return Err(format!(
            "token_command for account '{label}' produced empty output"
        ));
    }
    Ok(token.to_owned())
}

/// Build the process that runs a `token_command` under the platform's default
/// shell, so a command can use pipes and expansion (for example
/// `pass show suno | head -1`). The shell is the only platform-specific part:
/// `sh -c` on Unix, `cmd /C` on Windows.
#[cfg(unix)]
fn token_command_process(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(windows)]
fn token_command_process(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(command);
    cmd
}

fn exit_status_summary(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => "termination by signal".to_owned(),
    }
}

pub(crate) fn token_available(global: &GlobalArgs, env: &HashMap<String, String>) -> bool {
    if global.token.is_some()
        || env.contains_key("SUNO_TOKEN")
        || env.contains_key("SUNO_TOKEN_COMMAND")
    {
        return true;
    }
    // A token-only run without a configured account falls back to the implicit
    // `default` account, so also honour that label's per-account env vars (and an
    // explicit --account), matching the resolver's prefix via `label_to_env`.
    let prefix = suno_core::config::label_to_env(global.account.as_deref().unwrap_or("default"));
    env.contains_key(&format!("SUNO_{prefix}_TOKEN"))
        || env.contains_key(&format!("SUNO_{prefix}_TOKEN_COMMAND"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(
        token: Option<&str>,
        stored_token: Option<&str>,
        token_command: Option<&str>,
    ) -> suno_core::EffectiveSettings {
        suno_core::EffectiveSettings {
            token: token.map(str::to_owned),
            stored_token: stored_token.map(str::to_owned),
            token_command: token_command.map(str::to_owned),
            account_id: None,
            format: suno_core::AudioFormat::Flac,
            concurrency: 4,
            retries: 3,
            min_newest: 1,
            animated_covers: false,
            raw_animated_cover: false,
            video_cover_retention: suno_core::VideoCoverRetention::Neither,
            animated_cover_webp: suno_core::WebpEncodeSettings::default(),
            details_sidecar: false,
            lyrics_sidecar: false,
            lrc_sidecar: false,
            video_mp4: false,
            download_stems: false,
            stem_format: suno_core::StemFormat::Wav,
            naming_template: "{title}".to_owned(),
            character_set: suno_core::CharacterSet::Unicode,
            areas: None,
            album_overrides: std::collections::BTreeMap::new(),
            lead_tracks: Vec::new(),
            number_singletons: true,
        }
    }

    #[cfg(unix)]
    fn success_command(token: &str) -> String {
        format!("printf '%s\\n' {}", shell_single_quote(token))
    }

    #[cfg(unix)]
    fn fail_command(output: &str) -> String {
        format!("printf '%s' {}; exit 23", shell_single_quote(output))
    }

    #[cfg(unix)]
    fn shell_single_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    #[test]
    fn token_available_detects_every_source() {
        // A token is available from the global --token flag, SUNO_TOKEN or
        // SUNO_TOKEN_COMMAND, or the resolved account's per-account TOKEN /
        // TOKEN_COMMAND (the default account unless --account is given). Another
        // account's env var does not count.
        struct Row {
            label: &'static str,
            global: GlobalArgs,
            env: &'static [(&'static str, &'static str)],
            want: bool,
        }
        let rows = [
            Row {
                label: "global --token flag",
                global: GlobalArgs {
                    token: Some("flag-token".to_owned()),
                    ..Default::default()
                },
                env: &[],
                want: true,
            },
            Row {
                label: "SUNO_TOKEN",
                global: GlobalArgs::default(),
                env: &[("SUNO_TOKEN", "env-token")],
                want: true,
            },
            Row {
                label: "SUNO_TOKEN_COMMAND",
                global: GlobalArgs::default(),
                env: &[("SUNO_TOKEN_COMMAND", "printf secret")],
                want: true,
            },
            Row {
                label: "default account SUNO_DEFAULT_TOKEN",
                global: GlobalArgs::default(),
                env: &[("SUNO_DEFAULT_TOKEN", "env-token")],
                want: true,
            },
            Row {
                label: "explicit account SUNO_MY_LIB_TOKEN_COMMAND",
                global: GlobalArgs {
                    account: Some("my-lib".to_owned()),
                    ..Default::default()
                },
                env: &[("SUNO_MY_LIB_TOKEN_COMMAND", "printf secret")],
                want: true,
            },
            Row {
                label: "another account's token is ignored",
                global: GlobalArgs::default(),
                env: &[("SUNO_ALICE_TOKEN", "env-token")],
                want: false,
            },
        ];
        for Row {
            label,
            global,
            env,
            want,
        } in rows
        {
            let env: HashMap<String, String> = env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            assert_eq!(super::token_available(&global, &env), want, "{label}");
        }
    }

    #[tokio::test]
    async fn resolve_token_prefers_direct_token_over_stored_token() {
        let settings = settings_with(Some("flag-token"), Some("stored-token"), None);
        let token = resolve_token("alice", &settings).await.unwrap();
        assert_eq!(token.as_deref(), Some("flag-token"));
    }

    #[tokio::test]
    async fn resolve_token_falls_back_to_stored_token() {
        let settings = settings_with(None, Some("stored-token"), None);
        let token = resolve_token("alice", &settings).await.unwrap();
        assert_eq!(token.as_deref(), Some("stored-token"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_token_uses_trimmed_command_stdout() {
        let settings = settings_with(
            None,
            Some("stored-token"),
            Some(&success_command("cmd-token")),
        );
        let token = resolve_token("alice", &settings).await.unwrap();
        assert_eq!(token.as_deref(), Some("cmd-token"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_token_command_failure_is_clear_and_redacted() {
        let secret = "secret-command-output";
        let settings = settings_with(None, Some("stored-token"), Some(&fail_command(secret)));
        let err = resolve_token("alice", &settings).await.unwrap_err();
        assert!(err.contains("token_command for account 'alice' failed with exit status 23"));
        assert!(!err.contains(secret), "error leaked command output: {err}");
        assert!(!err.contains("stored-token"), "error leaked token: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_token_command_rejects_whitespace_output() {
        let settings = settings_with(None, Some("stored-token"), Some("printf '   \\n\\t'"));
        let err = resolve_token("alice", &settings).await.unwrap_err();
        assert!(err.contains("produced empty output"));
        assert!(!err.contains("stored-token"), "error leaked token: {err}");
    }
}
