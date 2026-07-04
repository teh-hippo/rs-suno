//! The sync/copy/check engine: resolve targets, list, select, reconcile, gate
//! deletions, execute, and persist.
//!
//! This is the orchestration layer. Every safety-critical decision is delegated
//! to the pure helpers in [`crate::cli::desired`]; this module only sequences
//! the IO around them: which accounts to run, listing through the client,
//! statting the manifest's files, gating deletions, executing the plan (racing
//! a signal so an interrupt preserves partial progress), and writing the
//! manifest, logs, and last-run marker.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::stream::{self, StreamExt};
use suno_core::select::{RecencySpec, SelectParams, select};
use suno_core::{
    AdoptDecision, AlbumArt, AlbumDesired, AlignedLyrics, ArtifactToggles, ClerkAuth, Clip, Config,
    Error as CoreError, ExecOptions, Filesystem, FlagOverrides, LIKED_PLAYLIST_ID, LineageContext,
    LocalFile, Manifest, NamingConfig, Owner, OwnerGate, PlaylistDesired, PlaylistInput,
    PlaylistState, Ports, ResolveOpts, RunStatus, SourceMode, SourceStatus, Stem, SunoClient,
    adopt_decision, album_desired, area_fully_enumerated, build_desired, build_playlist_desired,
    clip_stems, deletion_allowed, is_downloadable, narrows_downloads, owner_gate,
    plan_album_artifacts, plan_playlist_artifacts, playlist_authoritative, reconcile,
    resolve_roots,
};

use crate::cli::args::{GlobalArgs, SyncArgs};
use crate::cli::commands::version;
use crate::cli::desired::{
    Confirm, ExitCode, PlaylistPolicy, ResolvedSelection, build_modes_by_id, confirm_decision,
    confirmed, is_narrowed, mass_delete_abort, resolve_playlist, resolve_selection, run_exit_code,
    worse,
};
use crate::cli::logs;
use crate::cli::output;
use crate::clock::TokioClock;
use crate::download::cleanup_stale_parts;
#[cfg(unix)]
use crate::download::set_permissions_or_remove;
use crate::ffmpeg::FfmpegAdapter;
use crate::fs::FsAdapter;
use crate::http::ReqwestHttp;

const WAV_POLL_ATTEMPTS: u32 = 24;
const WAV_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How many deletion paths the confirmation prompt lists before summarising.
const PROMPT_PATH_LIMIT: usize = 3;
const LAST_RUN_NAME: &str = ".suno-last-run";
#[cfg(unix)]
const PRIVATE_STATE_FILE_MODE: u32 = 0o600;
/// Maximum number of accounts processed concurrently when `--all` targets
/// multiple accounts. Accounts share no mutable state (separate clients,
/// tokens, destination roots, manifests, and lineage files), so per-account
/// isolation is what makes this data-safe: each account's serial commit and
/// deletion-safety logic is entirely unaffected by the concurrency between
/// accounts.
const ACCOUNT_CONCURRENCY: usize = 4;

std::thread_local! {
    /// Per-account stderr buffer. When active (multi-account concurrent path),
    /// `eprint_t!` writes here instead of directly to stderr, so concurrent
    /// accounts' output lines never interleave. Flushed atomically after each
    /// account's thread completes.
    static TASK_STDERR: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// Write a formatted line to the per-account buffer when in a concurrent thread,
/// or directly to stderr for single-account (sequential) runs.
macro_rules! eprint_t {
    ($($arg:tt)*) => {{
        TASK_STDERR.with(|b| {
            let mut guard = b.borrow_mut();
            if let Some(buf) = guard.as_mut() {
                buf.push(format!($($arg)*));
            } else {
                eprintln!($($arg)*);
            }
        });
    }};
}

/// Which verb is running; it sets the source mode and whether the run executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Sync,
    Copy,
    Check,
}

impl Verb {
    fn mode(self) -> SourceMode {
        match self {
            // `check` reports a mirror view so it surfaces would-be deletions.
            Verb::Sync | Verb::Check => SourceMode::Mirror,
            Verb::Copy => SourceMode::Copy,
        }
    }

    fn summary_label(self) -> &'static str {
        match self {
            Verb::Sync => "Sync",
            Verb::Copy => "Copy",
            Verb::Check => "Check",
        }
    }

    fn progress_word(self) -> &'static str {
        match self {
            Verb::Sync => "sync",
            Verb::Copy => "copy",
            Verb::Check => "check",
        }
    }
}

/// Run `sync`.
pub async fn run_sync(global: &GlobalArgs, args: &SyncArgs) -> Result<ExitCode> {
    run(Verb::Sync, global, args, false).await
}

/// Run `copy`.
pub async fn run_copy(global: &GlobalArgs, args: &SyncArgs) -> Result<ExitCode> {
    run(Verb::Copy, global, args, false).await
}

/// Run `check`. `exit_code` makes a pending-change result exit 1.
pub async fn run_check(global: &GlobalArgs, args: &SyncArgs, exit_code: bool) -> Result<ExitCode> {
    run(Verb::Check, global, args, exit_code).await
}

/// One planned run target: an account label and the directory it mirrors into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSpec {
    pub label: String,
    pub dest: PathBuf,
    /// True when there is no configured account; settings come from flags/env.
    pub implicit: bool,
}

/// The inputs that decide which accounts run and where.
#[derive(Debug, Clone, Copy)]
pub struct Selection<'a> {
    pub all: bool,
    pub account: Option<&'a str>,
    pub dest: Option<&'a Path>,
    pub token_available: bool,
}

/// Decide the run targets from config and the selection flags (pure).
///
/// Returns a config-error message string on any ambiguous or impossible
/// selection, which the caller surfaces as exit code 3.
pub fn plan_targets(
    config: Option<&Config>,
    sel: &Selection<'_>,
) -> std::result::Result<Vec<TargetSpec>, String> {
    if sel.all {
        let cfg = config.ok_or("--all needs a config file with at least one account")?;
        if cfg.accounts.is_empty() {
            return Err("--all: no accounts are configured".to_owned());
        }
        if sel.dest.is_some() {
            return Err(
                "--all cannot be combined with a DEST; each account uses its configured root"
                    .to_owned(),
            );
        }
        let mut labels: Vec<&String> = cfg.accounts.keys().collect();
        labels.sort();
        return labels
            .into_iter()
            .map(|label| {
                account_root(cfg, label).map(|dest| TargetSpec {
                    label: label.clone(),
                    dest,
                    implicit: false,
                })
            })
            .collect();
    }

    if let Some(account) = sel.account {
        let cfg = config.ok_or_else(|| format!("account '{account}' not found: no config file"))?;
        if !cfg.accounts.contains_key(account) {
            return Err(unknown_account_message(cfg, account));
        }
        let dest = dest_for(cfg, account, sel.dest)?;
        return Ok(vec![TargetSpec {
            label: account.to_owned(),
            dest,
            implicit: false,
        }]);
    }

    match config {
        Some(cfg) if cfg.accounts.len() == 1 => {
            let label = cfg.accounts.keys().next().expect("one account").clone();
            let dest = dest_for(cfg, &label, sel.dest)?;
            Ok(vec![TargetSpec {
                label,
                dest,
                implicit: false,
            }])
        }
        Some(cfg) if cfg.accounts.len() > 1 => {
            let mut labels: Vec<&str> = cfg.accounts.keys().map(String::as_str).collect();
            labels.sort_unstable();
            Err(format!(
                "multiple accounts configured ({}); pass --account <label> or --all",
                labels.join(", ")
            ))
        }
        _ => {
            if !sel.token_available {
                return Err(
                    "no account configured and no token provided; pass --token, set SUNO_TOKEN_COMMAND, or run 'suno config init'"
                        .to_owned(),
                );
            }
            let dest = sel
                .dest
                .map(Path::to_path_buf)
                .ok_or("a destination directory is required")?;
            Ok(vec![TargetSpec {
                label: "default".to_owned(),
                dest,
                implicit: true,
            }])
        }
    }
}

fn account_root(cfg: &Config, label: &str) -> std::result::Result<PathBuf, String> {
    cfg.accounts
        .get(label)
        .and_then(|acc| acc.root.as_deref())
        .map(PathBuf::from)
        .ok_or_else(|| format!("account '{label}' has no configured root and no DEST was given"))
}

fn dest_for(
    cfg: &Config,
    label: &str,
    dest: Option<&Path>,
) -> std::result::Result<PathBuf, String> {
    if let Some(dest) = dest {
        return Ok(dest.to_path_buf());
    }
    account_root(cfg, label)
}

fn unknown_account_message(cfg: &Config, account: &str) -> String {
    let mut labels: Vec<&str> = cfg.accounts.keys().map(String::as_str).collect();
    labels.sort_unstable();
    if labels.is_empty() {
        format!("account '{account}' not found; no accounts are configured")
    } else {
        format!(
            "account '{account}' not found in config\n\nConfigured accounts: {}",
            labels.join(", ")
        )
    }
}

async fn run(
    verb: Verb,
    global: &GlobalArgs,
    args: &SyncArgs,
    exit_code: bool,
) -> Result<ExitCode> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let token_available = token_available(global, &env);

    let config = match load_config(global.config.as_deref())? {
        ConfigState::Loaded(cfg) => Some(cfg),
        ConfigState::Absent => None,
        ConfigState::Error(message) => {
            eprintln!("error: {message}");
            return Ok(ExitCode::Config);
        }
    };

    let sel = Selection {
        all: global.all,
        account: global.account.as_deref(),
        dest: args.dest.as_deref(),
        token_available,
    };
    let targets = match plan_targets(config.as_ref(), &sel) {
        Ok(targets) => targets,
        Err(message) => {
            eprintln!("error: {message}");
            return Ok(ExitCode::Config);
        }
    };

    let mut worst = ExitCode::Ok;
    if targets.len() <= 1 {
        // Single account: sequential with streaming output (unchanged behaviour).
        let flags = flag_overrides(global, args);
        for target in targets {
            let code = run_one(
                verb,
                global,
                args,
                &target,
                config.as_ref(),
                &flags,
                &env,
                exit_code,
            )
            .await?;
            worst = worse(worst, code);
            if code == ExitCode::Interrupted || code == ExitCode::DiskFull {
                break;
            }
        }
    } else {
        // Multiple accounts: bounded concurrency via OS threads (one per
        // account, each with its own current-thread tokio runtime), with
        // per-account buffered output flushed atomically on completion.
        // Accounts share no mutable state (separate clients, tokens,
        // destination roots, manifests, and lineage files), so per-account
        // isolation is what makes this data-safe — each account's serial
        // commit and deletion-safety logic is entirely unaffected by the
        // concurrency between accounts.
        use std::sync::Arc;
        let global = Arc::new(global.clone());
        let args = Arc::new(args.clone());
        let config = Arc::new(config);
        let env = Arc::new(env);
        let sem = Arc::new(tokio::sync::Semaphore::new(ACCOUNT_CONCURRENCY));
        let mut handles = Vec::new();
        for target in targets {
            let g = Arc::clone(&global);
            let a = Arc::clone(&args);
            let c = Arc::clone(&config);
            let e = Arc::clone(&env);
            // Acquire a slot before spawning; the permit is moved into the
            // thread and released when it exits.
            let permit = Arc::clone(&sem)
                .acquire_owned()
                .await
                .expect("semaphore closed");
            handles.push(std::thread::spawn(move || {
                let _permit = permit;
                TASK_STDERR.with(|b| *b.borrow_mut() = Some(Vec::new()));
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                let flags = flag_overrides(&g, &a);
                let result = rt.block_on(run_one(
                    verb,
                    &g,
                    &a,
                    &target,
                    (*c).as_ref(),
                    &flags,
                    &e,
                    exit_code,
                ));
                let lines = TASK_STDERR.with(|b| b.borrow_mut().take().unwrap_or_default());
                result.map(|code| (code, lines))
            }));
        }
        for handle in handles {
            let (code, lines) = handle
                .join()
                .map_err(|_| anyhow::anyhow!("account thread panicked"))??;
            for line in lines {
                eprintln!("{line}");
            }
            worst = worse(worst, code);
        }
    }
    Ok(worst)
}

enum ConfigState {
    Loaded(Config),
    Absent,
    Error(String),
}

/// Load config, printing a config error and returning its exit code on failure.
pub(crate) fn load_config_reported(
    override_path: Option<&Path>,
) -> std::result::Result<Option<Config>, ExitCode> {
    match load_config(override_path) {
        Ok(ConfigState::Loaded(cfg)) => Ok(Some(cfg)),
        Ok(ConfigState::Absent) => Ok(None),
        Ok(ConfigState::Error(message)) => {
            eprintln!("error: {message}");
            Err(ExitCode::Config)
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            Err(ExitCode::General)
        }
    }
}

/// Resolve a single account's effective settings for the token-only commands
/// (`ls`, `lsjson`, `fetch`, `auth refresh`). Pure given the loaded config.
pub(crate) fn single_account(
    config: Option<&Config>,
    global: &GlobalArgs,
    flags: &FlagOverrides,
    env: &HashMap<String, String>,
) -> std::result::Result<(String, suno_core::EffectiveSettings), String> {
    let token_available = token_available(global, env);
    let (label, implicit) = if global.all {
        return Err(
            "this command runs a single account; pass --account instead of --all".to_owned(),
        );
    } else if let Some(account) = global.account.as_deref() {
        let cfg = config.ok_or_else(|| format!("account '{account}' not found: no config file"))?;
        if !cfg.accounts.contains_key(account) {
            return Err(unknown_account_message(cfg, account));
        }
        (account.to_owned(), false)
    } else {
        match config {
            Some(cfg) if cfg.accounts.len() == 1 => (
                cfg.accounts.keys().next().expect("one account").clone(),
                false,
            ),
            Some(cfg) if cfg.accounts.len() > 1 => {
                let mut labels: Vec<&str> = cfg.accounts.keys().map(String::as_str).collect();
                labels.sort_unstable();
                return Err(format!(
                    "multiple accounts configured ({}); pass --account <label>",
                    labels.join(", ")
                ));
            }
            _ => {
                if !token_available {
                    return Err(
                        "no account configured and no token provided; pass --token or set SUNO_TOKEN_COMMAND"
                            .to_owned()
                    );
                }
                ("default".to_owned(), true)
            }
        }
    };
    let settings = if implicit {
        synthetic_config().resolve("default", None, env, flags)
    } else {
        config
            .expect("non-implicit account has config")
            .resolve(&label, None, env, flags)
    }
    .map_err(|err| err.to_string())?;
    Ok((label, settings))
}

pub(crate) async fn resolve_token(
    label: &str,
    settings: &suno_core::EffectiveSettings,
) -> std::result::Result<Option<String>, String> {
    if let Some(token) = settings.token.clone() {
        return Ok(Some(token));
    }
    if let Some(command) = settings.token_command.as_deref() {
        return run_token_command(label, command).await.map(Some);
    }
    Ok(settings.stored_token.clone())
}

async fn run_token_command(label: &str, command: &str) -> std::result::Result<String, String> {
    let command = command.to_owned();
    // Run the child process off the async runtime so a slow token_command never
    // stalls other tasks; tokio lacks the `process` feature here, so this wraps
    // the blocking `std::process::Command` rather than using `tokio::process`.
    let output = tokio::task::spawn_blocking(move || token_command_process(&command).output())
        .await
        .map_err(|err| format!("token_command for account '{label}' did not complete: {err}"))?
        .map_err(|err| format!("could not run token_command for account '{label}': {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "token_command for account '{label}' failed with {}",
            exit_status_summary(output.status)
        ));
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| {
        format!(
            "token_command for account '{label}' produced non-UTF-8 output; stdout must be UTF-8"
        )
    })?;
    let token = stdout.trim();
    if token.is_empty() {
        return Err(format!(
            "token_command for account '{label}' produced empty output"
        ));
    }
    Ok(token.to_owned())
}

#[cfg(unix)]
fn token_command_process(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(windows)]
fn token_command_process(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(command);
    cmd
}

fn exit_status_summary(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => "termination by signal".to_owned(),
    }
}

fn token_available(global: &GlobalArgs, env: &HashMap<String, String>) -> bool {
    if global.token.is_some()
        || env.contains_key("SUNO_TOKEN")
        || env.contains_key("SUNO_TOKEN_COMMAND")
    {
        return true;
    }
    // A token-only run without a configured account falls back to the implicit
    // `default` account, so also honour that label's per-account env vars (and an
    // explicit --account), matching the resolver's prefix via `label_to_env`.
    let prefix = suno_core::config::label_to_env(global.account.as_deref().unwrap_or("default"));
    env.contains_key(&format!("SUNO_{prefix}_TOKEN"))
        || env.contains_key(&format!("SUNO_{prefix}_TOKEN_COMMAND"))
}

/// Load config from the override or platform default. A missing default file is
/// `Absent`; a missing explicit `--config`, or a parse error, is an error.
fn load_config(override_path: Option<&Path>) -> Result<ConfigState> {
    let explicit = override_path.is_some();
    let Some(path) = logs::config_path(override_path) else {
        return Ok(ConfigState::Absent);
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match Config::from_toml(&text) {
            Ok(cfg) => Ok(ConfigState::Loaded(cfg)),
            Err(err) => Ok(ConfigState::Error(format!("{}: {err}", path.display()))),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if explicit {
                Ok(ConfigState::Error(format!(
                    "config file not found: {}",
                    path.display()
                )))
            } else {
                Ok(ConfigState::Absent)
            }
        }
        Err(err) => Err(err).with_context(|| format!("could not read {}", path.display())),
    }
}

fn flag_overrides(global: &GlobalArgs, args: &SyncArgs) -> FlagOverrides {
    FlagOverrides {
        token: global.token.clone(),
        format: args.format.map(Into::into),
        concurrency: args.concurrency,
        retries: args.retries,
        min_newest: args.min_newest,
        // A presence-only toggle can only enable; absence defers to config/env.
        animated_covers: args.animated_covers.then_some(true),
        video_cover_retention: args.video_cover_retention.map(Into::into),
        animated_cover_quality: args.animated_cover_quality,
        animated_cover_max_fps: args.animated_cover_max_fps,
        animated_cover_max_width: args.animated_cover_max_width,
        animated_cover_compression_level: args.animated_cover_compression_level,
        details_sidecar: args.details_sidecar.then_some(true),
        lyrics_sidecar: args.lyrics_sidecar.then_some(true),
        lrc_sidecar: args.lrc_sidecar.then_some(true),
        video_mp4: args.video_mp4.then_some(true),
        download_stems: args.download_stems.then_some(true),
        stem_format: args.stem_format.map(Into::into),
        naming_template: args.naming_template.clone(),
        character_set: args.character_set.map(Into::into),
    }
}

/// Strip the resolved format's extension from an audio path, giving the
/// extensionless base the sidecars and the `.stems` folder are built from.
/// Falls back to the whole path if the extension is somehow absent.
fn strip_format_ext(path: &str, format: suno_core::AudioFormat) -> &str {
    path.strip_suffix(&format!(".{format}")).unwrap_or(path)
}

/// List existing stems for the selected clips, when the feature is on.
///
/// Read-only and free: it pages the stems listing (`GET`) and NEVER generates or
/// spends credits. Returns a map from clip id to its AUTHORITATIVE stem set. A
/// clip is present ONLY when its listing fully enumerated at least one stem; a
/// clip absent from the map (feature off, `has_stem` false, or an
/// indeterminate/failed/partial/`400` listing) means "keep existing local
/// stems", so this can never drive a stem deletion. `has_stem` is the
/// precondition, so a clip Suno reports as stemless is never even queried.
async fn list_existing_stems(
    enabled: bool,
    clips: &[&Clip],
    client: &SunoClient<TokioClock>,
    http: &ReqwestHttp,
    concurrency: u32,
) -> HashMap<String, Vec<Stem>> {
    let mut out = HashMap::new();
    if !enabled {
        return out;
    }
    let candidates: Vec<&Clip> = clips.iter().copied().filter(|clip| clip.has_stem).collect();
    let fetched = stream::iter(candidates)
        .map(|clip| async move {
            (
                clip.id.clone(),
                client.list_stems(http, &clip.id).await.ok(),
            )
        })
        .buffered(concurrency.max(1) as usize)
        .collect::<Vec<_>>()
        .await;
    for (id, result) in fetched {
        if let Some((stems, true)) = result
            && !stems.is_empty()
        {
            out.insert(id, stems);
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn run_one(
    verb: Verb,
    global: &GlobalArgs,
    args: &SyncArgs,
    target: &TargetSpec,
    config: Option<&Config>,
    flags: &FlagOverrides,
    env: &HashMap<String, String>,
    exit_code: bool,
) -> Result<ExitCode> {
    let verbosity = global.verbosity();

    // Re-pinning is a destructive-intent override that only makes sense on an
    // executing run: check and dry-run never persist a pin, so accepting the
    // flag there would print a re-pin that silently never happens.
    if args.allow_account_change && (global.dry_run || verb == Verb::Check) {
        eprint_t!(
            "error: --allow-account-change only applies to an executing sync or copy, not check or --dry-run."
        );
        return Ok(ExitCode::Usage);
    }

    let settings = {
        let resolved = if target.implicit {
            synthetic_config().resolve("default", None, env, flags)
        } else {
            config
                .expect("non-implicit target has config")
                .resolve(&target.label, None, env, flags)
        };
        match resolved {
            Ok(settings) => settings,
            Err(err) => {
                eprint_t!("error: {err}");
                return Ok(ExitCode::Config);
            }
        }
    };

    let token = match resolve_token(&target.label, &settings).await {
        Ok(Some(token)) => token,
        Ok(None) => {
            eprint_t!(
                "error: no token for account '{}'; pass --token, set SUNO_TOKEN or SUNO_TOKEN_COMMAND, or set token/token_command in config",
                target.label
            );
            return Ok(ExitCode::Config);
        }
        Err(err) => {
            eprint_t!("error: {err}");
            return Ok(ExitCode::Config);
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
        return Ok(ExitCode::Config);
    }

    let http = ReqwestHttp::new().context("failed to build the HTTP client")?;
    let dest = &target.dest;
    let auth = ClerkAuth::new(&token);
    if let Err(err) = auth.authenticate(&http).await {
        return Ok(report_auth_failure(&target.label, &err));
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
        return Ok(ExitCode::Auth);
    };

    // Load the durable store up front so the identity guard can compare the
    // authenticated account against the account this library is pinned to,
    // before a single feed request is made (PHASE 1, below). A mismatch aborts
    // here so a swapped or mistyped token can never make another account's
    // clips look absent from source and delete this library's files.
    let mut store = logs::load_graph(dest)?;
    // Derive the eligible-root set from the loaded cache so overrides and
    // collision detection are correct even on a resolution-failed run (where
    // `store.update` is skipped below); a successful run refreshes it again.
    store.refresh_eligible_roots();
    // Layer this account's manual album-name overrides onto the store before any
    // album title is resolved, so the folder path, ALBUM tag, change hash, index
    // and disambiguation all reflect the preferred name from one source.
    store.set_album_overrides(settings.album_overrides.clone());
    let mut owner_dirty = false;
    // A pin/adopt/re-pin that actually happens this run: its notice is printed
    // and its audit line written only on the executing path, where the pin is
    // persisted (F1: check/dry-run must not claim a pin they never save).
    let mut pending_pin: Option<PendingPin> = None;

    // PHASE 1: decide identity with no network via the pure gate, then apply
    // the side-effects (pin, refresh, abort) here.
    let gate = owner_gate(
        store.owner(),
        settings.account_id.as_deref(),
        &user_id,
        args.allow_account_change,
    );
    let mut force_additive = gate.is_additive();
    match gate {
        OwnerGate::AbortConfigMismatch => {
            eprint_t!(
                "error: the configured account_id ({}) does not match the authenticated account (id {}). Refusing to run to protect the library.",
                short_id(settings.account_id.as_deref().unwrap_or_default()),
                short_id(&user_id)
            );
            return Ok(ExitCode::Safety);
        }
        OwnerGate::AbortMismatch => {
            let pinned = store.owner().expect("mismatch implies a pinned owner");
            eprint_t!(
                "error: this library belongs to {} (id {}) but the token authenticates as {} (id {}). Refusing to run to protect the library. Pass --allow-account-change to re-pin it to the authenticated account, or use a different destination.",
                pinned.display_name,
                short_id(&pinned.user_id),
                account,
                short_id(&user_id)
            );
            return Ok(ExitCode::Safety);
        }
        OwnerGate::Repin => {
            let previous = store
                .owner()
                .map(|owner| owner.display_name.clone())
                .unwrap_or_default();
            set_pin(
                &mut store,
                &mut owner_dirty,
                &mut pending_pin,
                &user_id,
                &account,
                "REPIN",
                format!(
                    "notice: re-pinned this library from {} to {} (id {}); this run is additive (no deletions). Run 'sync' again to mirror.",
                    previous,
                    account,
                    short_id(&user_id)
                ),
            );
        }
        OwnerGate::Proceed => {
            if store.refresh_display_name(&account) {
                owner_dirty = true;
            }
            if args.allow_account_change && verbosity >= 0 {
                eprint_t!(
                    "notice: --allow-account-change had no effect; this library already belongs to {} (id {}).",
                    account,
                    short_id(&user_id)
                );
            }
        }
        OwnerGate::FirstUse => {}
    }

    let client = SunoClient::new(auth, TokioClock);

    // Resolve which areas this run touches and their modes (pure). CLI scope
    // flags win over `[areas]` config; a copy verb or a force-additive run
    // rewrites every mode to Copy. When any Mirror area is armed and the library
    // is neither explicitly selected nor `"off"`, an implicit full-library copy
    // protector is injected so a Mirror area can never delete a library-exclusive
    // file (D1).
    let force_copy_initial = verb == Verb::Copy || force_additive;
    let selection = resolve_selection(
        verb.mode(),
        args.mode.map(SourceMode::from),
        args.liked,
        &args.playlist,
        settings.areas.as_ref(),
        force_copy_initial,
    );

    // List every area (IO). A failed secondary area contributes a
    // non-enumerated, empty source (never aborting, never vanishing) so one
    // failure suppresses all deletion while successful areas still download; an
    // unresolvable explicit `--playlist X` typo keeps today's hard failure.
    let areas = match enumerate_areas(
        &selection,
        &client,
        &http,
        &target.label,
        args,
        verbosity,
        settings.concurrency,
    )
    .await
    {
        Ok(areas) => areas,
        Err(code) => return Ok(code),
    };

    // Build the clip union in canonical area order (Library > Liked > Playlist),
    // keeping the first area's payload per id so the Library variant wins (H1).
    let clips = union_clips(&areas);

    // A purely scoped run that resolved to nothing downloadable is a no-op: keep
    // today's notice rather than fall through to an empty plan.
    if clips.is_empty() && selection.library.is_none() {
        if verbosity >= -1 {
            eprint_t!("notice: nothing to do; the requested scope holds no downloadable clips.");
        }
        return Ok(ExitCode::Ok);
    }

    // Resolve every listed clip's root ancestor (roots need the whole set as
    // the universe). Resolution is best-effort: a hard IO failure degrades to
    // the last-known-good roots already in the durable store rather than
    // aborting the sync or rewriting the library from a dropped call (H3).
    //
    // Seed the resolver with the store's persisted parent links so a walk can
    // hop through an ancestor whose clip is absent this run (an intermediate
    // remix, or one Suno has purged) using data captured earlier, instead of
    // self-rooting into a duplicate album. Read before `store.update` so it
    // reflects the prior run's archive.
    let archived_parents = store.archived_parents();
    let resolution = match resolve_roots(
        &clips,
        &archived_parents,
        &client,
        &http,
        ResolveOpts {
            concurrency: settings.concurrency,
            ..ResolveOpts::default()
        },
    )
    .await
    {
        Ok(resolution) => Some(resolution),
        Err(err) => {
            if verbosity >= -1 {
                eprint_t!(
                    "warning: lineage resolution failed ({err}); using the last-known-good graph"
                );
            }
            None
        }
    };

    // The durable lineage graph is the single source of truth for every
    // file-affecting decision (album folders, embedded tags, the change hash).
    // Deriving those from the live per-run resolution instead would let one
    // dropped resolution call rename and retag the whole library (HARDENING
    // H3), so fold in this run's resolution only when it succeeded (a monotonic
    // upsert that never downgrades a known root), and build every context from
    // the store. When resolution failed the store is used untouched, so prior
    // albums hold and nothing is rewritten.
    let graph_changed = resolution.is_some();
    if let Some(resolution) = &resolution {
        store.update(&clips, resolution, &now_rfc3339());
    }
    let colliding_albums = store.colliding_root_titles();
    // Preliminary authority for the first-use adoption check, computed before
    // any adoption can flip the run additive. A run that could delete (any
    // fully-enumerated Mirror source, e.g. a playlist under `library="off"`)
    // must confirm identity here too: otherwise it SkipPins and then deletes
    // against an account this library was never pinned to, the exact hole the
    // owner pin closes (#149).
    let enumerated = adoption_enumerated(&areas, force_copy_initial);

    // PHASE 2: first-use adoption, now the listing is known. Only a library
    // that PHASE 1 left unpinned (FirstUse) reaches here; identity is confirmed
    // from the overlap between this account's listing and the clips already
    // owned. The manifest is read before the lock deliberately: only the
    // empty-vs-non-empty and overlap facts matter, so a concurrent write cannot
    // flip the decision unsafely.
    if gate == OwnerGate::FirstUse {
        let owned = logs::load_manifest(dest)?;
        let owned_ids: BTreeSet<&str> = owned.entries.keys().map(String::as_str).collect();
        let listed_ids: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
        let decision = adopt_decision(
            &listed_ids,
            &owned_ids,
            enumerated,
            args.allow_account_change,
        );
        force_additive = force_additive || decision.is_additive();
        match decision {
            AdoptDecision::PinFresh => {
                set_pin(
                    &mut store,
                    &mut owner_dirty,
                    &mut pending_pin,
                    &user_id,
                    &account,
                    "PIN",
                    format!(
                        "notice: pinned this library to {} (id {}).",
                        account,
                        short_id(&user_id)
                    ),
                );
            }
            AdoptDecision::PinAdopt => {
                set_pin(
                    &mut store,
                    &mut owner_dirty,
                    &mut pending_pin,
                    &user_id,
                    &account,
                    "ADOPT",
                    format!(
                        "notice: adopted this existing library for {} (id {}).",
                        account,
                        short_id(&user_id)
                    ),
                );
            }
            AdoptDecision::AdoptForced => {
                set_pin(
                    &mut store,
                    &mut owner_dirty,
                    &mut pending_pin,
                    &user_id,
                    &account,
                    "ADOPT",
                    format!(
                        "notice: adopted this library for {} (id {}) despite no overlap; this run is additive (no deletions). Run 'sync' again to mirror.",
                        account,
                        short_id(&user_id)
                    ),
                );
            }
            AdoptDecision::Abort => {
                eprint_t!(
                    "error: none of the authenticated account's clips ({}, id {}) match this library at {}. Refusing to run in case the token authenticates as a different Suno account. Pass --allow-account-change to adopt it, or use a different destination.",
                    account,
                    short_id(&user_id),
                    dest.display()
                );
                return Ok(ExitCode::Safety);
            }
            AdoptDecision::SkipPin => {}
        }
    }

    // Assemble the final per-area view now the run's additivity is known. A copy
    // verb or a force-additive run (re-pin/adopt) rewrites every area to Copy, so
    // no Mirror source remains and deletion is impossible; the protector already
    // never armed anything.
    let force_copy = verb == Verb::Copy || force_additive;
    let sources = source_statuses(&areas, force_copy);
    let can_delete = deletion_allowed(&sources);
    // Art, `.m3u8`, and the library index are gated on an authoritative Library:
    // a Library area present in the selection (the implicit protector counts;
    // `library="off"` does not) that fully enumerated.
    let library_authoritative = library_authoritative(&areas, force_copy);

    // Every clip's modes across the areas holding it, so each Desired carries the
    // Copy protection of any Copy area even when a Mirror area also holds it
    // (SYNC-8).
    let area_modes: Vec<(SourceMode, Vec<String>)> = areas
        .iter()
        .map(|area| {
            (
                area_mode(area, force_copy),
                area.clips.iter().map(|clip| clip.id.clone()).collect(),
            )
        })
        .collect();
    let modes_by_id = build_modes_by_id(&area_modes);

    let since = match args.since.as_deref().map(RecencySpec::parse).transpose() {
        Ok(since) => since,
        Err(message) => {
            eprint_t!("error: {message}");
            return Ok(ExitCode::Config);
        }
    };
    // `--limit`/`--since` narrow the selection only on a run that cannot delete
    // and has no authoritative library: truncating the union on an armed or
    // protected run would drop a Mirror/protector clip from `desired` and turn it
    // into a deletion candidate, so a stray `--limit` never disarms a mirror (D2).
    let truncate = narrows_downloads(can_delete, library_authoritative);
    // When a full authoritative library is listed (the injected protector, or a
    // configured unfiltered `library="mirror"`), `--limit`/`--since` are inert:
    // the whole feed is listed regardless, so say so once rather than silently
    // ignoring them (#148). The most surprising case is the configured mirror,
    // where the flags are inert yet deletions still apply.
    if is_narrowed(args.limit, args.since.as_deref()) && !truncate && verbosity >= -1 {
        let deletes = if can_delete {
            "; the library is still mirrored and deletions apply"
        } else {
            ""
        };
        eprint_t!(
            "note: --limit/--since do not narrow this run (an authoritative full library is listed){deletes}"
        );
    }
    let params = SelectParams {
        limit: if truncate { args.limit } else { None },
        since: if truncate { since } else { None },
        min_newest: settings.min_newest as usize,
        now: now_secs(),
        last_run: read_last_run(dest),
    };
    let selected = select(&clips, &params);
    let contexts: HashMap<String, LineageContext> = selected
        .iter()
        .map(|clip| (clip.id.clone(), store.context_for(clip)))
        .collect();
    let mut desired = build_desired(
        &selected,
        settings.format,
        &modes_by_id,
        &contexts,
        &colliding_albums,
        ArtifactToggles {
            animated_covers: settings.animated_covers,
            details: settings.details_sidecar,
            lyrics: settings.lyrics_sidecar,
            lrc: settings.lrc_sidecar,
            video: settings.video_mp4,
        },
        &NamingConfig {
            template: settings.naming_template.clone(),
            character_set: settings.character_set,
            ..NamingConfig::default()
        },
    );
    // Stems (#100): existing stems are a per-clip keyed set that needs a network
    // listing (free, read-only), so they are threaded in after the pure
    // `build_desired`. Off by default; the listing only touches clips whose
    // `has_stem` is true, and only an authoritative set drives removals — an
    // absent/indeterminate listing leaves a clip's `stems` at `None` so existing
    // local stems are kept. This path never generates or spends credits.
    let stems_by_id = list_existing_stems(
        settings.download_stems,
        &selected,
        &client,
        &http,
        settings.concurrency,
    )
    .await;
    if settings.download_stems {
        for d in &mut desired {
            let base = strip_format_ext(&d.path, settings.format);
            d.stems = stems_by_id
                .get(&d.clip.id)
                .map(|stems| clip_stems(base, stems, settings.stem_format, settings.character_set));
        }
    }
    // Folder-level album art is keyed on the stable root id and chosen purely
    // from the selected clips. Without an authoritative Library the folder view
    // is partial, so leave folder art entirely untouched (no rewrites, no
    // deletes) by handing the planner an empty desired set.
    let albums_desired = if library_authoritative {
        album_desired(
            &desired,
            settings.animated_covers,
            settings.raw_animated_cover,
        )
    } else {
        Vec::new()
    };

    // Playlists (.m3u8). Only the classic plain-library run walks every account
    // playlist and maintains them all exactly as today (the full member sets are
    // knowable). Every scoped or `[areas]` run -- including one carrying an
    // injected copy-protector, which makes the Library authoritative for audio
    // deletion but selects no playlists -- maintains only the playlist areas it
    // selected and fully enumerated, and protects every other id so no `.m3u8`
    // is rewritten or deleted from a partial view (B2/D3).
    let mut protected_playlists: BTreeSet<String> = BTreeSet::new();
    let (playlist_desired, playlists_enumerated) =
        if selection.is_plain_library() && library_authoritative {
            fetch_playlist_desired(
                &client,
                &http,
                &desired,
                &mut protected_playlists,
                verbosity,
                settings.concurrency,
            )
            .await
        } else {
            build_scoped_playlist_desired(
                &areas,
                &desired,
                &store,
                &mut protected_playlists,
                force_copy,
                !truncate,
            )
        };
    // The stored view handed to the planner drops protected ids, so a playlist
    // whose members could not be fetched is never treated as stale (B2).
    let stored_playlists: BTreeMap<String, PlaylistState> = store
        .playlists
        .iter()
        .filter(|(id, _)| !protected_playlists.contains(id.as_str()))
        .map(|(id, state)| (id.clone(), state.clone()))
        .collect();

    let dry_run = global.dry_run || verb == Verb::Check;

    // Dry-run and check report without touching disk: the destination is not
    // created and no lock is taken. A missing manifest reads as empty. The synced
    // `.lrc` preview reflects which clips would be (re)fetched and written,
    // without any network fetch.
    if dry_run {
        let manifest = logs::load_manifest(dest)?;
        suno_core::preview_synced_lrc(&mut desired, &manifest, now_secs(), settings.lrc_sidecar);
        let plan = reconcile_run(
            &manifest,
            dest,
            &desired,
            &albums_desired,
            &store.albums,
            &playlist_desired,
            &stored_playlists,
            &sources,
            library_authoritative,
            playlists_enumerated,
        )
        .await;
        if verbosity >= 1 {
            let no_failures = HashSet::new();
            for line in output::action_lines(&plan, &no_failures, verbosity) {
                eprint_t!("{line}");
            }
        }
        if verbosity >= -1 {
            eprint_t!("{}", output::dry_summary(&account, &plan));
            // Read-only orphan report: audio files on disk that no manifest entry
            // tracks (moved or renamed by hand, or left from an older layout).
            // Listed only, never matched to a clip, renamed, or deleted (#146).
            let orphans = suno_core::untracked_audio(&manifest, &walk_audio_files(dest));
            if !orphans.is_empty() {
                eprint_t!("{}", output::orphan_report(&orphans));
            }
        }
        if verb == Verb::Check && exit_code && plan_has_changes(&plan) {
            return Ok(ExitCode::General);
        }
        return Ok(ExitCode::Ok);
    }

    // The executing run creates the destination, then takes the lock *before*
    // loading the manifest so a concurrent run cannot plan against it and then
    // execute a stale plan over the other run's writes. The lock lives to the
    // end of the function, covering reconcile, the confirmation prompt, and
    // execute.
    std::fs::create_dir_all(dest)
        .with_context(|| format!("could not create {}", dest.display()))?;
    let _lock = logs::acquire_lock(dest)?;
    let manifest = logs::load_manifest(dest)?;
    // Resolve this run's synced lyrics before reconcile: fetch Suno's alignment
    // for the clips that need it (gated by the per-clip marker, so a steady-state
    // re-sync fetches nothing and the feature being off fetches nothing), and
    // fill each clip's `.lrc` artifact with its content-hashed body. Reconcile
    // then plans the `.lrc` writes from the ACTUAL body, and the executor embeds
    // the same alignment as MP3 `SYLT`/plain-lyric tags.
    let (synced, pending_checks) = resolve_synced_lyrics(
        &mut desired,
        &manifest,
        &client,
        &http,
        settings.lrc_sidecar,
        verbosity,
        settings.concurrency,
    )
    .await;
    let plan = reconcile_run(
        &manifest,
        dest,
        &desired,
        &albums_desired,
        &store.albums,
        &playlist_desired,
        &stored_playlists,
        &sources,
        library_authoritative,
        playlists_enumerated,
    )
    .await;

    // Persist the lineage graph *before* execute (durability H4), under the same
    // lock as the manifest. This run refreshed it when it folded in a fresh
    // resolution (`graph_changed`) or when the identity guard pinned or updated
    // the owner (`owner_dirty`); an owner-only change must persist even when
    // resolution failed, so a first-use adoption is durable.
    if graph_changed || owner_dirty {
        logs::save_graph(dest, &store)?;
    }
    // Announce and audit an actual pin only now, on the executing path, so a
    // notice is never printed for a pin that check/dry-run would not persist
    // (F1). The full id goes to the audit file, never to stderr.
    if let Some(pin) = &pending_pin {
        if verbosity >= -1 {
            eprint_t!("{}", pin.notice);
        }
        if let Some(owner) = store.owner() {
            logs::append_owner_pin(dest, pin.action, &owner.user_id, &owner.display_name)?;
        }
    }

    let is_sync = verb == Verb::Sync && !force_additive;
    // The mass-delete cap counts every destructive action, audio and sidecar
    // alike (HARDENING B2), so a run that would mass-delete artifacts aborts too.
    let delete_count = plan.deletes() + plan.artifact_deletes() + plan.stem_deletes();
    if is_sync
        && mass_delete_abort(
            desired.len(),
            manifest.len(),
            delete_count,
            settings.min_newest,
            args.min_newest == Some(0),
            global.yes,
        )
    {
        eprint_t!(
            "error: sync aborted -- deletion safety rule triggered\n\nThe listing yielded {} clip(s), which would delete {} of {} local file(s).\nThis is almost certainly a listing error. No files were deleted.\n\nIf you intended to delete everything, pass --min-newest 0 --yes to confirm.",
            desired.len(),
            delete_count,
            manifest.len()
        );
        return Ok(ExitCode::Safety);
    }

    match confirm_decision(
        is_sync,
        delete_count,
        global.yes,
        std::io::stdin().is_terminal(),
    ) {
        Confirm::Proceed => {}
        Confirm::Prompt => {
            if !prompt_delete(&plan, verbosity)? {
                eprint_t!("Aborted; no changes made.");
                return Ok(ExitCode::Ok);
            }
        }
        Confirm::RefuseNonInteractive => {
            eprint_t!(
                "error: sync would delete {} file(s) but stdin is not a TTY and --yes was not passed\n  Pass --yes to confirm, or use 'copy' to skip deletions.",
                delete_count
            );
            return Ok(ExitCode::Safety);
        }
    }

    if verbosity == 0 {
        eprint_t!(
            "{}",
            output::progress_start(verb.progress_word(), &account, &plan)
        );
    }

    execute_plan(
        verb,
        plan,
        &desired,
        manifest,
        synced,
        pending_checks,
        &mut store,
        &client,
        &http,
        dest,
        &settings,
        &account,
        verbosity,
        library_authoritative,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_plan(
    verb: Verb,
    plan: suno_core::Plan,
    desired: &[suno_core::Desired],
    mut manifest: suno_core::Manifest,
    synced: HashMap<String, AlignedLyrics>,
    pending_checks: Vec<suno_core::PendingCheck>,
    store: &mut suno_core::LineageStore,
    client: &SunoClient<TokioClock>,
    http: &ReqwestHttp,
    dest: &Path,
    settings: &suno_core::EffectiveSettings,
    account: &str,
    verbosity: i8,
    library_authoritative: bool,
) -> Result<ExitCode> {
    cleanup_stale_parts(dest);
    let fs = FsAdapter::new(dest);
    let ffmpeg = FfmpegAdapter::new(dest);
    let clock = TokioClock;
    let opts = ExecOptions {
        max_retries: settings.retries,
        wav_poll_attempts: WAV_POLL_ATTEMPTS,
        wav_poll_interval: WAV_POLL_INTERVAL,
        concurrency: settings.concurrency,
        cover_webp: settings.animated_cover_webp,
    };
    let started = std::time::Instant::now();

    let outcome = {
        let ports = Ports {
            client,
            http,
            fs: &fs,
            ffmpeg: &ffmpeg,
            clock: &clock,
        };
        tokio::select! {
            out = suno_core::execute(&plan, &mut manifest, &mut store.albums, &mut store.playlists, desired, &synced, ports, &opts) => Some(out),
            _ = wait_for_signal() => None,
        }
    };

    let Some(outcome) = outcome else {
        logs::save_manifest(dest, &manifest)?;
        // Folder art may have been written before the interrupt; persist the
        // album-art store so those sidecars are tracked on the next run.
        logs::save_graph(dest, store)?;
        // A signal cancels the executor mid-flight, before its own end-of-run
        // prune; tidy any directories emptied by moves/deletes so far. The
        // completed path is already pruned inside `execute`.
        let _ = fs.prune_empty_dirs("");
        eprint_t!(
            "warning: interrupted -- partial run saved\n  Progress so far is recorded in the manifest; re-run to continue."
        );
        return Ok(ExitCode::Interrupted);
    };

    if outcome.status == RunStatus::DiskFull {
        // A full disk aborts the run; persistence would only re-hit ENOSPC, so
        // save best-effort (mirroring the interrupt path) and stop before the
        // `?`-propagating summary writes below. The summary and hint are
        // eprintln-only, so they never re-hit the full disk.
        let _ = logs::save_manifest(dest, &manifest);
        let _ = logs::save_graph(dest, store);
        let _ = fs.prune_empty_dirs("");
        // The counter block honours quiet mode, but the actionable error and its
        // specific reason always print (even under `-qq`), matching main.rs.
        if verbosity >= -1 {
            eprint_t!(
                "{}",
                output::run_summary(
                    verb.summary_label(),
                    account,
                    &outcome,
                    started.elapsed().as_secs_f64()
                )
            );
        }
        eprint_t!(
            "error: {} The library is unchanged for the failing action.",
            crate::diskspace::DISK_FULL_HINT
        );
        if let Some(last) = outcome.failures.last() {
            eprint_t!("  {}", last.reason);
        }
        return Ok(ExitCode::DiskFull);
    }

    // Record the synced-lyrics resolution markers now the writes have landed:
    // an instrumental is marked so it is not re-fetched every run, and a written
    // clip is marked only once its `.lrc` slot reflects the body (so an
    // interrupted or failed write is re-resolved next run rather than skipped).
    record_synced_lyrics_checks(&mut manifest, &pending_checks);

    logs::save_manifest(dest, &manifest)?;
    // Persist the graph again after execute: the lineage part was already saved
    // for durability before execute, but album-art state is mutated *during*
    // execute (folder.jpg / cover.webp writes and deletes), so it lands now.
    logs::save_graph(dest, store)?;
    let clips_by_id: HashMap<&str, &Clip> = desired
        .iter()
        .map(|d| (d.clip.id.as_str(), &d.clip))
        .collect();
    // Best-effort library index: a regenerable scripting artefact, so a failure
    // to write it must never fail an otherwise-green mirror (unlike the
    // manifest). Gated on an authoritative Library (D4), not playlist membership:
    // a narrowed `--limit`/`--since` or area-only run sees only a window of clips
    // live, so it would null the artist/tags/duration of every out-of-window clip
    // and regress a richer index from a prior full run; only an authoritative
    // Library run writes, avoiding that live-field oscillation.
    if library_authoritative
        && let Err(err) = logs::save_index(dest, &manifest, store, &clips_by_id)
        && verbosity >= -1
    {
        eprint_t!("warning: could not write {}: {err}", logs::INDEX_NAME);
    }
    logs::append_failures(dest, &outcome.failures, &clips_by_id)?;
    let failed: HashSet<&str> = outcome
        .failures
        .iter()
        .map(|f| f.clip_id.as_str())
        .collect();
    let rename_owner: HashMap<&str, &str> = desired
        .iter()
        .map(|d| (d.path.as_str(), d.clip.id.as_str()))
        .collect();
    logs::append_audit(dest, &plan, &failed, &rename_owner)?;
    write_last_run(dest);

    if verbosity >= 1 {
        for line in output::action_lines(&plan, &failed, verbosity) {
            eprint_t!("{line}");
        }
    }

    if !outcome.failures.is_empty() && verbosity >= -1 {
        eprint_t!(
            "warning: {} clip(s) failed after retries\n  See {} for details.",
            outcome.failures.len(),
            dest.join(".suno-failures.log").display()
        );
    }
    if verbosity >= -1 {
        eprint_t!(
            "{}",
            output::run_summary(
                verb.summary_label(),
                account,
                &outcome,
                started.elapsed().as_secs_f64()
            )
        );
    }

    Ok(run_exit_code(&outcome))
}

/// The warning shown when a clip's alignment fetch fails. Deliberately carries
/// NO clip id, request URL, or error detail: a reqwest transport error's text
/// can include the full `/api/gen/{id}/...` URL, so the raw error is never
/// interpolated into any message (the clip id must not leak).
const SYNCED_LYRICS_FETCH_WARNING: &str = "could not fetch synced lyrics for a clip; its synced lyrics are skipped this run and retried next run";

/// Resolve this run's synced lyrics: fetch Suno's word/line alignment for the
/// clips that need it, fill each clip's `.lrc` artifact with its content-hashed
/// body, and return the per-clip alignment (for the executor's `SYLT`/plain
/// tags) plus the resolution checks to record after the writes land.
///
/// The pure [`synced_lyrics_targets`](suno_core::synced_lyrics_targets) decides
/// which clips to fetch (empty when the feature is off, and skipping clips
/// already resolved at this render version), and [`apply_synced_lrc`](suno_core::apply_synced_lrc)
/// maps each result onto the desired artifact; this function is only the IO glue.
/// A fetch failure keeps the clip's existing `.lrc`/tags untouched (no downgrade)
/// and is retried next run; its warning never prints the clip id, URL, or token.
async fn resolve_synced_lyrics(
    desired: &mut [suno_core::Desired],
    manifest: &Manifest,
    client: &SunoClient<TokioClock>,
    http: &ReqwestHttp,
    enabled: bool,
    verbosity: i8,
    concurrency: u32,
) -> (HashMap<String, AlignedLyrics>, Vec<suno_core::PendingCheck>) {
    let mut synced: HashMap<String, AlignedLyrics> = HashMap::new();
    let targets = suno_core::synced_lyrics_targets(desired, manifest, now_secs(), enabled);
    let fetched = stream::iter(targets.iter())
        .map(|id| async move { (id.clone(), client.aligned_lyrics(http, id).await) })
        .buffered(concurrency.max(1) as usize)
        .collect::<Vec<_>>()
        .await;
    for (id, result) in fetched {
        match result {
            Ok(aligned) => {
                synced.insert(id, aligned);
            }
            Err(_) => {
                if verbosity >= -1 {
                    eprint_t!("warning: {SYNCED_LYRICS_FETCH_WARNING}");
                }
            }
        }
    }
    let pending = suno_core::apply_synced_lrc(desired, manifest, &synced);
    (synced, pending)
}

/// Record the synced-lyrics resolution markers after this run's `.lrc` writes.
///
/// An instrumental (empty) clip is marked unconditionally so it is not re-fetched
/// every run; a clip that produced a body is marked only once its `.lrc` slot
/// reflects that body's hash, so an interrupted or failed write leaves no marker
/// and is re-resolved next run rather than skipped.
fn record_synced_lyrics_checks(manifest: &mut Manifest, pending: &[suno_core::PendingCheck]) {
    let now = now_secs();
    for check in pending {
        let durable = if check.empty {
            true
        } else {
            match (&check.body_hash, manifest.get(&check.clip_id)) {
                (Some(hash), Some(entry)) => {
                    entry.lrc.as_ref().map(|slot| &slot.hash) == Some(hash)
                }
                _ => false,
            }
        };
        if !durable {
            continue;
        }
        if let Some(entry) = manifest.entries.get_mut(&check.clip_id) {
            entry.synced_lyrics = Some(suno_core::SyncedLyricsCheck {
                version: suno_core::SYNCED_LRC_VERSION,
                checked_unix: now,
                empty: check.empty,
                timed: check.timed,
            });
        }
    }
}

/// One area's listing outcome for the multi-area planner.
///
/// The `authoritative_ignoring_empty` flag is the area's completeness verdict
/// *before* the empty-mirror guard (§5), which [`area_enumerated`] applies later
/// against the final mode, so a copy-verb override that turns a Mirror area Copy
/// re-scores an empty area correctly.
struct AreaListing {
    kind: AreaKind,
    /// The resolved (pre copy-override) mode for this area.
    mode: SourceMode,
    /// The area's downloadable clips.
    clips: Vec<Clip>,
    /// Completeness modulo the empty-mirror guard: `true` when the listing
    /// drained, was not deliberately narrowed, and lost no member to the
    /// downloadable filter.
    authoritative_ignoring_empty: bool,
}

/// Which kind of area a listing came from, carrying playlist identity so its
/// `.m3u8` can be maintained by id and name.
enum AreaKind {
    Library,
    Liked,
    Playlist { id: String, name: String },
}

/// This area's mode after the copy-verb / force-additive override.
fn area_mode(area: &AreaListing, force_copy: bool) -> SourceMode {
    if force_copy {
        SourceMode::Copy
    } else {
        area.mode
    }
}

/// Whether this area is authoritative for deletion, applying the empty-mirror
/// guard (§5) against the final mode.
fn area_enumerated(area: &AreaListing, force_copy: bool) -> bool {
    area_fully_enumerated(
        area.authoritative_ignoring_empty,
        area.clips.is_empty(),
        area_mode(area, force_copy),
    )
}

/// Whether a Library area is present and fully enumerated (the implicit
/// protector counts; `library="off"` leaves no Library area, so this is false).
fn library_authoritative(areas: &[AreaListing], force_copy: bool) -> bool {
    areas
        .iter()
        .any(|a| matches!(a.kind, AreaKind::Library) && area_enumerated(a, force_copy))
}

/// The per-source enumeration status of every area, for the deletion verdict.
fn source_statuses(areas: &[AreaListing], force_copy: bool) -> Vec<SourceStatus> {
    areas
        .iter()
        .map(|area| SourceStatus {
            mode: area_mode(area, force_copy),
            fully_enumerated: area_enumerated(area, force_copy),
        })
        .collect()
}

/// Whether first-use adoption can confirm identity from this run's listing.
///
/// An authoritative Library is the usual anchor, but a fully-enumerated Mirror
/// source of any kind (e.g. a playlist under `library="off"`) also arms
/// deletion. Deleting against an account this library was never pinned to is
/// the hole the owner pin closes (#149), so such a run is treated as enumerated:
/// `adopt_decision` then confirms identity by clip overlap and aborts on a
/// foreign account instead of skipping the pin.
fn adoption_enumerated(areas: &[AreaListing], force_copy: bool) -> bool {
    library_authoritative(areas, force_copy)
        || deletion_allowed(&source_statuses(areas, force_copy))
}

/// Build the clip union across areas in canonical order, first area winning per
/// id so the Library payload is kept (H1).
fn union_clips(areas: &[AreaListing]) -> Vec<Clip> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut union: Vec<Clip> = Vec::new();
    for area in areas {
        for clip in &area.clips {
            if seen.insert(clip.id.clone()) {
                union.push(clip.clone());
            }
        }
    }
    union
}

/// A playlist area whose listing could not be resolved or fetched: it holds no
/// clips and is never authoritative, so it suppresses deletion without ever
/// vanishing from the sources (§6).
fn unresolved_playlist_area(mode: SourceMode) -> AreaListing {
    AreaListing {
        kind: AreaKind::Playlist {
            id: String::new(),
            name: String::new(),
        },
        mode,
        clips: Vec::new(),
        authoritative_ignoring_empty: false,
    }
}

/// List every selected area (IO), in canonical order Library > Liked > Playlist.
///
/// A failed *secondary* area (liked, a playlist, or the unfiltered library
/// protector) warns and contributes a non-enumerated, empty source so one
/// failure suppresses all deletion while successful areas still download (§6). A
/// failed *plain* library listing (the sole area of a classic run) keeps today's
/// hard abort, and an unresolvable explicit `--playlist X` typo keeps today's
/// hard [`ExitCode::Config`].
async fn enumerate_areas(
    selection: &ResolvedSelection,
    client: &SunoClient<TokioClock>,
    http: &ReqwestHttp,
    label: &str,
    args: &SyncArgs,
    verbosity: i8,
    concurrency: u32,
) -> std::result::Result<Vec<AreaListing>, ExitCode> {
    let mut areas: Vec<AreaListing> = Vec::new();
    // A `--limit`/`--since` narrowing is a deliberate act, so a narrowed Library
    // or Liked area is not authoritative; the unfiltered protector ignores it (D2)
    // and playlists take neither flag.
    let narrowed = is_narrowed(args.limit, args.since.as_deref());

    if let Some(lib) = selection.library {
        if lib.unfiltered {
            // Protector / configured Library: list the whole feed, ignoring any
            // `--limit`/`--since` so a stray narrowing never disarms it (D2).
            match client.list_clips(http, false, None).await {
                Ok((clips, complete)) => areas.push(AreaListing {
                    kind: AreaKind::Library,
                    mode: lib.mode,
                    clips,
                    authoritative_ignoring_empty: complete,
                }),
                Err(err) => {
                    if verbosity >= -1 {
                        eprint_t!(
                            "warning: library listing failed ({err}); suppressing deletion this run"
                        );
                    }
                    areas.push(AreaListing {
                        kind: AreaKind::Library,
                        mode: lib.mode,
                        clips: Vec::new(),
                        authoritative_ignoring_empty: false,
                    });
                }
            }
        } else {
            // Plain Library run: honours `--limit`, and a listing failure aborts
            // exactly as today (the run has no other data source).
            match client.list_clips(http, false, args.limit).await {
                Ok((clips, complete)) => areas.push(AreaListing {
                    kind: AreaKind::Library,
                    mode: lib.mode,
                    clips,
                    authoritative_ignoring_empty: complete && !narrowed,
                }),
                Err(err) => return Err(report_listing_failure(label, &err)),
            }
        }
    }

    if let Some(mode) = selection.liked {
        match client.list_clips(http, true, None).await {
            Ok((clips, complete)) => areas.push(AreaListing {
                kind: AreaKind::Liked,
                mode,
                clips,
                authoritative_ignoring_empty: complete && !narrowed,
            }),
            Err(err) => {
                if verbosity >= -1 {
                    eprint_t!(
                        "warning: liked feed failed to list ({err}); suppressing deletion this run"
                    );
                }
                areas.push(AreaListing {
                    kind: AreaKind::Liked,
                    mode,
                    clips: Vec::new(),
                    authoritative_ignoring_empty: false,
                });
            }
        }
    }

    if !matches!(selection.playlists, PlaylistPolicy::None) {
        // Resolve names and enumerate the `All` group via the account's playlists.
        let playlists = match client.get_playlists(http).await {
            Ok(playlists) => Some(playlists),
            Err(err) => {
                if selection.cli_scoped {
                    return Err(report_listing_failure(label, &err));
                }
                if verbosity >= -1 {
                    eprint_t!(
                        "warning: playlist listing failed ({err}); suppressing deletion this run"
                    );
                }
                None
            }
        };
        match (&selection.playlists, playlists) {
            (PlaylistPolicy::Explicit(list), Some(pls)) => {
                let mut to_fetch: Vec<(String, String, SourceMode)> = Vec::new();
                for (value, mode) in list {
                    let playlist = match resolve_playlist(value, &pls) {
                        Ok(playlist) => playlist,
                        Err(err) => {
                            if selection.cli_scoped {
                                eprint_t!("error: {err}.");
                                print_visible_playlists(&pls, verbosity);
                                return Err(ExitCode::Config);
                            }
                            if verbosity >= -1 {
                                eprint_t!(
                                    "warning: a configured playlist could not be resolved ({err}); leaving its .m3u8 untouched"
                                );
                            }
                            areas.push(unresolved_playlist_area(*mode));
                            continue;
                        }
                    };
                    to_fetch.push((playlist.id.clone(), playlist.name.clone(), *mode));
                }
                let fetched = stream::iter(to_fetch)
                    .map(|(id, name, mode)| async move {
                        list_playlist_area(client, http, &id, &name, mode, narrowed, verbosity)
                            .await
                    })
                    .buffered(concurrency.max(1) as usize)
                    .collect::<Vec<_>>()
                    .await;
                areas.extend(fetched);
            }
            (PlaylistPolicy::All { default, overrides }, Some(pls)) => {
                let to_fetch: Vec<(String, String, SourceMode)> = pls
                    .iter()
                    .map(|playlist| {
                        (
                            playlist.id.clone(),
                            playlist.name.clone(),
                            overrides.get(&playlist.id).copied().unwrap_or(*default),
                        )
                    })
                    .collect();
                let fetched = stream::iter(to_fetch)
                    .map(|(id, name, mode)| async move {
                        list_playlist_area(client, http, &id, &name, mode, narrowed, verbosity)
                            .await
                    })
                    .buffered(concurrency.max(1) as usize)
                    .collect::<Vec<_>>()
                    .await;
                areas.extend(fetched);
            }
            (PlaylistPolicy::Explicit(list), None) => {
                for (_, mode) in list {
                    areas.push(unresolved_playlist_area(*mode));
                }
            }
            (PlaylistPolicy::All { default, .. }, None) => {
                areas.push(unresolved_playlist_area(*default));
            }
            (PlaylistPolicy::None, _) => {}
        }
    }

    Ok(areas)
}

/// List one playlist's members (IO), filtering to downloadable clips. A failure
/// contributes a non-enumerated, empty source (§6); a member lost to the
/// downloadable filter marks the area non-authoritative so its Mirror cannot
/// delete this run (§4).
async fn list_playlist_area(
    client: &SunoClient<TokioClock>,
    http: &ReqwestHttp,
    id: &str,
    name: &str,
    mode: SourceMode,
    narrowed: bool,
    verbosity: i8,
) -> AreaListing {
    match client.get_playlist_clips(http, id).await {
        Ok((raw, complete)) => {
            let raw_len = raw.len();
            let clips: Vec<Clip> = raw.into_iter().filter(is_downloadable).collect();
            let any_filtered = clips.len() < raw_len;
            AreaListing {
                kind: AreaKind::Playlist {
                    id: id.to_owned(),
                    name: name.to_owned(),
                },
                mode,
                clips,
                authoritative_ignoring_empty: playlist_authoritative(
                    complete,
                    any_filtered,
                    narrowed,
                ),
            }
        }
        Err(err) => {
            if verbosity >= -1 {
                eprint_t!(
                    "warning: playlist '{name}' members failed to list ({err}); suppressing deletion this run"
                );
            }
            AreaListing {
                kind: AreaKind::Playlist {
                    id: id.to_owned(),
                    name: name.to_owned(),
                },
                mode,
                clips: Vec::new(),
                authoritative_ignoring_empty: false,
            }
        }
    }
}

/// Build the `.m3u8` desired state for an area-scoped run (no authoritative
/// Library). Only the playlist and liked areas that fully enumerated their
/// members are rendered, and only when `members_intact` (the union was not
/// truncated by `--limit`/`--since`, so `desired` still holds every member);
/// every other stored playlist id is protected so no `.m3u8` is rewritten or
/// deleted from a partial view (B2/D3).
fn build_scoped_playlist_desired(
    areas: &[AreaListing],
    desired: &[suno_core::Desired],
    store: &suno_core::LineageStore,
    protected: &mut BTreeSet<String>,
    force_copy: bool,
    members_intact: bool,
) -> (Vec<PlaylistDesired>, bool) {
    let mut owned: Vec<(String, String, Vec<Clip>)> = Vec::new();
    for area in areas {
        match &area.kind {
            AreaKind::Playlist { id, name } => {
                if members_intact && !id.is_empty() && area_enumerated(area, force_copy) {
                    owned.push((id.clone(), name.clone(), area.clips.clone()));
                } else if !id.is_empty() {
                    protected.insert(id.clone());
                }
            }
            AreaKind::Liked => {
                if members_intact && area_enumerated(area, force_copy) {
                    owned.push((
                        LIKED_PLAYLIST_ID.to_owned(),
                        "Liked Songs".to_owned(),
                        area.clips.clone(),
                    ));
                } else {
                    protected.insert(LIKED_PLAYLIST_ID.to_owned());
                }
            }
            AreaKind::Library => {}
        }
    }
    let rendered: BTreeSet<&str> = owned.iter().map(|(id, _, _)| id.as_str()).collect();
    // Protect every stored playlist this run is not authoritatively rewriting, so
    // a non-selected playlist's `.m3u8` is never treated as stale.
    for id in store.playlists.keys() {
        if !rendered.contains(id.as_str()) {
            protected.insert(id.clone());
        }
    }
    let inputs: Vec<PlaylistInput<'_>> = owned
        .iter()
        .map(|(id, name, members)| PlaylistInput {
            id: id.as_str(),
            name: name.as_str(),
            members: members.as_slice(),
        })
        .collect();
    (build_playlist_desired(&inputs, desired), true)
}

/// Print the account's own playlists to help a user correct a `--playlist` typo.
fn print_visible_playlists(playlists: &[suno_core::Playlist], verbosity: i8) {
    if verbosity < -1 {
        return;
    }
    if playlists.is_empty() {
        eprint_t!("no playlists are visible for this account.");
        return;
    }
    eprint_t!("visible playlists:");
    for playlist in playlists {
        eprint_t!("  {} ({})", playlist.name, playlist.id);
    }
}

/// Fetch this run's playlists best-effort and build their desired `.m3u8`
/// state, honouring HARDENING B2 at every step.
///
/// Only ever called on a fully-enumerated run (the caller gates on that). A
/// failed `/api/playlist/me` listing returns `(empty, false)` so the planner
/// makes no playlist writes or deletes and every existing `.m3u8` is left
/// untouched. A single playlist whose member fetch fails, or a truncated liked
/// feed, is added to `protected` and excluded from the desired set, so the
/// caller can also exclude it from the stale-delete candidate set: its file is
/// neither rewritten nor removed. The synthetic liked feed is appended last, in
/// liked order, under the id [`LIKED_PLAYLIST_ID`].
async fn fetch_playlist_desired(
    client: &SunoClient<TokioClock>,
    http: &ReqwestHttp,
    desired: &[suno_core::Desired],
    protected: &mut BTreeSet<String>,
    verbosity: i8,
    concurrency: u32,
) -> (Vec<PlaylistDesired>, bool) {
    let playlists = match client.get_playlists(http).await {
        Ok(playlists) => playlists,
        Err(err) => {
            if verbosity >= -1 {
                eprint_t!(
                    "warning: playlist listing failed ({err}); leaving existing .m3u8 files untouched"
                );
            }
            return (Vec::new(), false);
        }
    };

    // Own each playlist's members so the borrowed `PlaylistInput`s stay valid. A
    // playlist whose single page did not return its whole member set (D5) is
    // protected rather than rendered from a truncated page (B2).
    let mut fetched: Vec<(String, String, Vec<Clip>)> = Vec::new();
    let member_results = stream::iter(playlists.iter())
        .map(|playlist| async move {
            (
                playlist.id.clone(),
                playlist.name.clone(),
                client.get_playlist_clips(http, &playlist.id).await,
            )
        })
        .buffered(concurrency.max(1) as usize)
        .collect::<Vec<_>>()
        .await;
    for (id, name, result) in member_results {
        match result {
            Ok((members, true)) => fetched.push((id, name, members)),
            Ok((_, false)) => {
                if verbosity >= -1 {
                    eprint_t!(
                        "warning: playlist '{}' returned an incomplete member page; keeping its .m3u8 unchanged",
                        name
                    );
                }
                protected.insert(id);
            }
            Err(err) => {
                if verbosity >= -1 {
                    eprint_t!(
                        "warning: playlist '{}' members failed to list ({err}); keeping its .m3u8 unchanged",
                        name
                    );
                }
                protected.insert(id);
            }
        }
    }

    // The liked feed becomes a synthetic "Liked Songs" playlist, but only when it
    // drained fully: a truncated feed would render a short playlist and is left
    // untouched instead (B2).
    match client.list_clips(http, true, None).await {
        Ok((liked, true)) => {
            fetched.push((
                LIKED_PLAYLIST_ID.to_owned(),
                "Liked Songs".to_owned(),
                liked,
            ));
        }
        Ok((_, false)) => {
            if verbosity >= -1 {
                eprint_t!("warning: liked feed was truncated; keeping Liked Songs.m3u8 unchanged");
            }
            protected.insert(LIKED_PLAYLIST_ID.to_owned());
        }
        Err(err) => {
            if verbosity >= -1 {
                eprint_t!(
                    "warning: liked feed failed to list ({err}); keeping Liked Songs.m3u8 unchanged"
                );
            }
            protected.insert(LIKED_PLAYLIST_ID.to_owned());
        }
    }

    let inputs: Vec<PlaylistInput<'_>> = fetched
        .iter()
        .map(|(id, name, members)| PlaylistInput {
            id: id.as_str(),
            name: name.as_str(),
            members: members.as_slice(),
        })
        .collect();
    (build_playlist_desired(&inputs, desired), true)
}

/// Reconcile `desired` against `manifest` (already loaded), then append the
/// folder-art and playlist plans.
///
/// Shared by the dry-run and executing paths. The manifest is loaded and the
/// desired `.lrc` artifacts resolved by the caller *before* this, so reconcile
/// sees each `.lrc`'s real content hash. Statting absent files is harmless, so
/// this never creates the destination directory. The folder-art actions share
/// the run's single deletion verdict ([`deletion_allowed`]) so album art is
/// never removed on an incomplete listing, and they land on the same [`Plan`] so
/// the mass-delete cap and the confirmation prompt already cover them.
///
/// Playlists carry a second, independent gate: `playlists_enumerated` is true
/// only when the playlist listing succeeded on a fully-enumerated run.
/// [`plan_playlist_artifacts`] emits a playlist delete only when BOTH the shared
/// `can_delete` verdict and `playlists_enumerated` hold, so a failed, empty, or
/// partial playlist listing never removes an existing `.m3u8` (HARDENING B2).
/// These deletes also count toward the mass-delete cap via [`Plan::artifact_deletes`].
///
/// `sources` is one [`SourceStatus`] per selected area, so [`deletion_allowed`]
/// requires every area fully enumerated and at least one Mirror. Folder art
/// carries the extra `library_authoritative` gate: without an authoritative
/// Library the folder view is partial, so art is neither rewritten (the caller
/// passes an empty `albums_desired`) nor deleted.
#[allow(clippy::too_many_arguments)]
async fn reconcile_run(
    manifest: &suno_core::Manifest,
    dest: &Path,
    desired: &[suno_core::Desired],
    albums_desired: &[AlbumDesired],
    albums: &BTreeMap<String, AlbumArt>,
    playlist_desired: &[PlaylistDesired],
    playlists: &BTreeMap<String, PlaylistState>,
    sources: &[SourceStatus],
    library_authoritative: bool,
    playlists_enumerated: bool,
) -> suno_core::Plan {
    let local = stat_manifest(dest, manifest, albums, playlists).await;
    let can_delete = deletion_allowed(sources);
    let art_can_delete = can_delete && library_authoritative;
    let mut plan = reconcile(manifest, desired, &local, sources);
    plan.actions.extend(plan_album_artifacts(
        albums_desired,
        albums,
        art_can_delete,
        &local,
    ));
    plan.actions.extend(plan_playlist_artifacts(
        playlist_desired,
        playlists,
        can_delete,
        playlists_enumerated,
        &local,
    ));
    plan
}

/// Stat every manifest path and all tracked artifact paths so reconcile can
/// spot missing or empty files.
///
/// Returns a combined map keyed by both clip-id (for audio) and file path (for
/// per-clip sidecars, folder art, and playlist files). Statting absent paths is
/// harmless; the caller's destination directory need not exist yet.
async fn stat_manifest(
    dest: &Path,
    manifest: &suno_core::Manifest,
    albums: &BTreeMap<String, AlbumArt>,
    playlists: &BTreeMap<String, PlaylistState>,
) -> HashMap<String, LocalFile> {
    // Collect (key, absolute_path) pairs to stat. Audio is keyed by clip_id;
    // everything else is keyed by its stored relative path, deduplicated.
    let mut to_stat: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (clip_id, entry) in manifest.iter() {
        // Audio file, keyed by clip_id (may share a path with another clip; stat separately).
        to_stat.push((clip_id.clone(), dest.join(&entry.path)));

        for path in [
            entry.cover_jpg.as_ref().map(|s| s.path.as_str()),
            entry.cover_webp.as_ref().map(|s| s.path.as_str()),
            entry.details_txt.as_ref().map(|s| s.path.as_str()),
            entry.lyrics_txt.as_ref().map(|s| s.path.as_str()),
            entry.lrc.as_ref().map(|s| s.path.as_str()),
            entry.video_mp4.as_ref().map(|s| s.path.as_str()),
        ]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        {
            if seen.insert(path.to_owned()) {
                to_stat.push((path.to_owned(), dest.join(path)));
            }
        }

        for state in entry.stems.values().filter(|s| !s.path.is_empty()) {
            if seen.insert(state.path.clone()) {
                to_stat.push((state.path.clone(), dest.join(&state.path)));
            }
        }
    }

    for art in albums.values() {
        for state in [
            art.folder_jpg.as_ref(),
            art.folder_webp.as_ref(),
            art.folder_mp4.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.path.is_empty())
        {
            if seen.insert(state.path.clone()) {
                to_stat.push((state.path.clone(), dest.join(&state.path)));
            }
        }
    }

    for state in playlists.values().filter(|s| !s.path.is_empty()) {
        if seen.insert(state.path.clone()) {
            to_stat.push((state.path.clone(), dest.join(&state.path)));
        }
    }

    tokio::task::spawn_blocking(move || {
        to_stat
            .into_iter()
            .map(|(key, path)| {
                let meta = std::fs::metadata(&path).ok();
                let local = LocalFile {
                    exists: meta.is_some(),
                    size: meta.map(|m| m.len()).unwrap_or(0),
                };
                (key, local)
            })
            .collect()
    })
    .await
    .expect("stat_manifest blocking task panicked")
}

/// Whether a file extension names one of the audio formats we write.
fn is_audio_ext(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "flac" | "mp3" | "wav")
}

/// Walk `dest` recursively for audio files, returning their paths relative to
/// `dest` with forward slashes, for the orphan report. Best-effort and
/// read-only: an unreadable directory (or an absent `dest`) contributes
/// nothing, so a dry run never fails on a walk error.
fn walk_audio_files(dest: &Path) -> Vec<String> {
    fn recurse(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                recurse(root, &path, out);
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(is_audio_ext)
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    recurse(dest, dest, &mut out);
    out
}

/// True when the plan would change disk (anything but skips).
fn plan_has_changes(plan: &suno_core::Plan) -> bool {
    plan.downloads()
        + plan.reformats()
        + plan.retags()
        + plan.renames()
        + plan.artifact_moves()
        + plan.stem_moves()
        + plan.deletes()
        + plan.artifact_writes()
        + plan.artifact_deletes()
        + plan.stem_writes()
        + plan.stem_deletes()
        > 0
}

/// Every path this plan would remove: audio deletes and sidecar (artifact)
/// deletes alike, so the confirmation listing reflects the full destructive
/// footprint, not just the audio files.
fn deletion_paths(plan: &suno_core::Plan) -> Vec<String> {
    plan.actions
        .iter()
        .filter_map(|action| match action {
            suno_core::Action::Delete { path, .. }
            | suno_core::Action::DeleteArtifact { path, .. }
            | suno_core::Action::DeleteStem { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

/// Print the deletion list and read a `[y/N]` answer from stdin.
fn prompt_delete(plan: &suno_core::Plan, verbosity: i8) -> Result<bool> {
    let paths = deletion_paths(plan);
    let show = if verbosity >= 1 {
        paths.len()
    } else {
        PROMPT_PATH_LIMIT
    };
    eprint!("{} [y/N] ", output::delete_prompt(&paths, show));
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("could not read confirmation")?;
    Ok(confirmed(&answer))
}

pub(crate) fn report_auth_failure(label: &str, err: &CoreError) -> ExitCode {
    eprint_t!(
        "error: authentication failed for account '{label}'\n\nThe stored token may have expired. Re-authenticate with:\n  suno auth refresh {label}\n\nIf the token was rotated in Suno, update it with:\n  suno config add-account {label} --token <new-token>"
    );
    let _ = err;
    ExitCode::Auth
}

pub(crate) fn report_listing_failure(label: &str, err: &CoreError) -> ExitCode {
    match err {
        CoreError::Auth(_) => report_auth_failure(label, err),
        CoreError::Connection(_) | CoreError::RateLimited { .. } => {
            eprint_t!(
                "error: could not list the library for '{label}': {err}\n  No files were written. Re-run when connectivity is restored."
            );
            ExitCode::Transient
        }
        other => {
            eprint_t!("error: could not list the library for '{label}': {other}");
            ExitCode::General
        }
    }
}

/// A one-account config used when running purely from `--token`/env.
fn synthetic_config() -> Config {
    let mut config = Config::default();
    config
        .accounts
        .insert("default".to_owned(), suno_core::AccountConfig::default());
    config
}

/// The first eight characters of an id, for user-facing messages. The full id
/// (and never the token) may go to the audit file, but only a short prefix is
/// ever printed.
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// A pin/adopt/re-pin that this run will apply: its audit action (`PIN`,
/// `ADOPT`, or `REPIN`) and the stderr notice, both deferred to the executing
/// path so they are emitted only when the pin is actually persisted.
struct PendingPin {
    action: &'static str,
    notice: String,
}

/// Record a pin/adopt/re-pin: pin the owner in `store`, mark `owner_dirty`,
/// and queue the `PendingPin` notice. Called from the four owner-gate arms
/// that differ only in `action` and `notice`.
fn set_pin(
    store: &mut suno_core::LineageStore,
    owner_dirty: &mut bool,
    pending_pin: &mut Option<PendingPin>,
    user_id: &str,
    account: &str,
    action: &'static str,
    notice: String,
) {
    store.pin_owner(Owner {
        user_id: user_id.to_owned(),
        display_name: account.to_owned(),
    });
    *owner_dirty = true;
    *pending_pin = Some(PendingPin { action, notice });
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The current UTC instant as an RFC 3339 timestamp (`YYYY-MM-DDThh:mm:ssZ`),
/// used to stamp `first_seen_at`/`last_seen_at` on graph nodes and edges.
fn now_rfc3339() -> String {
    rfc3339_from_unix(now_secs())
}

/// Format Unix seconds as an RFC 3339 UTC timestamp via Howard Hinnant's
/// civil-from-days algorithm, avoiding a date-library dependency for a single
/// audit stamp.
fn rfc3339_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = (secs % 86_400) as i64;
    let (hour, minute, second) = (tod / 3_600, (tod % 3_600) / 60, tod % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn read_last_run(dest: &Path) -> Option<u64> {
    std::fs::read_to_string(dest.join(LAST_RUN_NAME))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write_last_run(dest: &Path) {
    let path = dest.join(LAST_RUN_NAME);
    if std::fs::write(&path, now_secs().to_string()).is_ok() {
        #[cfg(unix)]
        let _ = set_permissions_or_remove(&path, PRIVATE_STATE_FILE_MODE);
    }
}

/// Resolve when a SIGINT (Ctrl-C) or, on Unix, a SIGTERM arrives.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(accounts: &[(&str, Option<&str>)]) -> Config {
        let mut cfg = Config::default();
        for (label, root) in accounts {
            let acc = suno_core::AccountConfig {
                root: root.map(str::to_owned),
                ..Default::default()
            };
            cfg.accounts.insert((*label).to_owned(), acc);
        }
        cfg
    }

    fn sel<'a>(
        all: bool,
        account: Option<&'a str>,
        dest: Option<&'a Path>,
        token: bool,
    ) -> Selection<'a> {
        Selection {
            all,
            account,
            dest,
            token_available: token,
        }
    }

    fn settings_with(
        token: Option<&str>,
        stored_token: Option<&str>,
        token_command: Option<&str>,
    ) -> suno_core::EffectiveSettings {
        suno_core::EffectiveSettings {
            token: token.map(str::to_owned),
            stored_token: stored_token.map(str::to_owned),
            token_command: token_command.map(str::to_owned),
            account_id: None,
            format: suno_core::AudioFormat::Flac,
            concurrency: 4,
            retries: 3,
            min_newest: 1,
            animated_covers: false,
            raw_animated_cover: false,
            video_cover_retention: suno_core::VideoCoverRetention::Neither,
            animated_cover_webp: suno_core::WebpEncodeSettings::default(),
            details_sidecar: false,
            lyrics_sidecar: false,
            lrc_sidecar: false,
            video_mp4: false,
            download_stems: false,
            stem_format: suno_core::StemFormat::Wav,
            naming_template: "{title}".to_owned(),
            character_set: suno_core::CharacterSet::Unicode,
            areas: None,
            album_overrides: std::collections::BTreeMap::new(),
        }
    }

    #[cfg(unix)]
    fn success_command(token: &str) -> String {
        format!("printf '%s\\n' {}", shell_single_quote(token))
    }

    #[cfg(unix)]
    fn fail_command(output: &str) -> String {
        format!("printf '%s' {}; exit 23", shell_single_quote(output))
    }

    #[cfg(unix)]
    fn shell_single_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    #[cfg(unix)]
    #[test]
    fn last_run_marker_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = Path::new("target").join(format!(
            "run-last-run-perms-{}-{}",
            std::process::id(),
            now_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        write_last_run(&dir);
        let mode = std::fs::metadata(dir.join(LAST_RUN_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn implicit_target_needs_token_and_dest() {
        let dest = PathBuf::from("/music");
        let s = sel(false, None, Some(&dest), true);
        let targets = plan_targets(None, &s).unwrap();
        assert_eq!(targets.len(), 1);
        assert!(targets[0].implicit);
        assert_eq!(targets[0].dest, dest);
    }

    #[tokio::test]
    async fn reconcile_run_reads_a_missing_destination_as_empty() {
        // The dry-run / check path reads through a missing destination as an
        // empty manifest without creating it, so it never touches disk.
        let dir =
            Path::new("target").join(format!("run-nodir-{}-{}", std::process::id(), now_secs()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!dir.exists());
        let sources = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        }];
        let manifest = logs::load_manifest(&dir).unwrap();
        let plan = reconcile_run(
            &manifest,
            &dir,
            &[],
            &[],
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
            &sources,
            false,
            false,
        )
        .await;
        assert!(manifest.is_empty());
        assert!(plan.actions.is_empty());
        assert!(
            !dir.exists(),
            "dry-run path must not create the destination directory"
        );
    }

    #[test]
    fn synced_lyrics_fetch_warning_never_leaks_a_clip_id_or_url() {
        // The fetch-failure warning must not carry the request URL or clip id: a
        // reqwest transport error's text can include `/api/gen/{id}/...`, so the
        // raw error is never interpolated. This guards that redaction.
        let msg = SYNCED_LYRICS_FETCH_WARNING;
        assert!(!msg.contains("/api/gen/"));
        assert!(!msg.contains("aligned_lyrics"));
        assert!(!msg.contains('{'), "no interpolation placeholder");
        assert!(!msg.contains("http"));
    }

    #[test]
    fn implicit_without_token_errors() {
        let dest = PathBuf::from("/music");
        let s = sel(false, None, Some(&dest), false);
        assert!(plan_targets(None, &s).is_err());
    }

    #[test]
    fn implicit_without_dest_errors() {
        let s = sel(false, None, None, true);
        assert!(plan_targets(None, &s).is_err());
    }

    #[test]
    fn token_available_accepts_global_token_flag() {
        let global = GlobalArgs {
            token: Some("flag-token".to_owned()),
            ..Default::default()
        };
        assert!(super::token_available(&global, &HashMap::new()));
    }

    #[test]
    fn token_available_accepts_token_env() {
        let global = GlobalArgs::default();
        let env: HashMap<String, String> = [("SUNO_TOKEN".to_owned(), "env-token".to_owned())]
            .into_iter()
            .collect();
        assert!(super::token_available(&global, &env));
    }

    #[test]
    fn token_available_accepts_token_command_env() {
        let global = GlobalArgs::default();
        let env: HashMap<String, String> =
            [("SUNO_TOKEN_COMMAND".to_owned(), "printf secret".to_owned())]
                .into_iter()
                .collect();
        assert!(super::token_available(&global, &env));
    }

    #[test]
    fn token_available_accepts_default_account_env() {
        let global = GlobalArgs::default();
        let env: HashMap<String, String> =
            [("SUNO_DEFAULT_TOKEN".to_owned(), "env-token".to_owned())]
                .into_iter()
                .collect();
        assert!(super::token_available(&global, &env));
    }

    #[test]
    fn token_available_accepts_explicit_account_command_env() {
        let global = GlobalArgs {
            account: Some("my-lib".to_owned()),
            ..Default::default()
        };
        let env: HashMap<String, String> = [(
            "SUNO_MY_LIB_TOKEN_COMMAND".to_owned(),
            "printf secret".to_owned(),
        )]
        .into_iter()
        .collect();
        assert!(super::token_available(&global, &env));
    }

    #[test]
    fn token_available_ignores_other_account_env() {
        let global = GlobalArgs::default();
        let env: HashMap<String, String> =
            [("SUNO_ALICE_TOKEN".to_owned(), "env-token".to_owned())]
                .into_iter()
                .collect();
        assert!(!super::token_available(&global, &env));
    }

    #[test]
    fn single_account_accepts_implicit_token_command_env() {
        let global = GlobalArgs::default();
        let env: HashMap<String, String> =
            [("SUNO_TOKEN_COMMAND".to_owned(), "printf token".to_owned())]
                .into_iter()
                .collect();
        let (label, settings) =
            single_account(None, &global, &FlagOverrides::default(), &env).unwrap();
        assert_eq!(label, "default");
        assert_eq!(settings.token_command.as_deref(), Some("printf token"));
    }

    #[tokio::test]
    async fn resolve_token_prefers_direct_token_over_stored_token() {
        let settings = settings_with(Some("flag-token"), Some("stored-token"), None);
        let token = resolve_token("alice", &settings).await.unwrap();
        assert_eq!(token.as_deref(), Some("flag-token"));
    }

    #[tokio::test]
    async fn resolve_token_falls_back_to_stored_token() {
        let settings = settings_with(None, Some("stored-token"), None);
        let token = resolve_token("alice", &settings).await.unwrap();
        assert_eq!(token.as_deref(), Some("stored-token"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_token_uses_trimmed_command_stdout() {
        let settings = settings_with(
            None,
            Some("stored-token"),
            Some(&success_command("cmd-token")),
        );
        let token = resolve_token("alice", &settings).await.unwrap();
        assert_eq!(token.as_deref(), Some("cmd-token"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_token_command_failure_is_clear_and_redacted() {
        let secret = "secret-command-output";
        let settings = settings_with(None, Some("stored-token"), Some(&fail_command(secret)));
        let err = resolve_token("alice", &settings).await.unwrap_err();
        assert!(err.contains("token_command for account 'alice' failed with exit status 23"));
        assert!(!err.contains(secret), "error leaked command output: {err}");
        assert!(!err.contains("stored-token"), "error leaked token: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_token_command_rejects_whitespace_output() {
        let settings = settings_with(None, Some("stored-token"), Some("printf '   \\n\\t'"));
        let err = resolve_token("alice", &settings).await.unwrap_err();
        assert!(err.contains("produced empty output"));
        assert!(!err.contains("stored-token"), "error leaked token: {err}");
    }

    #[test]
    fn account_uses_dest_then_root() {
        let cfg = config_with(&[("alice", Some("/lib/alice"))]);
        let dest = PathBuf::from("/override");
        let with_dest =
            plan_targets(Some(&cfg), &sel(false, Some("alice"), Some(&dest), true)).unwrap();
        assert_eq!(with_dest[0].dest, dest);
        let from_root = plan_targets(Some(&cfg), &sel(false, Some("alice"), None, true)).unwrap();
        assert_eq!(from_root[0].dest, PathBuf::from("/lib/alice"));
    }

    #[test]
    fn account_without_dest_or_root_errors() {
        let cfg = config_with(&[("alice", None)]);
        assert!(plan_targets(Some(&cfg), &sel(false, Some("alice"), None, true)).is_err());
    }

    #[test]
    fn unknown_account_errors_with_listing() {
        let cfg = config_with(&[("alice", Some("/a")), ("bob", Some("/b"))]);
        let err = plan_targets(Some(&cfg), &sel(false, Some("carol"), None, true)).unwrap_err();
        assert!(err.contains("carol"));
        assert!(err.contains("alice"));
        assert!(err.contains("bob"));
    }

    #[test]
    fn all_runs_every_account_from_roots() {
        let cfg = config_with(&[("alice", Some("/a")), ("bob", Some("/b"))]);
        let targets = plan_targets(Some(&cfg), &sel(true, None, None, true)).unwrap();
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().all(|t| !t.implicit));
        // Sorted by label for determinism.
        assert_eq!(targets[0].label, "alice");
        assert_eq!(targets[1].label, "bob");
    }

    #[test]
    fn all_rejects_dest() {
        let cfg = config_with(&[("alice", Some("/a"))]);
        let dest = PathBuf::from("/x");
        assert!(plan_targets(Some(&cfg), &sel(true, None, Some(&dest), true)).is_err());
    }

    #[test]
    fn all_requires_roots() {
        let cfg = config_with(&[("alice", None)]);
        assert!(plan_targets(Some(&cfg), &sel(true, None, None, true)).is_err());
    }

    #[test]
    fn all_without_config_errors() {
        assert!(plan_targets(None, &sel(true, None, None, true)).is_err());
    }

    #[test]
    fn single_account_config_is_used_implicitly() {
        let cfg = config_with(&[("solo", Some("/solo"))]);
        let targets = plan_targets(Some(&cfg), &sel(false, None, None, false)).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "solo");
        assert!(!targets[0].implicit);
    }

    #[test]
    fn multiple_accounts_need_selection() {
        let cfg = config_with(&[("alice", Some("/a")), ("bob", Some("/b"))]);
        let err = plan_targets(Some(&cfg), &sel(false, None, None, true)).unwrap_err();
        assert!(err.contains("--account"));
        assert!(err.contains("--all"));
    }

    #[test]
    fn worse_prefers_higher_code() {
        assert_eq!(worse(ExitCode::Ok, ExitCode::Partial), ExitCode::Partial);
        assert_eq!(worse(ExitCode::Safety, ExitCode::Auth), ExitCode::Safety);
        assert_eq!(worse(ExitCode::Ok, ExitCode::Ok), ExitCode::Ok);
    }

    #[test]
    fn verb_modes_and_labels() {
        assert_eq!(Verb::Sync.mode(), SourceMode::Mirror);
        assert_eq!(Verb::Check.mode(), SourceMode::Mirror);
        assert_eq!(Verb::Copy.mode(), SourceMode::Copy);
        assert_eq!(Verb::Copy.summary_label(), "Copy");
    }

    #[test]
    fn artifact_only_deletes_drive_the_confirmation_gate() {
        use suno_core::{Action, ArtifactKind, Plan};
        // A plan with zero audio deletes but several sidecar deletes must still
        // gate: run.rs feeds plan.deletes() + plan.artifact_deletes() into
        // confirm_decision, so it prompts on a TTY and refuses without one.
        let plan = Plan {
            actions: (0..3)
                .map(|i| Action::DeleteArtifact {
                    kind: ArtifactKind::CoverJpg,
                    path: format!("c{i}/cover.jpg"),
                    owner_id: format!("c{i}"),
                })
                .collect(),
        };
        let delete_count = plan.deletes() + plan.artifact_deletes();
        assert_eq!(plan.deletes(), 0);
        assert_eq!(delete_count, 3);

        assert_eq!(
            confirm_decision(true, delete_count, false, true),
            Confirm::Prompt
        );
        assert_eq!(
            confirm_decision(true, delete_count, false, false),
            Confirm::RefuseNonInteractive
        );
        assert_eq!(
            confirm_decision(true, delete_count, true, false),
            Confirm::Proceed
        );

        // The confirmation listing includes the sidecar paths.
        assert_eq!(
            deletion_paths(&plan),
            vec!["c0/cover.jpg", "c1/cover.jpg", "c2/cover.jpg"]
        );
    }

    #[test]
    fn deletion_paths_lists_both_audio_and_sidecar_removals() {
        use suno_core::{Action, ArtifactKind, Plan};
        let plan = Plan {
            actions: vec![
                Action::Delete {
                    path: "a.flac".to_owned(),
                    clip_id: "a".to_owned(),
                },
                Action::DeleteArtifact {
                    kind: ArtifactKind::CoverJpg,
                    path: "a/cover.jpg".to_owned(),
                    owner_id: "a".to_owned(),
                },
                Action::Skip {
                    clip_id: "z".to_owned(),
                },
            ],
        };
        assert_eq!(deletion_paths(&plan), vec!["a.flac", "a/cover.jpg"]);
    }

    #[tokio::test]
    async fn allow_account_change_is_rejected_on_check_before_any_network() {
        // F1: the flag re-pins, which check/dry-run never persist, so run_one
        // must reject it up front with a usage error and never reach auth or the
        // feed. The target points at a bogus dest with no token, proving the
        // early return happens before any listing.
        let global = GlobalArgs::default();
        let args = SyncArgs {
            allow_account_change: true,
            ..Default::default()
        };
        let target = TargetSpec {
            label: "alice".to_owned(),
            dest: PathBuf::from("/nonexistent-check-guard"),
            implicit: false,
        };
        let flags = FlagOverrides::default();
        let env = HashMap::new();
        let code = run_one(
            Verb::Check,
            &global,
            &args,
            &target,
            None,
            &flags,
            &env,
            false,
        )
        .await
        .unwrap();
        assert_eq!(code, ExitCode::Usage);
    }

    #[tokio::test]
    async fn allow_account_change_is_rejected_on_dry_run() {
        // The same rejection applies to any verb under --dry-run.
        let global = GlobalArgs {
            dry_run: true,
            ..Default::default()
        };
        let args = SyncArgs {
            allow_account_change: true,
            ..Default::default()
        };
        let target = TargetSpec {
            label: "alice".to_owned(),
            dest: PathBuf::from("/nonexistent-dryrun-guard"),
            implicit: false,
        };
        let flags = FlagOverrides::default();
        let env = HashMap::new();
        let code = run_one(
            Verb::Sync,
            &global,
            &args,
            &target,
            None,
            &flags,
            &env,
            false,
        )
        .await
        .unwrap();
        assert_eq!(code, ExitCode::Usage);
    }

    fn tclip(id: &str) -> Clip {
        Clip {
            id: id.to_owned(),
            title: "Song".to_owned(),
            handle: "alice".to_owned(),
            ..Default::default()
        }
    }

    fn area(kind: AreaKind, mode: SourceMode, ids: &[&str], authoritative: bool) -> AreaListing {
        AreaListing {
            kind,
            mode,
            clips: ids.iter().map(|id| tclip(id)).collect(),
            authoritative_ignoring_empty: authoritative,
        }
    }

    // Test 5: an empty Mirror area is never authoritative (a legitimately empty
    // mirror is indistinguishable from a dropped listing), so deletion is
    // suppressed. An empty Copy area stays enumerated (it protects nothing).
    #[test]
    fn empty_mirror_area_is_not_enumerated() {
        let mirror = area(AreaKind::Liked, SourceMode::Mirror, &[], true);
        assert!(!area_enumerated(&mirror, false));
        let copy = area(AreaKind::Liked, SourceMode::Copy, &[], true);
        assert!(area_enumerated(&copy, false));
        // A non-empty mirror that fully listed is authoritative.
        let full = area(AreaKind::Liked, SourceMode::Mirror, &["x"], true);
        assert!(area_enumerated(&full, false));
    }

    // A run under `library="off"` that mirrors a fully-enumerated playlist can
    // delete, so first-use adoption must confirm identity (enumerated == true)
    // rather than SkipPin into a delete against an unconfirmed account (#149).
    #[test]
    fn adoption_enumerated_covers_a_mirror_playlist_under_library_off() {
        let playlist = |mode, ids: &[&str], auth| {
            area(
                AreaKind::Playlist {
                    id: "p".into(),
                    name: "P".into(),
                },
                mode,
                ids,
                auth,
            )
        };
        // library="off" + a fully-enumerated Mirror playlist arms deletion.
        assert!(adoption_enumerated(
            &[playlist(SourceMode::Mirror, &["pl"], true)],
            false
        ));
        // A copy-only run cannot delete, so identity need not be confirmed.
        assert!(!adoption_enumerated(
            &[playlist(SourceMode::Copy, &["pl"], true)],
            false
        ));
        // An empty mirror (a dropped or ambiguous listing) is not authoritative.
        assert!(!adoption_enumerated(
            &[playlist(SourceMode::Mirror, &[], true)],
            false
        ));
        // A partial (non-authoritative) mirror listing does not arm adoption.
        assert!(!adoption_enumerated(
            &[playlist(SourceMode::Mirror, &["pl"], false)],
            false
        ));
        // A force-copy (additive) run never deletes, so never forces the pin.
        assert!(!adoption_enumerated(
            &[playlist(SourceMode::Mirror, &["pl"], true)],
            true
        ));
        // The classic authoritative-library anchor still counts.
        assert!(adoption_enumerated(
            &[area(AreaKind::Library, SourceMode::Mirror, &["lib"], true)],
            false,
        ));
    }

    // library_authoritative counts the implicit protector but is false for
    // `library="off"` (no library area at all).
    #[test]
    fn library_authoritative_counts_protector_not_off() {
        let with_protector = vec![
            area(AreaKind::Library, SourceMode::Copy, &["lib"], true),
            area(
                AreaKind::Playlist {
                    id: "p".into(),
                    name: "P".into(),
                },
                SourceMode::Mirror,
                &["pl"],
                true,
            ),
        ];
        assert!(library_authoritative(&with_protector, false));

        let off = vec![area(
            AreaKind::Playlist {
                id: "p".into(),
                name: "P".into(),
            },
            SourceMode::Mirror,
            &["pl"],
            true,
        )];
        assert!(!library_authoritative(&off, false));
    }

    /// (can_delete, library_authoritative, truncate) for a set of areas, exactly
    /// as `run_one` computes them, for the #148 scenario traces.
    fn verdict(areas: &[AreaListing]) -> (bool, bool, bool) {
        let can_delete = deletion_allowed(&source_statuses(areas, false));
        let lib_auth = library_authoritative(areas, false);
        (
            can_delete,
            lib_auth,
            narrows_downloads(can_delete, lib_auth),
        )
    }

    fn pl_area(mode: SourceMode, ids: &[&str], authoritative: bool) -> AreaListing {
        area(
            AreaKind::Playlist {
                id: "p".into(),
                name: "P".into(),
            },
            mode,
            ids,
            authoritative,
        )
    }

    // The #148 behaviour change at the area level: a narrowed playlist mirror
    // neither enumerates nor arms deletion; the same listing un-narrowed does both.
    #[test]
    fn narrowed_playlist_mirror_disarms_deletion() {
        let narrowed = pl_area(
            SourceMode::Mirror,
            &["a"],
            playlist_authoritative(true, false, true),
        );
        assert!(!area_enumerated(&narrowed, false));
        assert!(!deletion_allowed(&source_statuses(&[narrowed], false)));

        let full = pl_area(
            SourceMode::Mirror,
            &["a"],
            playlist_authoritative(true, false, false),
        );
        assert!(area_enumerated(&full, false));
        assert!(deletion_allowed(&source_statuses(&[full], false)));
    }

    // #148 scenario (c): a narrowed playlist mirror WITH the injected full-library
    // protector does not delete (the playlist disarms) and does not narrow
    // downloads (the protector lists the whole library, which drives index/art).
    #[test]
    fn narrowed_playlist_with_protector_neither_deletes_nor_narrows() {
        let areas = vec![
            area(AreaKind::Library, SourceMode::Copy, &["lib"], true),
            pl_area(
                SourceMode::Mirror,
                &["pl"],
                playlist_authoritative(true, false, true),
            ),
        ];
        let (can_delete, lib_auth, truncate) = verdict(&areas);
        assert!(!can_delete, "narrowed playlist mirror is disarmed");
        assert!(lib_auth, "the protector is an authoritative library");
        assert!(
            !truncate,
            "the full library is listed, so downloads are not narrowed"
        );
    }

    // #148 scenario (d): a narrowed playlist mirror under `library="off"` (no
    // protector) does not delete and DOES narrow downloads, matching a narrowed
    // library-only or liked run.
    #[test]
    fn narrowed_playlist_off_disarms_and_narrows() {
        let areas = vec![pl_area(
            SourceMode::Mirror,
            &["pl"],
            playlist_authoritative(true, false, true),
        )];
        let (can_delete, lib_auth, truncate) = verdict(&areas);
        assert!(!can_delete, "narrowed playlist mirror is disarmed");
        assert!(!lib_auth, "library=off leaves no library area");
        assert!(
            truncate,
            "no armed deletion and no full library, so downloads narrow"
        );
    }

    // #148 regression guard for scenario (e): a configured unfiltered
    // `library="mirror"` lists the whole feed regardless of `--limit`/`--since`,
    // so it stays armed and authoritative. The fix must NOT disarm it — that is
    // the #149/D2 guarantee that closes the token-swap hole.
    #[test]
    fn configured_full_library_mirror_still_deletes_when_narrowed() {
        let areas = vec![area(AreaKind::Library, SourceMode::Mirror, &["lib"], true)];
        let (can_delete, lib_auth, truncate) = verdict(&areas);
        assert!(
            can_delete,
            "the configured full-library mirror still deletes"
        );
        assert!(lib_auth);
        assert!(
            !truncate,
            "the full library is listed, so downloads are not narrowed"
        );
    }

    // A narrowed `library="off"` mirror playlist cannot delete (#148), so first-use
    // adoption skips the pin rather than confirming identity — the #149 rule that
    // only a delete-capable run must confirm the account composes cleanly.
    #[test]
    fn adoption_skips_pin_on_a_narrowed_library_off_playlist() {
        let areas = vec![pl_area(
            SourceMode::Mirror,
            &["pl"],
            playlist_authoritative(true, false, true),
        )];
        assert!(!adoption_enumerated(&areas, false));
    }

    // H1: the union keeps the first area's payload per id (Library wins over a
    // later playlist copy of the same clip).
    #[test]
    fn union_keeps_first_area_payload() {
        let mut lib = tclip("shared");
        lib.title = "Library".to_owned();
        let mut pl = tclip("shared");
        pl.title = "Playlist".to_owned();
        let areas = vec![
            AreaListing {
                kind: AreaKind::Library,
                mode: SourceMode::Copy,
                clips: vec![lib, tclip("lib-only")],
                authoritative_ignoring_empty: true,
            },
            AreaListing {
                kind: AreaKind::Playlist {
                    id: "p".into(),
                    name: "P".into(),
                },
                mode: SourceMode::Mirror,
                clips: vec![pl],
                authoritative_ignoring_empty: true,
            },
        ];
        let union = union_clips(&areas);
        assert_eq!(union.len(), 2);
        assert_eq!(union[0].id, "shared");
        assert_eq!(union[0].title, "Library");
        assert_eq!(union[1].id, "lib-only");
    }

    // D1 / Test 3: `sync --playlist X --mode mirror` (no config) protects
    // library-exclusive files while deleting a playlist-exclusive orphan. The
    // protector lists the whole library as Copy, so a library-only manifest entry
    // is stamped Copy in the union and never deleted, even though the playlist
    // Mirror arms the run.
    #[test]
    fn mirror_playlist_protects_library_exclusive_files() {
        use suno_core::{LocalFile, Manifest, ManifestEntry, reconcile};

        // The resolved selection: playlist Mirror + injected library protector.
        let selection = resolve_selection(
            SourceMode::Mirror,
            Some(SourceMode::Mirror),
            false,
            &["holiday".to_owned()],
            None,
            false,
        );
        assert!(selection.library.unwrap().protector);

        // Enumerate: the protector holds the full library (lib-only + shared); the
        // mirror playlist holds shared + pl-only.
        let areas = vec![
            area(
                AreaKind::Library,
                SourceMode::Copy,
                &["lib-only", "shared"],
                true,
            ),
            area(
                AreaKind::Playlist {
                    id: "holiday".into(),
                    name: "Holiday".into(),
                },
                SourceMode::Mirror,
                &["shared", "pl-only"],
                true,
            ),
        ];
        let force_copy = false;
        let sources: Vec<SourceStatus> = areas
            .iter()
            .map(|a| SourceStatus {
                mode: area_mode(a, force_copy),
                fully_enumerated: area_enumerated(a, force_copy),
            })
            .collect();
        assert!(deletion_allowed(&sources), "armed and fully enumerated");

        let area_modes: Vec<(SourceMode, Vec<String>)> = areas
            .iter()
            .map(|a| {
                (
                    area_mode(a, force_copy),
                    a.clips.iter().map(|c| c.id.clone()).collect(),
                )
            })
            .collect();
        let modes = build_modes_by_id(&area_modes);
        // The library-exclusive clip is Copy-only; the shared clip is protected.
        assert_eq!(modes["lib-only"], vec![SourceMode::Copy]);
        assert_eq!(modes["shared"], vec![SourceMode::Mirror, SourceMode::Copy]);
        assert_eq!(modes["pl-only"], vec![SourceMode::Mirror]);

        let union = union_clips(&areas);
        let desired = build_desired(
            &union.iter().collect::<Vec<_>>(),
            suno_core::AudioFormat::Flac,
            &modes,
            &HashMap::new(),
            &BTreeSet::new(),
            ArtifactToggles::default(),
            &suno_core::NamingConfig::default(),
        );

        // Manifest: the three known clips plus a playlist-exclusive orphan that is
        // no longer anywhere in source.
        let mut manifest = Manifest::new();
        for id in ["lib-only", "shared", "pl-only", "gone-orphan"] {
            manifest.insert(
                id,
                ManifestEntry {
                    path: format!("{id}.flac"),
                    format: suno_core::AudioFormat::Flac,
                    size: 100,
                    ..Default::default()
                },
            );
        }
        let local: HashMap<String, LocalFile> = manifest
            .iter()
            .map(|(id, _)| {
                (
                    id.clone(),
                    LocalFile {
                        exists: true,
                        size: 100,
                    },
                )
            })
            .collect();
        let plan = reconcile(&manifest, &desired, &local, &sources);
        let deleted: Vec<&str> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                suno_core::Action::Delete { clip_id, .. } => Some(clip_id.as_str()),
                _ => None,
            })
            .collect();
        // Only the orphan with no source area is deleted; the library-exclusive
        // file and the copy-protected shared clip survive.
        assert_eq!(deleted, vec!["gone-orphan"]);
    }

    // Test 9: a single failed (non-enumerated) area suppresses deletion for the
    // whole run, even when another area is armed and fully enumerated.
    #[test]
    fn a_failed_area_suppresses_deletion_for_the_run() {
        let areas = [
            area(AreaKind::Liked, SourceMode::Mirror, &["a"], true),
            // Playlist listing failed: empty and non-authoritative.
            area(
                AreaKind::Playlist {
                    id: "p".into(),
                    name: "P".into(),
                },
                SourceMode::Mirror,
                &[],
                false,
            ),
        ];
        let sources: Vec<SourceStatus> = areas
            .iter()
            .map(|a| SourceStatus {
                mode: area_mode(a, false),
                fully_enumerated: area_enumerated(a, false),
            })
            .collect();
        assert!(!deletion_allowed(&sources));
    }

    // Test 8: with every area enumerated, a mixed Mirror + Copy selection deletes
    // only orphans exclusive to a Mirror area; a Copy area's orphan is protected
    // and the run remains armed.
    #[test]
    fn mixed_mode_deletes_only_mirror_exclusive_orphans() {
        use suno_core::{LocalFile, Manifest, ManifestEntry, reconcile};

        let areas = vec![
            area(AreaKind::Liked, SourceMode::Mirror, &["m-live"], true),
            area(
                AreaKind::Playlist {
                    id: "p".into(),
                    name: "P".into(),
                },
                SourceMode::Copy,
                &["c-live"],
                true,
            ),
        ];
        let sources: Vec<SourceStatus> = areas
            .iter()
            .map(|a| SourceStatus {
                mode: area_mode(a, false),
                fully_enumerated: area_enumerated(a, false),
            })
            .collect();
        assert!(deletion_allowed(&sources));

        let area_modes: Vec<(SourceMode, Vec<String>)> = areas
            .iter()
            .map(|a| {
                (
                    area_mode(a, false),
                    a.clips.iter().map(|c| c.id.clone()).collect(),
                )
            })
            .collect();
        let modes = build_modes_by_id(&area_modes);
        let union = union_clips(&areas);
        let desired = build_desired(
            &union.iter().collect::<Vec<_>>(),
            suno_core::AudioFormat::Flac,
            &modes,
            &HashMap::new(),
            &BTreeSet::new(),
            ArtifactToggles::default(),
            &suno_core::NamingConfig::default(),
        );

        let mut manifest = Manifest::new();
        // Orphans: one previously from the mirror area, one from the copy area.
        for id in ["m-live", "c-live", "m-orphan", "c-orphan"] {
            manifest.insert(
                id,
                ManifestEntry {
                    path: format!("{id}.flac"),
                    format: suno_core::AudioFormat::Flac,
                    size: 100,
                    // The copy-area orphan carries the preserve marker a prior copy
                    // run stamped, so it can never be deleted.
                    preserve: id == "c-orphan",
                    ..Default::default()
                },
            );
        }
        let local: HashMap<String, LocalFile> = manifest
            .iter()
            .map(|(id, _)| {
                (
                    id.clone(),
                    LocalFile {
                        exists: true,
                        size: 100,
                    },
                )
            })
            .collect();
        let plan = reconcile(&manifest, &desired, &local, &sources);
        let deleted: Vec<&str> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                suno_core::Action::Delete { clip_id, .. } => Some(clip_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deleted, vec!["m-orphan"]);
    }
}
