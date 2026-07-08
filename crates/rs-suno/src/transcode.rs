//! The ffmpeg adapter: transcode WAV bytes to FLAC bytes, and MP4 preview bytes
//! to animated WebP cover bytes.
//!
//! The FLAC path reads and writes seekable temporary files so ffmpeg patches
//! `STREAMINFO` (notably `total_samples`), which a non-seekable pipe would leave
//! at zero and make players report an unknown duration. The WebP path has no
//! such requirement, so it streams the MP4 in and the WebP out over pipes,
//! draining both output streams on threads to avoid a pipe deadlock. Tagging is
//! handled separately by the pure core tagger; these steps only re-encode media.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use suno_core::{AudioFormat, WebpEncodeSettings};

use crate::scratch::{Scratch, unique_stamp};

/// Hard cap on a single ffmpeg transcode before we kill it.
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);
/// Longer cap for the animated-WebP transcode: a full-resolution lossless encode
/// is single-threaded and legitimately runs into the minutes, so it needs more
/// headroom than the audio path. The lossy default finishes in seconds.
const WEBP_FFMPEG_TIMEOUT: Duration = Duration::from_secs(600);
/// How often to check whether ffmpeg has finished.
const FFMPEG_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Transcode `wav` to the given lossless `format`'s bytes, staging temporary
/// files in `scratch_dir`.
pub fn wav_to_lossless(wav: &[u8], format: AudioFormat, scratch_dir: &Path) -> Result<Vec<u8>> {
    let (codec, container) = lossless_codec_args(format)?;
    let stamp = unique_stamp();
    let wav_path = scratch_dir.join(format!(".{stamp}.wav"));
    let out_path = scratch_dir.join(format!(".{stamp}.{}", format.ext()));
    let _scratch = Scratch::all(vec![wav_path.clone(), out_path.clone()]);

    std::fs::write(&wav_path, wav)
        .with_context(|| format!("could not stage WAV at {}", wav_path.display()))?;

    let mut child = Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(&wav_path)
        .args(["-map", "0:a:0", "-c:a", codec, "-f", container])
        .arg(&out_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run ffmpeg (is it installed?)")?;

    let deadline = Instant::now() + FFMPEG_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().context("could not wait for ffmpeg")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "ffmpeg timed out after {} seconds",
                FFMPEG_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(FFMPEG_POLL_INTERVAL);
    };

    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr);
    }

    if !status.success() {
        // ffmpeg reports a full disk only as stderr text with no io::Error. The
        // scratch dir is the library destination, so a disk that fills mid-encode
        // (the WAV staged, but no room for the WAV+output pair) would otherwise
        // degrade to a per-clip skip and repeat for every clip. Probe the scratch
        // dir: if a tiny write also hits ENOSPC, carry a real out-of-space
        // io::Error so the adapter classifies this as a disk-full run abort.
        if let Some(err) = scratch_out_of_space(scratch_dir) {
            return Err(
                anyhow::Error::new(err).context(format!("disk full while transcoding to {format}"))
            );
        }
        bail!(
            "ffmpeg failed to transcode WAV to {format}: {}",
            stderr_tail(&stderr)
        );
    }

    std::fs::read(&out_path).with_context(|| {
        format!(
            "could not read transcoded {format} at {}",
            out_path.display()
        )
    })
}

/// Map a lossless output format to its ffmpeg audio codec and container.
///
/// Only formats rs-suno transcodes from the WAV render belong here; MP3 is a
/// source download and WAV the raw render, so neither is transcoded.
fn lossless_codec_args(format: AudioFormat) -> Result<(&'static str, &'static str)> {
    match format {
        AudioFormat::Flac => Ok(("flac", "flac")),
        AudioFormat::Alac => Ok(("alac", "ipod")),
        other => bail!("wav_to_lossless cannot encode {other}"),
    }
}

/// Transcode an MP4 preview to animated WebP bytes under `settings`.
///
/// The MP4 streams in on stdin and the WebP streams out on stdout, so no
/// temporary files are staged. Both output pipes are drained on their own
/// threads while a third feeds stdin, because ffmpeg interleaves writing the
/// encoded frames with reading the input: draining only after `wait` would
/// deadlock once a pipe buffer fills.
pub fn mp4_to_webp(mp4: &[u8], settings: WebpEncodeSettings) -> Result<Vec<u8>> {
    let mut child = Command::new("ffmpeg")
        .arg("-y")
        .args(["-i", "pipe:0", "-an"])
        .args(["-vf", &video_filter(&settings)])
        .args(["-c:v", "libwebp_anim"])
        .args(quality_args(&settings))
        .args(compression_args(&settings))
        .args(["-loop", "0", "-f", "webp", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run ffmpeg (is it installed?)")?;

    // Feed stdin on its own thread, then close it so ffmpeg sees EOF.
    let mut stdin = child.stdin.take().context("ffmpeg stdin was not piped")?;
    let input = mp4.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        drop(stdin);
    });

    // Drain stdout and stderr concurrently to avoid a full-pipe deadlock.
    let mut out_pipe = child.stdout.take().context("ffmpeg stdout was not piped")?;
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let mut err_pipe = child.stderr.take().context("ffmpeg stderr was not piped")?;
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + WEBP_FFMPEG_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().context("could not wait for ffmpeg")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "ffmpeg timed out after {} seconds",
                WEBP_FFMPEG_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(FFMPEG_POLL_INTERVAL);
    };

    let _ = feeder.join();
    let webp = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    if !status.success() {
        bail!(
            "ffmpeg failed to transcode MP4 to WebP: {}",
            stderr_tail(&stderr)
        );
    }
    if webp.is_empty() {
        bail!("ffmpeg produced an empty WebP: {}", stderr_tail(&stderr));
    }
    Ok(webp)
}

/// The `-vf` chain: cap the frame rate, optionally scale a wider source down,
/// then convert to RGB honouring the source's colour tags.
///
/// With a width cap, scale a wider source down (never upscaling) to an even
/// height; `None` keeps the source resolution. The trailing `format=rgba` makes
/// ffmpeg (not libwebp) do the YUV→RGB conversion using the source's declared
/// range and matrix: this is bit-exact for a lossless encode and measurably more
/// colour-faithful for a lossy one, closing a range/matrix drift that libwebp's
/// internal conversion otherwise introduces.
fn video_filter(settings: &WebpEncodeSettings) -> String {
    match settings.max_width {
        Some(width) => format!(
            "scale='min({width},iw)':-2,fps={},format=rgba",
            settings.max_fps
        ),
        None => format!("fps={},format=rgba", settings.max_fps),
    }
}

/// The quality flags: a lossless switch, or the lossy `-q:v` scale.
fn quality_args(settings: &WebpEncodeSettings) -> Vec<String> {
    if settings.lossless {
        vec!["-lossless".to_owned(), "1".to_owned()]
    } else {
        vec!["-q:v".to_owned(), settings.quality.to_string()]
    }
}

/// The compression-effort flag from the configured 0-6 effort level.
fn compression_args(settings: &WebpEncodeSettings) -> Vec<String> {
    vec![
        "-compression_level".to_owned(),
        settings.compression_level.to_string(),
    ]
}

/// The last few lines of ffmpeg's stderr, for a concise error message.
fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    let start = lines.len().saturating_sub(3);
    lines[start..].join("; ")
}

/// Probe `dir` for out-of-space by writing a tiny hidden file.
///
/// Returns `Some(err)` only when the probe write fails with an out-of-space
/// error (proving the disk is full); a successful probe or any other error
/// returns `None`, so a genuine encode failure on a healthy disk stays a
/// per-clip skip. The probe file is removed best-effort in every case.
fn scratch_out_of_space(dir: &Path) -> Option<std::io::Error> {
    let probe = dir.join(format!(".suno-space-probe-{}", unique_stamp()));
    let result = std::fs::write(&probe, b"0");
    let _ = std::fs::remove_file(&probe);
    match result {
        Ok(()) => None,
        Err(err) if crate::diskspace::is_out_of_space(&err) => Some(err),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_webp_filter_bounds_width_and_caps_fps() {
        // The default bounds the width to 640 px so the embedded animated cover
        // reliably fits the FLAC picture cap, then caps fps and converts to RGB.
        assert_eq!(
            video_filter(&WebpEncodeSettings::default()),
            "scale='min(640,iw)':-2,fps=24,format=rgba"
        );
        // No width cap keeps the source resolution: only the fps cap applies.
        let native = WebpEncodeSettings {
            max_width: None,
            ..Default::default()
        };
        assert_eq!(video_filter(&native), "fps=24,format=rgba");
        // An explicit width cap scales a wider source down to an even height.
        let capped = WebpEncodeSettings {
            max_width: Some(720),
            ..Default::default()
        };
        assert_eq!(
            video_filter(&capped),
            "scale='min(720,iw)':-2,fps=24,format=rgba"
        );
    }

    #[test]
    fn lossy_quality_uses_q_scale_and_default_compression_effort() {
        let settings = WebpEncodeSettings::default();
        assert_eq!(quality_args(&settings), vec!["-q:v", "90"]);
        // Effort defaults to level 4; callers can choose any level 0-4.
        assert_eq!(compression_args(&settings), vec!["-compression_level", "4"]);
        let full_effort = WebpEncodeSettings {
            compression_level: 4,
            ..Default::default()
        };
        assert_eq!(
            compression_args(&full_effort),
            vec!["-compression_level", "4"]
        );
    }

    #[test]
    fn lossless_and_no_compression_flip_the_flags() {
        let settings = WebpEncodeSettings {
            lossless: true,
            compression_level: 2,
            ..Default::default()
        };
        assert_eq!(quality_args(&settings), vec!["-lossless", "1"]);
        assert_eq!(compression_args(&settings), vec!["-compression_level", "2"]);
    }

    #[test]
    fn scratch_probe_is_none_on_a_writable_dir() {
        // A healthy, writable scratch dir never reports out-of-space, so a real
        // encode failure there stays a per-clip skip. The true-ENOSPC branch
        // reuses the already-tested `is_out_of_space` and is not unit-testable
        // without a genuinely full disk.
        let dir = Path::new("target").join(format!("space-probe-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(scratch_out_of_space(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flac_codec_args_pin_the_default_output() {
        // FLAC encodes with codec, container, and extension all "flac"; pinning
        // them keeps the default output byte-identical as new formats are added.
        assert_eq!(
            lossless_codec_args(AudioFormat::Flac).unwrap(),
            ("flac", "flac")
        );
        assert_eq!(AudioFormat::Flac.ext(), "flac");
    }

    #[test]
    fn wav_to_lossless_rejects_non_transcoded_formats() {
        // MP3 is a source download and WAV the raw render; neither is transcoded
        // here, so they are rejected before any disk or ffmpeg work.
        let dir = Path::new("target").join("no-such-scratch");
        assert!(wav_to_lossless(b"", AudioFormat::Mp3, &dir).is_err());
        assert!(wav_to_lossless(b"", AudioFormat::Wav, &dir).is_err());
    }

    /// Proves the real ffmpeg pipeline: a file-output FLAC carries a complete
    /// `STREAMINFO` so the duration is correct. Ignored because CI has no
    /// ffmpeg; run locally with `cargo test -p rs-suno -- --ignored`.
    #[test]
    #[ignore = "requires ffmpeg and ffprobe"]
    fn wav_to_flac_yields_correct_duration() {
        let dir = Path::new("target").join("transcode-smoke");
        std::fs::create_dir_all(&dir).unwrap();
        let wav_path = dir.join("tone.wav");
        let made = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-ar",
                "44100",
                "-ac",
                "2",
            ])
            .arg(&wav_path)
            .status()
            .unwrap();
        assert!(made.success());

        let wav = std::fs::read(&wav_path).unwrap();
        let flac = wav_to_lossless(&wav, AudioFormat::Flac, &dir).unwrap();
        assert_eq!(&flac[..4], b"fLaC");

        let flac_path = dir.join("out.flac");
        std::fs::write(&flac_path, &flac).unwrap();
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=nokey=1:noprint_wrappers=1",
            ])
            .arg(&flac_path)
            .output()
            .unwrap();
        let duration: f64 = String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse()
            .unwrap();
        assert!((duration - 2.0).abs() < 0.1, "duration was {duration}");

        let _ = std::fs::remove_file(&wav_path);
        let _ = std::fs::remove_file(&flac_path);
    }

    /// Proves the real animated-WebP pipeline: a generated MP4 streams through
    /// ffmpeg over pipes and yields a non-empty RIFF/WEBP file. Ignored because
    /// CI has no ffmpeg; run locally with `cargo test -p rs-suno -- --ignored`.
    #[test]
    #[ignore = "requires ffmpeg with libwebp_anim"]
    fn mp4_to_webp_yields_a_riff_webp() {
        let dir = Path::new("target").join("transcode-smoke");
        std::fs::create_dir_all(&dir).unwrap();
        let mp4_path = dir.join("preview.mp4");
        let made = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=640x360:rate=30:duration=2",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&mp4_path)
            .status()
            .unwrap();
        assert!(made.success());

        let mp4 = std::fs::read(&mp4_path).unwrap();
        let webp = mp4_to_webp(&mp4, WebpEncodeSettings::default()).unwrap();
        assert!(!webp.is_empty());
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");

        let _ = std::fs::remove_file(&mp4_path);
    }
}
