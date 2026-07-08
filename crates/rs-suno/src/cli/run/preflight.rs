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
