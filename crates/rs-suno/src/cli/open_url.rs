//! Open a URL in the user's default browser, cross-platform and dependency-free.
//!
//! The command is built by a pure function so it can be unit-tested on every
//! platform without spawning a browser.

use std::process::{Command, Stdio};

/// The program and arguments that open `url` in the default browser on `os`,
/// where `os` is a [`std::env::consts::OS`] value such as `"linux"`, `"macos"`,
/// or `"windows"`.
fn open_command(os: &str, url: &str) -> (&'static str, Vec<String>) {
    match os {
        "macos" => ("open", vec![url.to_owned()]),
        // `start` treats the first quoted argument as the window title, so an
        // empty title stops the URL from being swallowed.
        "windows" => (
            "cmd",
            vec![
                "/C".to_owned(),
                "start".to_owned(),
                String::new(),
                url.to_owned(),
            ],
        ),
        // Linux, the BSDs, and anything else with freedesktop tooling.
        _ => ("xdg-open", vec![url.to_owned()]),
    }
}

/// Launch the default browser at `url`, returning `false` when no opener could
/// be spawned (for example on a headless host).
pub fn open_in_browser(url: &str) -> bool {
    let (program, args) = open_command(std::env::consts::OS, url);
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_strs(args: &[String]) -> Vec<&str> {
        args.iter().map(String::as_str).collect()
    }

    #[test]
    fn linux_uses_xdg_open() {
        let (program, args) = open_command("linux", "https://example.test/x");
        assert_eq!(program, "xdg-open");
        assert_eq!(arg_strs(&args), ["https://example.test/x"]);
    }

    #[test]
    fn macos_uses_open() {
        let (program, args) = open_command("macos", "https://example.test/x");
        assert_eq!(program, "open");
        assert_eq!(arg_strs(&args), ["https://example.test/x"]);
    }

    #[test]
    fn windows_uses_cmd_start_with_empty_title() {
        let (program, args) = open_command("windows", "https://example.test/x");
        assert_eq!(program, "cmd");
        assert_eq!(
            arg_strs(&args),
            ["/C", "start", "", "https://example.test/x"]
        );
    }

    #[test]
    fn unknown_os_falls_back_to_xdg_open() {
        let (program, _) = open_command("freebsd", "https://example.test/x");
        assert_eq!(program, "xdg-open");
    }
}
