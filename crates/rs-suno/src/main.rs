//! The `suno` command line tool: a thin binary that parses arguments and drives
//! the IO-free `suno-core` engine through the adapters in this crate.

mod cli;
mod clock;
mod diskspace;
mod download;
mod ffmpeg;
mod fs;
mod http;
mod transcode;

use clap::Parser;

use crate::cli::args::{Cli, Command};
use crate::cli::commands::{auth, completions, config, doctor, fetch, ls, version};
use crate::cli::desired::ExitCode;
use crate::cli::run;

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install ring TLS provider: a provider is already set");
    let cli = Cli::parse();
    let code = match dispatch(cli).await {
        Ok(code) => code,
        Err(err) => {
            if crate::diskspace::anyhow_is_out_of_space(&err) {
                eprintln!("error: {}", crate::diskspace::DISK_FULL_HINT);
                ExitCode::DiskFull
            } else {
                eprintln!("error: {err:#}");
                ExitCode::General
            }
        }
    };
    std::process::exit(code.code());
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
