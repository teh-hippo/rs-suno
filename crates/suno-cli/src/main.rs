//! The `suno` command line tool.

mod http;

use anyhow::Context;
use clap::{Parser, Subcommand};
use suno_core::{ClerkAuth, SunoClient};

use crate::http::ReqwestHttp;

/// A download-only tool for mirroring your Suno.ai library.
#[derive(Parser)]
#[command(name = "suno", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List clips in your Suno library.
    Ls(LsArgs),
}

#[derive(clap::Args)]
struct LsArgs {
    /// Your Suno `__client` token (raw JWT or cookie). Reads SUNO_TOKEN if unset.
    #[arg(long, env = "SUNO_TOKEN", hide_env_values = true)]
    token: String,
    /// List only liked clips.
    #[arg(long)]
    liked: bool,
    /// Stop after the first N clips.
    #[arg(long)]
    limit: Option<usize>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Ls(args) => list(args).await,
    }
}

async fn list(args: LsArgs) -> anyhow::Result<()> {
    let http = ReqwestHttp::new().context("failed to build the HTTP client")?;

    let mut auth = ClerkAuth::new(&args.token);
    let user_id = auth
        .authenticate(&http)
        .await
        .context("authentication failed")?;
    let display_name = auth.display_name().to_string();

    let mut client = SunoClient::new(auth);
    let clips = client
        .list_clips(&http, args.liked, args.limit)
        .await
        .context("failed to list the library")?;

    eprintln!("{display_name} ({user_id}): {} clip(s)", clips.len());
    for clip in &clips {
        println!(
            "{}\t{:>7.1}s\t{}\t{}",
            clip.id,
            clip.duration,
            truncate(&clip.title, 48),
            clip.tags
        );
    }
    Ok(())
}

/// Truncate `text` to `max` characters, appending an ellipsis when shortened.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
