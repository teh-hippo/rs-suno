//! Config loading and the CLI-to-`FlagOverrides` mapping.

use std::path::Path;

use anyhow::{Context, Result};
use suno_core::{Config, FlagOverrides};

use crate::cli::args::{GlobalArgs, SyncArgs};
use crate::cli::desired::ExitCode;
use crate::cli::logs;

pub(crate) enum ConfigState {
    Loaded(Config),
    Absent,
    Error(String),
}

/// Load config, printing a config error and returning its exit code on failure.
pub(crate) fn load_config_reported(
    override_path: Option<&Path>,
) -> std::result::Result<Option<Config>, ExitCode> {
    match load_config(override_path) {
        Ok(ConfigState::Loaded(cfg)) => Ok(Some(cfg)),
        Ok(ConfigState::Absent) => Ok(None),
        Ok(ConfigState::Error(message)) => {
            eprintln!("error: {message}");
            Err(ExitCode::Config)
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            Err(ExitCode::General)
        }
    }
}

/// Load config from the override or platform default. A missing default file is
/// `Absent`; a missing explicit `--config`, or a parse error, is an error.
pub(crate) fn load_config(override_path: Option<&Path>) -> Result<ConfigState> {
    let explicit = override_path.is_some();
    let Some(path) = logs::config_path(override_path) else {
        return Ok(ConfigState::Absent);
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match Config::from_toml(&text) {
            Ok(cfg) => Ok(ConfigState::Loaded(cfg)),
            Err(err) => Ok(ConfigState::Error(format!("{}: {err}", path.display()))),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if explicit {
                Ok(ConfigState::Error(format!(
                    "config file not found: {}",
                    path.display()
                )))
            } else {
                Ok(ConfigState::Absent)
            }
        }
        Err(err) => Err(err).with_context(|| format!("could not read {}", path.display())),
    }
}

pub(crate) fn flag_overrides(global: &GlobalArgs, args: &SyncArgs) -> FlagOverrides {
    FlagOverrides {
        token: global.token.clone(),
        format: args.format.map(Into::into),
        concurrency: args.concurrency,
        retries: args.retries,
        min_newest: args.min_newest,
        // A presence-only toggle can only enable; absence defers to config/env.
        animated_covers: args.animated_covers.then_some(true),
        video_cover_retention: args.video_cover_retention.map(Into::into),
        animated_cover_quality: args.animated_cover_quality,
        animated_cover_max_fps: args.animated_cover_max_fps,
        animated_cover_max_width: args.animated_cover_max_width,
        animated_cover_compression_level: args.animated_cover_compression_level,
        animated_cover_lossless: args.animated_cover_lossless.then_some(true),
        details_sidecar: args.details_sidecar.then_some(true),
        lyrics_sidecar: args.lyrics_sidecar.then_some(true),
        lrc_sidecar: args.lrc_sidecar.then_some(true),
        video_mp4: args.video_mp4.then_some(true),
        download_stems: args.download_stems.then_some(true),
        stem_format: args.stem_format.map(Into::into),
        naming_template: args.naming_template.clone(),
        character_set: args.character_set.map(Into::into),
    }
}
