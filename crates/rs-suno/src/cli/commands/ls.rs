//! `ls` and `lsjson`: authenticate one account, list its clips, apply the same
//! recency/limit selection the engine uses, and print a table or NDJSON.

use std::collections::HashMap;
use std::io::IsTerminal;

use crate::clock::TokioClock;
use anyhow::{Context, Result};
use suno_core::select::{RecencySpec, SelectParams, select};
use suno_core::{ClerkAuth, FlagOverrides, SunoClient};

use crate::cli::args::{GlobalArgs, LsArgs, OutputFormat};
use crate::cli::desired::ExitCode;
use crate::cli::failure;
use crate::cli::output;
use crate::cli::run;
use crate::cli::wallclock;
use crate::http::ReqwestHttp;

/// Run `ls` (or `lsjson` when `force_json`).
pub async fn run_ls(global: &GlobalArgs, args: &LsArgs, force_json: bool) -> Result<ExitCode> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let flags = FlagOverrides {
        token: global.token.clone(),
        ..FlagOverrides::default()
    };

    let config = match run::load_config_reported(global.config.as_deref()) {
        Ok(config) => config,
        Err(code) => return Ok(code),
    };
    let (label, settings) = match run::single_account(config.as_ref(), global, &flags, &env) {
        Ok(resolved) => resolved,
        Err(message) => {
            eprintln!("error: {message}");
            return Ok(ExitCode::Config);
        }
    };
    let token = match run::resolve_token(&label, &settings).await {
        Ok(Some(token)) => token,
        Ok(None) => {
            eprintln!(
                "error: no token for account '{label}'; pass --token, set SUNO_TOKEN or SUNO_TOKEN_COMMAND, or set token/token_command in config"
            );
            return Ok(ExitCode::Config);
        }
        Err(err) => {
            eprintln!("error: {err}");
            return Ok(ExitCode::Config);
        }
    };

    let since = match args.since.as_deref().map(RecencySpec::parse).transpose() {
        Ok(since) => since,
        Err(message) => {
            eprintln!("error: {message}");
            return Ok(ExitCode::Config);
        }
    };

    let http = ReqwestHttp::new().context("failed to build the HTTP client")?;
    let auth = ClerkAuth::new(&token);
    let user_id = match auth.authenticate(&http).await {
        Ok(user_id) => user_id,
        Err(err) => return Ok(failure::report_auth_failure(&label, &err)),
    };
    let display_name = auth.display_name().to_owned();
    crate::cli::expiry::warn_token_expiry(&label, &auth, global.verbosity());
    let client = SunoClient::new(auth, TokioClock);

    let (clips, _complete, _) = match client.list_clips(&http, args.liked, args.limit).await {
        Ok(result) => result,
        Err(err) => return Ok(failure::report_listing_failure(&label, &err)),
    };

    let params = SelectParams {
        limit: args.limit,
        since,
        min_newest: settings.min_newest as usize,
        now: wallclock::now_secs(),
        last_run: None,
    };
    let selected = select(&clips, &params);

    let json = force_json || args.format == OutputFormat::Json;
    if json {
        for clip in &selected {
            println!("{}", output::lsjson_line(clip));
        }
    } else {
        if std::io::stdout().is_terminal() {
            println!("{}", output::ls_header());
        }
        for clip in &selected {
            println!("{}", output::ls_row(clip));
        }
    }

    if global.verbosity() >= -1 {
        eprintln!("{display_name} ({user_id}): {} clip(s)", selected.len());
    }
    Ok(ExitCode::Ok)
}
