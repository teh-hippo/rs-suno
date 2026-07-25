//! The `suno` command line tool: a thin binary that parses arguments and drives
//! the IO-free `suno-core` engine through the adapters in this crate.

#![forbid(unsafe_code)]
// Mirror the never-panic contract `suno-core` sets in its lib.rs. The CLI owns
// the documented exit-code contract (0-9, docs/src/scheduling-and-exit-codes.md)
// and a panic bypasses it entirely, handing the user a stack trace instead of a
// code a scheduler can act on. Denying outside tests forces every intentional
// site to carry an explicit `#[allow]` with a justification.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented
    )
)]

mod cli;
mod clock;
mod diskspace;
mod download;
mod ffmpeg;
mod fs;
mod http;
mod scratch;
mod transcode;

use clap::Parser;

use crate::cli::args::{Cli, Command};
use crate::cli::commands::{auth, completions, config, doctor, fetch, ls, version};
use crate::cli::desired::ExitCode;
use crate::cli::run;

#[tokio::main]
async fn main() {
    let code = match run().await {
        Ok(code) => code,
        Err(err) => report(&err),
    };
    std::process::exit(code.code());
}

/// Install the TLS provider, then parse and dispatch.
async fn run() -> anyhow::Result<ExitCode> {
    install_crypto_provider()?;
    dispatch(Cli::parse()).await
}

/// Install the process-wide rustls provider. Fails only if one is already set,
/// which cannot happen before `main` has run, but reporting it beats a panic:
/// the exit-code contract is the CLI's whole interface to a scheduler.
fn install_crypto_provider() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install the ring TLS provider"))
}

/// Print an error and map it to the process exit code.
fn report(err: &anyhow::Error) -> ExitCode {
    if crate::diskspace::anyhow_is_out_of_space(err) {
        eprintln!("error: {}", crate::diskspace::DISK_FULL_HINT);
        ExitCode::DiskFull
    } else {
        eprintln!("error: {err:#}");
        ExitCode::General
    }
}

/// Route a parsed command to its handler, returning the process exit code.
async fn dispatch(cli: Cli) -> anyhow::Result<ExitCode> {
    let global = cli.global;
    match cli.command {
        Command::Sync(args) => run::run_sync(&global, &args).await,
        Command::Copy(args) => run::run_copy(&global, &args).await,
        Command::Check(args) => run::run_check(&global, &args.sync, args.exit_code).await,
        Command::Ls(args) => ls::run_ls(&global, &args, false).await,
        Command::Lsjson(args) => ls::run_ls(&global, &args, true).await,
        Command::Fetch(args) => fetch::run_fetch(&global, &args).await,
        Command::Config(args) => config::run_config(&global, &args),
        Command::Auth(args) => auth::run_auth(&global, &args).await,
        Command::Doctor => doctor::run_doctor(&global).await,
        Command::Version => version::run_version(&global),
        Command::Completions(args) => Ok(completions::run_completions(&args)),
    }
}
