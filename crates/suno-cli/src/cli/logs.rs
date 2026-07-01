//! On-disk bookkeeping the CLI owns: the manifest, the run lock, the audit and
//! failure logs, and config-path resolution.
//!
//! These are the engine's own records, not library audio, so the CLI touches
//! them with `std::fs` directly rather than through the rooted [`Filesystem`]
//! adapter. The manifest is saved atomically (temp sibling then rename) and a
//! single exclusive lock prevents two runs from corrupting it.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use suno_core::{Action, Clip, Failure, LineageStore, Manifest, Plan};

use crate::download::write_atomic;

/// The manifest file name, kept beside the mirrored library.
pub const MANIFEST_NAME: &str = ".suno-manifest.json";
/// The lineage graph store file name, kept beside the manifest.
///
/// The store and its persistence are added ahead of use: the run flow wires
/// them in a later phase (persist before execute and on interrupt, HARDENING
/// H4), so these entry points are intentionally unreferenced for now.
#[allow(dead_code)]
pub const GRAPH_NAME: &str = ".suno-lineage.json";
const LOCK_NAME: &str = ".suno.lock";
const FAILURES_NAME: &str = ".suno-failures.log";
const AUDIT_NAME: &str = ".suno-audit.log";

/// Resolve the effective config path: the explicit override, else the platform
/// default. Returns `None` only when no home or config directory can be found.
pub fn config_path(override_path: Option<&Path>) -> Option<PathBuf> {
    override_path
        .map(Path::to_path_buf)
        .or_else(default_config_path)
}

/// The platform default config path.
///
/// `$SUNO_CONFIG` is handled by clap; this is `%APPDATA%/suno/config.toml` on
/// Windows and `$XDG_CONFIG_HOME` (or `~/.config`) `/suno/config.toml`
/// elsewhere.
pub fn default_config_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|base| PathBuf::from(base).join("suno").join("config.toml"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|base| base.join("suno").join("config.toml"))
    }
}

/// Load the manifest beside `dest`, returning an empty one when absent.
///
/// A present-but-unparseable manifest is an error rather than a silent empty:
/// treating a corrupt prior as empty would drop the `preserve` markers and
/// re-download the whole library, so the run must stop and let the user fix it.
pub fn load_manifest(dest: &Path) -> Result<Manifest> {
    let path = dest.join(MANIFEST_NAME);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("the manifest at {} is corrupt", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::new()),
        Err(err) => Err(err).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Save `manifest` beside `dest` atomically.
pub fn save_manifest(dest: &Path, manifest: &Manifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest).context("could not serialise the manifest")?;
    write_atomic(&dest.join(MANIFEST_NAME), &bytes).context("could not write the manifest")
}

/// Load the lineage graph store beside `dest`, returning an empty one when absent.
///
/// Unlike the manifest, the graph store is an append-durable archive, so a
/// present-but-unparseable file is an error rather than a silent empty: treating
/// a corrupt prior as empty would discard archived (often trashed) ancestors
/// that cannot be re-fetched once Suno purges them, so the run must stop.
#[allow(dead_code)]
pub fn load_graph(dest: &Path) -> Result<LineageStore> {
    let path = dest.join(GRAPH_NAME);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("the lineage store at {} is corrupt", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(LineageStore::new()),
        Err(err) => Err(err).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Save the lineage graph `store` beside `dest` atomically.
#[allow(dead_code)]
pub fn save_graph(dest: &Path, store: &LineageStore) -> Result<()> {
    let bytes =
        serde_json::to_vec_pretty(store).context("could not serialise the lineage store")?;
    write_atomic(&dest.join(GRAPH_NAME), &bytes).context("could not write the lineage store")
}

/// An exclusive run lock, removed when dropped.
pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Acquire the single-run lock beside `dest`, failing when another run holds it.
pub fn acquire_lock(dest: &Path) -> Result<LockGuard> {
    let path = dest.join(LOCK_NAME);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            let _ = writeln!(file, "{}", std::process::id());
            Ok(LockGuard { path })
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => bail!(
            "another suno run is active (lock at {}); remove it if no run is in progress",
            path.display()
        ),
        Err(err) => Err(err).with_context(|| format!("could not create lock {}", path.display())),
    }
}

/// Append one line per failed clip to `.suno-failures.log` (id, title, url, error).
pub fn append_failures(
    dest: &Path,
    failures: &[Failure],
    clips: &HashMap<&str, &Clip>,
) -> Result<()> {
    if failures.is_empty() {
        return Ok(());
    }
    let now = iso_utc(unix_now());
    let mut buf = String::new();
    for failure in failures {
        let (title, url) = clips
            .get(failure.clip_id.as_str())
            .map(|clip| (clip.title.as_str(), clip.audio_url.as_str()))
            .unwrap_or(("", ""));
        buf.push_str(&format!(
            "{now}\t{}\t{title}\t{url}\t{}\n",
            failure.clip_id, failure.reason
        ));
    }
    append(&dest.join(FAILURES_NAME), &buf)
}

/// Append every applied deletion and rename to `.suno-audit.log`.
///
/// An action is skipped when its executor outcome was a failure, so the log
/// only records changes that actually happened. A delete is keyed by `clip_id`;
/// a rename carries no clip id, so it is keyed by the clip that owns its
/// destination path (`rename_owner`, falling back to the path itself, mirroring
/// how the executor attributes a rename failure).
pub fn append_audit(
    dest: &Path,
    plan: &Plan,
    failed: &HashSet<&str>,
    rename_owner: &HashMap<&str, &str>,
) -> Result<()> {
    let now = iso_utc(unix_now());
    let mut buf = String::new();
    for action in &plan.actions {
        match action {
            Action::Delete { path, clip_id } if !failed.contains(clip_id.as_str()) => {
                buf.push_str(&format!("{now}\tDELETE\t{clip_id}\t{path}\t\n"));
            }
            Action::Rename { from, to } => {
                let owner = rename_owner
                    .get(to.as_str())
                    .copied()
                    .unwrap_or(to.as_str());
                if !failed.contains(owner) {
                    buf.push_str(&format!("{now}\tRENAME\t\t{from}\t{to}\n"));
                }
            }
            _ => {}
        }
    }
    if buf.is_empty() {
        return Ok(());
    }
    append(&dest.join(AUDIT_NAME), &buf)
}

/// Append `text` to `path`, creating it if needed.
fn append(path: &Path, text: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("could not append to {}", path.display()))
}

/// Current Unix time in seconds, saturating to 0 before the epoch.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format Unix seconds as an ISO 8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
fn iso_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let (year, month, day) = days_to_civil(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert days since the Unix epoch to a civil `(year, month, day)`.
///
/// Howard Hinnant's algorithm, the inverse of the one in `select.rs`.
fn days_to_civil(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use suno_core::{AudioFormat, ManifestEntry, Resolution, ResolveStatus, RootInfo};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = Path::new("target").join(format!(
            "logs-{tag}-{}-{seq}-{}",
            std::process::id(),
            unix_now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn iso_utc_formats_known_instants() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_710_080_521), "2024-03-10T14:22:01Z");
    }

    #[test]
    fn load_missing_manifest_is_empty() {
        let dir = temp_dir("missing");
        assert!(load_manifest(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = temp_dir("roundtrip");
        let mut manifest = Manifest::new();
        manifest.insert(
            "clip",
            ManifestEntry {
                path: "a.flac".to_owned(),
                format: AudioFormat::Flac,
                meta_hash: "m".to_owned(),
                art_hash: "a".to_owned(),
                size: 10,
                preserve: true,
            },
        );
        save_manifest(&dir, &manifest).unwrap();
        let back = load_manifest(&dir).unwrap();
        assert_eq!(back, manifest);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_manifest_errors() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join(MANIFEST_NAME), b"not json {{{").unwrap();
        assert!(load_manifest(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = temp_dir("lock");
        let guard = acquire_lock(&dir).unwrap();
        assert!(acquire_lock(&dir).is_err());
        drop(guard);
        assert!(acquire_lock(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_log_records_only_successful_deletes_and_renames() {
        let dir = temp_dir("audit");
        let plan = Plan {
            actions: vec![
                Action::Delete {
                    path: "gone.flac".to_owned(),
                    clip_id: "g".to_owned(),
                },
                Action::Delete {
                    path: "kept.flac".to_owned(),
                    clip_id: "k".to_owned(),
                },
                Action::Rename {
                    from: "old.flac".to_owned(),
                    to: "new.flac".to_owned(),
                },
                Action::Rename {
                    from: "bad.flac".to_owned(),
                    to: "worse.flac".to_owned(),
                },
            ],
        };
        let failed: HashSet<&str> = ["k", "r2"].into_iter().collect();
        let rename_owner: HashMap<&str, &str> = [("new.flac", "r1"), ("worse.flac", "r2")]
            .into_iter()
            .collect();
        append_audit(&dir, &plan, &failed, &rename_owner).unwrap();
        let log = std::fs::read_to_string(dir.join(AUDIT_NAME)).unwrap();
        assert!(log.contains("DELETE\tg\tgone.flac"));
        assert!(!log.contains("kept.flac"));
        assert!(log.contains("RENAME\t\told.flac\tnew.flac"));
        assert!(!log.contains("worse.flac"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failures_log_records_id_title_url_reason() {
        let dir = temp_dir("failures");
        let clip = Clip {
            id: "x".to_owned(),
            title: "Boom Track".to_owned(),
            audio_url: "https://cdn1.suno.ai/x.mp3".to_owned(),
            ..Default::default()
        };
        let mut clips: HashMap<&str, &Clip> = HashMap::new();
        clips.insert("x", &clip);
        let failures = vec![Failure {
            clip_id: "x".to_owned(),
            reason: "timeout".to_owned(),
        }];
        append_failures(&dir, &failures, &clips).unwrap();
        let log = std::fs::read_to_string(dir.join(FAILURES_NAME)).unwrap();
        assert!(log.contains("\tx\tBoom Track\thttps://cdn1.suno.ai/x.mp3\ttimeout"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_failures_writes_nothing() {
        let dir = temp_dir("nofail");
        append_failures(&dir, &[], &HashMap::new()).unwrap();
        assert!(!dir.join(FAILURES_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_graph_is_empty() {
        let dir = temp_dir("graph-missing");
        let store = load_graph(&dir).unwrap();
        assert!(store.is_empty());
        assert_eq!(store.schema_version, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_graph_roundtrips() {
        let dir = temp_dir("graph-roundtrip");
        let clip = Clip {
            id: "child".to_owned(),
            title: "Cover".to_owned(),
            clip_type: "gen".to_owned(),
            task: "cover".to_owned(),
            cover_clip_id: "root".to_owned(),
            edited_clip_id: "root".to_owned(),
            ..Default::default()
        };
        let mut roots = HashMap::new();
        roots.insert(
            "child".to_owned(),
            RootInfo {
                root_id: "root".to_owned(),
                root_title: "Original".to_owned(),
                status: ResolveStatus::Resolved,
            },
        );
        let resolution = Resolution {
            roots,
            gap_filled: Vec::new(),
        };
        let mut store = LineageStore::new();
        store.update(&[clip], &resolution, "2024-01-01T00:00:00Z");

        save_graph(&dir, &store).unwrap();
        let back = load_graph(&dir).unwrap();
        assert_eq!(back, store);
        assert!(back.node("child").is_some());
        assert_eq!(back.get_root("child").unwrap().root_id, "root");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
