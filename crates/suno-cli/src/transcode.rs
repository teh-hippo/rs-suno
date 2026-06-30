//! The ffmpeg adapter: transcode WAV bytes to FLAC bytes.
//!
//! This is the engine's ffmpeg port realised with a child process. ffmpeg
//! reads and writes seekable temporary files so it patches `STREAMINFO`
//! (notably `total_samples`), which a non-seekable pipe would leave at zero and
//! make players report an unknown duration. Tagging is handled separately by
//! the pure core tagger; this step only re-encodes the audio.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Hard cap on a single ffmpeg transcode before we kill it.
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);
/// How often to check whether ffmpeg has finished.
const FFMPEG_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Transcode `wav` to FLAC bytes, staging temporary files in `scratch_dir`.
pub fn wav_to_flac(wav: &[u8], scratch_dir: &Path) -> Result<Vec<u8>> {
    let stamp = unique_stamp();
    let wav_path = scratch_dir.join(format!(".{stamp}.wav"));
    let flac_path = scratch_dir.join(format!(".{stamp}.flac"));
    let _scratch = Scratch(vec![wav_path.clone(), flac_path.clone()]);

    std::fs::write(&wav_path, wav)
        .with_context(|| format!("could not stage WAV at {}", wav_path.display()))?;

    let mut child = Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(&wav_path)
        .args(["-map", "0:a:0", "-c:a", "flac", "-f", "flac"])
        .arg(&flac_path)
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
        bail!(
            "ffmpeg failed to transcode WAV to FLAC: {}",
            stderr_tail(&stderr)
        );
    }

    std::fs::read(&flac_path)
        .with_context(|| format!("could not read transcoded FLAC at {}", flac_path.display()))
}

/// The last few lines of ffmpeg's stderr, for a concise error message.
fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    let start = lines.len().saturating_sub(3);
    lines[start..].join("; ")
}

/// A process- and call-unique stamp for temporary file names.
fn unique_stamp() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("suno-{}-{nanos}-{seq}", std::process::id())
}

/// Removes its temporary paths when dropped, even on the error path.
struct Scratch(Vec<PathBuf>);

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the real ffmpeg pipeline: a file-output FLAC carries a complete
    /// `STREAMINFO` so the duration is correct. Ignored because CI has no
    /// ffmpeg; run locally with `cargo test -p suno-cli -- --ignored`.
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
        let flac = wav_to_flac(&wav, &dir).unwrap();
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
}
