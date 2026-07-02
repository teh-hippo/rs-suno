//! Execute a configured `token_command` and return its trimmed stdout as a
//! token string. Runs `sh -c <cmd>` on Unix and `cmd /c <cmd>` on Windows.
//!
//! This is deliberate IO that belongs in the CLI adapter, not in suno-core.

use std::process::Command;

use anyhow::{Context, Result, bail};

/// Run `command` in a shell and return the trimmed stdout.
///
/// Errors on a non-zero exit, empty output, or spawn failure. Never logs or
/// prints the command's output (it may contain a secret).
pub fn resolve_token(command: &str) -> Result<String> {
    let output = shell_command(command)
        .output()
        .context("failed to run token_command")?;

    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_owned());
        bail!("token_command exited with status {code}");
    }

    let token = String::from_utf8(output.stdout)
        .context("token_command output is not valid UTF-8")?
        .trim()
        .to_owned();

    if token.is_empty() {
        bail!("token_command produced empty output");
    }

    Ok(token)
}

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(command);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_returns_trimmed_output() {
        let token = resolve_token("echo test-token").unwrap();
        assert_eq!(token, "test-token");
    }

    #[test]
    fn trims_whitespace() {
        let token = resolve_token("echo '  spaced  '").unwrap();
        assert_eq!(token, "spaced");
    }

    #[test]
    fn nonzero_exit_errors() {
        let err = resolve_token("exit 1").unwrap_err();
        assert!(
            err.to_string().contains("exited with status 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn empty_output_errors() {
        let err = resolve_token("echo -n ''").unwrap_err();
        assert!(
            err.to_string().contains("empty output"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn nonexistent_command_errors() {
        let err = resolve_token("this-command-does-not-exist-xyz 2>/dev/null").unwrap_err();
        // The shell returns a non-zero exit; it might be 127 or similar.
        assert!(err.to_string().contains("exited with status"));
    }
}
