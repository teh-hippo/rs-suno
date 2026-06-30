//! The `suno` command line tool.

mod download;
mod http;
mod transcode;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use suno_core::{ClerkAuth, Clip, SunoClient, TrackMetadata, tag_flac, tag_mp3};

use crate::http::ReqwestHttp;

/// How long to wait for a server-side WAV render before giving up.
const WAV_POLL_ATTEMPTS: u32 = 24;
const WAV_POLL_INTERVAL: Duration = Duration::from_secs(5);

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
    /// Download a single clip's audio and write its metadata tags.
    Fetch(FetchArgs),
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

#[derive(clap::Args)]
struct FetchArgs {
    /// The clip ID or a Suno URL containing it.
    id: String,
    /// Your Suno `__client` token (raw JWT or cookie). Reads SUNO_TOKEN if unset.
    #[arg(long, env = "SUNO_TOKEN", hide_env_values = true)]
    token: String,
    /// Audio format to download.
    #[arg(long, value_enum, default_value_t = Format::Flac, env = "SUNO_FORMAT")]
    format: Format,
}

/// The audio format `fetch` produces.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Format {
    Mp3,
    Flac,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Ls(args) => list(args).await,
        Command::Fetch(args) => fetch(args).await,
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

async fn fetch(args: FetchArgs) -> anyhow::Result<()> {
    let http = ReqwestHttp::new().context("failed to build the HTTP client")?;
    let id = parse_clip_id(&args.id);

    let mut auth = ClerkAuth::new(&args.token);
    auth.authenticate(&http)
        .await
        .context("authentication failed")?;
    let mut client = SunoClient::new(auth);

    let clip = client
        .get_clip(&http, &id)
        .await
        .context("could not fetch the clip")?;
    let meta = TrackMetadata::from_clip(&clip);
    let cover = download::cover(&http, &clip).await;

    let dir = Path::new("downloads");
    std::fs::create_dir_all(dir).context("could not create the downloads directory")?;

    let path = match args.format {
        Format::Mp3 => {
            let url = mp3_source_url(&clip);
            let audio = download::get_bytes(&http, &url)
                .await
                .context("could not download the MP3")?;
            let tagged = tag_mp3(&audio, &meta, cover.as_deref())?;
            let path = dir.join(format!("{id}.mp3"));
            download::write_atomic(&path, &tagged)?;
            path
        }
        Format::Flac => {
            let wav_url = ensure_wav_url(&mut client, &http, &id).await?;
            let wav = download::get_bytes(&http, &wav_url)
                .await
                .context("could not download the WAV")?;
            let flac = transcode::wav_to_flac(&wav, dir)?;
            let tagged = tag_flac(&flac, &meta, cover.as_deref())?;
            let path = dir.join(format!("{id}.flac"));
            download::write_atomic(&path, &tagged)?;
            path
        }
    };

    eprintln!("{} ({id})", clip.title);
    println!("{}", path.display());
    Ok(())
}

/// Resolve the rendered WAV URL, requesting a render and polling if needed.
async fn ensure_wav_url(
    client: &mut SunoClient,
    http: &ReqwestHttp,
    id: &str,
) -> anyhow::Result<String> {
    if let Some(url) = client.wav_url(http, id).await? {
        return Ok(url);
    }
    client
        .request_wav(http, id)
        .await
        .context("could not request a WAV render")?;
    for _ in 0..WAV_POLL_ATTEMPTS {
        tokio::time::sleep(WAV_POLL_INTERVAL).await;
        if let Some(url) = client.wav_url(http, id).await? {
            return Ok(url);
        }
    }
    bail!(
        "the WAV render was not ready after {} seconds",
        u64::from(WAV_POLL_ATTEMPTS) * WAV_POLL_INTERVAL.as_secs()
    );
}

/// The MP3 source URL for `clip`, falling back to the deterministic CDN URL
/// when the clip carries no `audio_url` (mirrors ha-suno).
fn mp3_source_url(clip: &Clip) -> String {
    if clip.audio_url.is_empty() {
        format!("https://cdn1.suno.ai/{}.mp3", clip.id)
    } else {
        clip.audio_url.clone()
    }
}

/// Extract a clip ID from a bare ID or a Suno URL.
fn parse_clip_id(input: &str) -> String {
    let trimmed = input.trim();
    let without_query = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    let segment = without_query
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(without_query);
    segment
        .rsplit_once('.')
        .map_or(segment, |(stem, _ext)| stem)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clip_id_accepts_a_bare_id() {
        assert_eq!(parse_clip_id("  abc-123  "), "abc-123");
    }

    #[test]
    fn parse_clip_id_extracts_from_a_url() {
        assert_eq!(parse_clip_id("https://suno.com/song/abc-123"), "abc-123");
    }

    #[test]
    fn parse_clip_id_strips_query_and_extension() {
        assert_eq!(
            parse_clip_id("https://cdn1.suno.ai/abc-123.mp3?token=x"),
            "abc-123"
        );
    }

    #[test]
    fn mp3_source_url_prefers_the_clip_audio_url() {
        let clip = Clip {
            id: "abc-123".to_owned(),
            audio_url: "https://cdn1.suno.ai/real.mp3".to_owned(),
            ..Default::default()
        };
        assert_eq!(mp3_source_url(&clip), "https://cdn1.suno.ai/real.mp3");
    }

    #[test]
    fn mp3_source_url_synthesises_the_cdn_url_when_empty() {
        let clip = Clip {
            id: "abc-123".to_owned(),
            audio_url: String::new(),
            ..Default::default()
        };
        assert_eq!(mp3_source_url(&clip), "https://cdn1.suno.ai/abc-123.mp3");
    }
}
