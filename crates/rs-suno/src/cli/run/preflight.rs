//! The preflight phase: resolve settings, authenticate, and load the durable
//! lineage store before a single feed request is made.

use super::*;

/// Resolve settings, mint an authenticated client, and load the durable store,
/// before any feed request. Returns the ready context, or an [`ExitCode`] for
/// every preflight refusal (bad config, missing token or ffmpeg, an auth
/// failure, or an unidentifiable account) so the caller returns it unchanged.
pub(super) async fn preflight(
    target: &account::TargetSpec,
    config: Option<&Config>,
    flags: &FlagOverrides,
    env: &HashMap<String, String>,
    verbosity: i8,
) -> Result<std::result::Result<Preflight, ExitCode>> {
    let settings = {
        let resolved = if target.implicit {
            account::synthetic_config().resolve("default", None, env, flags)
        } else {
            config
                .expect("non-implicit target has config")
                .resolve(&target.label, None, env, flags)
        };
        match resolved {
            Ok(settings) => settings,
            Err(err) => {
                eprint_t!("error: {err}");
                return Ok(Err(ExitCode::Config));
            }
        }
    };

    // `video_cover_retention` resolves `animated_covers` last, so a `neither`/`mp4`
    // retention in any config tier silently defeats an explicit `--animated-covers`
    // on the command line (the documented precedence, `config/resolve.rs`). Say so
    // once when it happens rather than dropping the flag in silence (#357); the
    // precedence itself is unchanged, this only reports it.
    note_animated_covers_override(flags, &settings, verbosity);

    let token = match token::resolve_token(&target.label, &settings).await {
        Ok(Some(token)) => token,
        Ok(None) => {
            eprint_t!(
                "error: no token for account '{}'; pass --token, set SUNO_TOKEN or SUNO_TOKEN_COMMAND, or set token/token_command in config",
                target.label
            );
            return Ok(Err(ExitCode::Config));
        }
        Err(err) => {
            eprint_t!("error: {err}");
            return Ok(Err(ExitCode::Config));
        }
    };

    if settings.requires_ffmpeg() && version::ffmpeg_version().is_none() {
        eprint_t!(
            "error: ffmpeg is required for {} output{} but was not found on PATH; \
             install ffmpeg or switch to mp3 format",
            settings.format,
            if settings.animated_covers && !settings.raw_animated_cover {
                " and animated WebP covers"
            } else if settings.animated_covers {
                " and animated covers"
            } else {
                ""
            }
        );
        return Ok(Err(ExitCode::Config));
    }

    let http = ReqwestHttp::new().context("failed to build the HTTP client")?;
    let dest = &target.dest;
    let auth = ClerkAuth::new(&token);
    if let Err(err) = auth.authenticate(&http).await {
        return Ok(Err(failure::report_auth_failure(&target.label, &err)));
    }
    let account = auth.display_name().to_owned();
    crate::cli::expiry::warn_token_expiry(&target.label, &auth, verbosity);
    // Fail closed: the identity guard cannot run without an authenticated id,
    // and proceeding would delete against an unverified account. authenticate()
    // already errors on a missing id; this makes the invariant explicit.
    let Some(user_id) = auth.user_id() else {
        eprint_t!(
            "error: could not determine the authenticated account for '{}'. Refusing to run to protect the library.",
            target.label
        );
        return Ok(Err(ExitCode::Auth));
    };

    // Load the durable store up front so the identity guard can compare the
    // authenticated account against the account this library is pinned to,
    // before a single feed request is made. A mismatch aborts so a swapped or
    // mistyped token can never make another account's clips look absent from
    // source and delete this library's files.
    let mut store = logs::load_graph(dest)?;
    // Derive the eligible-root set from the loaded cache so overrides and
    // collision detection are correct even on a resolution-failed run (where
    // `store.update` is skipped below); a successful run refreshes it again.
    store.refresh_eligible_roots();
    // Layer this account's manual album-name overrides onto the store before any
    // album title is resolved, so the folder path, ALBUM tag, change hash, index
    // and disambiguation all reflect the preferred name from one source.
    store.set_album_overrides(settings.album_overrides.clone());

    let client = SunoClient::new(auth, TokioClock);
    Ok(Ok(Preflight {
        settings,
        http,
        client,
        store,
        user_id,
        account,
    }))
}

/// Print a one-line note when an explicit `--animated-covers` was silently
/// overridden by a `video_cover_retention` that keeps no animated WebP cover.
///
/// The precedence (`video_cover_retention` resolves `animated_covers` last) is
/// documented and intentional; this only surfaces the drop so it is not silent
/// (#357). Guarded by `verbosity >= -1` to mirror the other preflight `note:`s.
fn note_animated_covers_override(
    flags: &FlagOverrides,
    settings: &suno_core::EffectiveSettings,
    verbosity: i8,
) {
    if animated_covers_flag_overridden(flags.settings.animated_covers, settings.animated_covers)
        && verbosity >= -1
    {
        eprint_t!(
            "note: --animated-covers is overridden by video_cover_retention = \"{}\"; \
             set video_cover_retention = \"webp\" or \"both\" to keep the animated cover",
            settings.video_cover_retention
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::task_output::{capture_task_stderr, flush_task_stderr};
    use std::collections::HashMap;
    use suno_core::{Config, FlagOverrides, Settings};

    /// Resolve settings from an inline TOML with the given `video_cover_retention`,
    /// under a `FlagOverrides` that sets `--animated-covers` when `flag` is true.
    fn resolve(retention: &str, flag: bool) -> suno_core::EffectiveSettings {
        let toml = format!("[defaults]\nvideo_cover_retention = \"{retention}\"\n[accounts.a]\n");
        let cfg = Config::from_toml(&toml).unwrap();
        let flags = FlagOverrides {
            settings: Settings {
                animated_covers: flag.then_some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.resolve("a", None, &HashMap::new(), &flags).unwrap()
    }

    /// Capture the `eprint_t!` note (if any) emitted for the given retention and
    /// flag at the given verbosity, driving the real predicate + message.
    fn note_lines(retention: &str, flag: bool, verbosity: i8) -> Vec<String> {
        let settings = resolve(retention, flag);
        let flags = FlagOverrides {
            settings: Settings {
                animated_covers: flag.then_some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        capture_task_stderr();
        note_animated_covers_override(&flags, &settings, verbosity);
        flush_task_stderr()
    }

    #[test]
    fn note_emitted_when_retention_overrides_the_flag() {
        for retention in ["neither", "mp4"] {
            let lines = note_lines(retention, true, 0);
            assert_eq!(lines.len(), 1, "expected one note for {retention}");
            assert!(
                lines[0].contains("--animated-covers is overridden by video_cover_retention"),
                "note wording for {retention}: {}",
                lines[0]
            );
            assert!(
                lines[0].contains(retention),
                "note names the retention {retention}: {}",
                lines[0]
            );
        }
    }

    #[test]
    fn note_silent_when_flag_is_honoured() {
        for retention in ["webp", "both"] {
            let lines = note_lines(retention, true, 0);
            assert!(
                lines.is_empty(),
                "no note when {retention} keeps the animated cover: {lines:?}"
            );
        }
    }

    #[test]
    fn note_silent_when_flag_absent() {
        for retention in ["neither", "mp4", "webp", "both"] {
            let lines = note_lines(retention, false, 0);
            assert!(
                lines.is_empty(),
                "no note without --animated-covers ({retention}): {lines:?}"
            );
        }
    }

    #[test]
    fn note_suppressed_below_minus_one_verbosity() {
        // `-qq` (verbosity -2) silences notes, matching the other preflight `note:`s.
        let lines = note_lines("neither", true, -2);
        assert!(lines.is_empty(), "note suppressed at -2: {lines:?}");
    }
}
