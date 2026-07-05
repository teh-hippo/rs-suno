//! `fetch`: download one clip by ID or URL into a chosen file or directory,
//! tagging MP3/FLAC output. Shares token and format resolution with the engine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use suno_core::{
    AudioFormat, ClerkAuth, Clock, Ffmpeg, Filesystem, FlagOverrides, LineageContext, SunoClient,
    TrackMetadata, tag_flac, tag_mp3,
};

use crate::cli::args::{FetchArgs, GlobalArgs};
use crate::cli::desired::ExitCode;
use crate::cli::run;
use crate::clock::TokioClock;
use crate::download;
use crate::ffmpeg::FfmpegAdapter;
use crate::fs::FsAdapter;
use crate::http::ReqwestHttp;

const WAV_POLL_ATTEMPTS: u32 = 24;
const WAV_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Run `fetch`.
pub async fn run_fetch(global: &GlobalArgs, args: &FetchArgs) -> Result<ExitCode> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let flags = FlagOverrides {
        token: global.token.clone(),
        format: args.format.map(Into::into),
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

    let id = parse_clip_id(&args.id);
    let (root, filename) = fetch_destination(
        args.output.as_deref(),
        args.dest.as_deref(),
        &id,
        settings.format,
    );

    let http = ReqwestHttp::new().context("failed to build the HTTP client")?;
    let auth = ClerkAuth::new(&token);
    if let Err(err) = auth.authenticate(&http).await {
        return Ok(run::report_auth_failure(&label, &err));
    }
    crate::cli::expiry::warn_token_expiry(&label, &auth, global.verbosity());
    let client = SunoClient::new(auth, TokioClock);

    let clip = client
        .get_clip(&http, &id)
        .await
        .context("could not fetch the clip")?;
    // A single-clip fetch has no resolution universe, so the clip stands as its
    // own root: album folders under its own title and no lineage tags.
    let lineage = LineageContext::own_root(&clip);
    let meta = TrackMetadata::from_clip(&clip, &lineage);
    let cover = download::cover(&http, &clip).await;

    let fs = FsAdapter::new(&root);
    let ffmpeg = FfmpegAdapter::new(&root);

    match settings.format {
        AudioFormat::Mp3 => {
            let url = clip.mp3_url();
            let audio = download::get_bytes(&http, &url)
                .await
                .context("could not download the MP3")?;
            let tagged = tag_mp3(&audio, &meta, cover.as_deref(), None)?;
            fs.write_atomic(&filename, &tagged)?;
        }
        AudioFormat::Flac => {
            let clock = TokioClock;
            let wav_url = ensure_wav_url(&client, &http, &clock, &id).await?;
            let wav = download::get_bytes(&http, &wav_url)
                .await
                .context("could not download the WAV")?;
            let flac = ffmpeg.wav_to_lossless(&wav, AudioFormat::Flac).await?;
            let tagged = tag_flac(&flac, &meta, cover.as_deref())?;
            fs.write_atomic(&filename, &tagged)?;
        }
        AudioFormat::Wav => {
            if global.verbosity() >= -1 {
                eprintln!(
                    "warning: WAV carries limited metadata; lyrics and album art will be omitted (use flac or mp3 for full tags)"
                );
            }
            let clock = TokioClock;
            let wav_url = ensure_wav_url(&client, &http, &clock, &id).await?;
            let wav = download::get_bytes(&http, &wav_url)
                .await
                .context("could not download the WAV")?;
            fs.write_atomic(&filename, &wav)?;
        }
    }

    if global.verbosity() >= -1 {
        eprintln!("{} ({id})", clip.title);
    }
    println!("{}", root.join(&filename).display());
    Ok(ExitCode::Ok)
}

/// Resolve the output directory and bare file name for a fetch.
///
/// `--output` names the file outright. Otherwise `DEST` is treated as a
/// directory when it exists as one or carries no extension, and as a file path
/// when it has an extension; the default is `<id>.<format>` in the current
/// directory.
fn fetch_destination(
    output: Option<&Path>,
    dest: Option<&Path>,
    id: &str,
    format: AudioFormat,
) -> (PathBuf, String) {
    let default_name = format!("{id}.{}", format.ext());
    if let Some(output) = output {
        return split_file(output, &default_name);
    }
    let dest = dest.unwrap_or_else(|| Path::new("."));
    if looks_like_dir(dest) {
        (dest.to_path_buf(), default_name)
    } else {
        split_file(dest, &default_name)
    }
}

/// Split a file path into `(parent_or_dot, file_name)`, falling back to
/// `default_name` when the path has no final component.
fn split_file(path: &Path, default_name: &str) -> (PathBuf, String) {
    match path.file_name() {
        Some(name) => {
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
            (
                parent.map_or_else(|| PathBuf::from("."), Path::to_path_buf),
                name.to_string_lossy().into_owned(),
            )
        }
        None => (path.to_path_buf(), default_name.to_owned()),
    }
}

/// Whether `dest` should be treated as a directory: it already is one, ends in a
/// separator, or carries no file extension.
fn looks_like_dir(dest: &Path) -> bool {
    if dest.is_dir() {
        return true;
    }
    dest.extension().is_none()
}

/// Resolve the rendered WAV URL, requesting a render and polling if needed.
async fn ensure_wav_url(
    client: &SunoClient<TokioClock>,
    http: &ReqwestHttp,
    clock: &impl Clock,
    id: &str,
) -> Result<String> {
    if let Some(url) = client.wav_url(http, id).await? {
        return Ok(url);
    }
    client
        .request_wav(http, id)
        .await
        .context("could not request a WAV render")?;
    for _ in 0..WAV_POLL_ATTEMPTS {
        clock.sleep(WAV_POLL_INTERVAL).await;
        if let Some(url) = client.wav_url(http, id).await? {
            return Ok(url);
        }
    }
    bail!(
        "the WAV render was not ready after {} seconds",
        u64::from(WAV_POLL_ATTEMPTS) * WAV_POLL_INTERVAL.as_secs()
    );
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
    fn destination_defaults_to_cwd_with_id_name() {
        let (root, name) = fetch_destination(None, None, "abc", AudioFormat::Flac);
        assert_eq!(root, PathBuf::from("."));
        assert_eq!(name, "abc.flac");
    }

    #[test]
    fn destination_treats_extensionless_dest_as_directory() {
        let (root, name) = fetch_destination(
            None,
            Some(Path::new("music/library")),
            "abc",
            AudioFormat::Mp3,
        );
        assert_eq!(root, PathBuf::from("music/library"));
        assert_eq!(name, "abc.mp3");
    }

    #[test]
    fn destination_treats_dest_with_extension_as_file() {
        let (root, name) = fetch_destination(
            None,
            Some(Path::new("music/song.mp3")),
            "abc",
            AudioFormat::Flac,
        );
        assert_eq!(root, PathBuf::from("music"));
        assert_eq!(name, "song.mp3");
    }

    #[test]
    fn destination_output_overrides_dest() {
        let (root, name) = fetch_destination(
            Some(Path::new("out/track.flac")),
            Some(Path::new("ignored")),
            "abc",
            AudioFormat::Flac,
        );
        assert_eq!(root, PathBuf::from("out"));
        assert_eq!(name, "track.flac");
    }

    #[test]
    fn destination_bare_filename_uses_cwd() {
        let (root, name) = fetch_destination(
            Some(Path::new("track.flac")),
            None,
            "abc",
            AudioFormat::Flac,
        );
        assert_eq!(root, PathBuf::from("."));
        assert_eq!(name, "track.flac");
    }
}
