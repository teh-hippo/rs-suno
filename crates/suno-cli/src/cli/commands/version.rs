//! `version`: report the build version, target triple, resolved config path, and
//! the detected ffmpeg (the same `ffmpeg` on `PATH` the transcoder runs).

use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;

use crate::cli::args::GlobalArgs;
use crate::cli::desired::ExitCode;
use crate::cli::logs;

/// Run `version`.
pub fn run_version(global: &GlobalArgs) -> Result<ExitCode> {
    println!(
        "suno {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("SUNO_TARGET")
    );
    match logs::config_path(global.config.as_deref()) {
        Some(path) => println!("config: {}", path.display()),
        None => println!("config: (none)"),
    }
    match ffmpeg_version() {
        Some((version, path)) => println!("ffmpeg: {version} (detected at {path})"),
        None => println!("ffmpeg: not found on PATH"),
    }
    Ok(ExitCode::Ok)
}

/// The detected ffmpeg `(version, path)`, or `None` when it is not runnable.
fn ffmpeg_version() -> Option<(String, String)> {
    let path = find_in_path("ffmpeg")?;
    let output = Command::new(&path).arg("-version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = parse_ffmpeg_version(&stdout);
    Some((version, path.display().to_string()))
}

/// Pull the version token from ffmpeg's `-version` banner
/// (`ffmpeg version 6.1.1 Copyright ...`).
fn parse_ffmpeg_version(banner: &str) -> String {
    banner
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(2))
        .unwrap_or("unknown")
        .to_owned()
}

/// Find an executable named `name` on `PATH`.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if cfg!(windows) {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_from_banner() {
        let banner =
            "ffmpeg version 6.1.1 Copyright (c) 2000-2023 the FFmpeg developers\nbuilt with gcc";
        assert_eq!(parse_ffmpeg_version(banner), "6.1.1");
    }

    #[test]
    fn unknown_when_banner_is_unexpected() {
        assert_eq!(parse_ffmpeg_version("garbage"), "unknown");
        assert_eq!(parse_ffmpeg_version(""), "unknown");
    }
}
