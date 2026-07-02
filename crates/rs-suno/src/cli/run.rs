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
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use suno_core::select::{RecencySpec, SelectParams, select};
use suno_core::{
    AdoptDecision, AlbumArt, AlbumDesired, ClerkAuth, Clip, Config, Error as CoreError,
    ExecOptions, Filesystem, FlagOverrides, LineageContext, LocalFile, Owner, OwnerGate,
    PlaylistDesired, PlaylistState, Ports, ResolveOpts, RunStatus, SourceMode, SourceStatus,
    SunoClient, adopt_decision, album_desired, deletion_allowed, is_downloadable, owner_gate,
    plan_album_artifacts, plan_playlist_artifacts, reconcile, resolve_roots,
};

use crate::cli::args::{GlobalArgs, SyncArgs};
use crate::cli::desired::{
    ArtifactToggles, Confirm, ExitCode, LIKED_PLAYLIST_ID, PlaylistInput, build_desired,
    build_playlist_desired, confirm_decision, confirmed, dedup_clips_by_id, fully_enumerated,
    is_narrowed, is_scoped, mass_delete_abort, resolve_playlist, run_exit_code,
};
use crate::cli::logs;
use crate::cli::output;
use crate::clock::TokioClock;
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
                    "no account configured and no token provided; pass --token or run 'suno config init'"
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
    let token_available = global.token.is_some() || env.contains_key("SUNO_TOKEN");

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

    let flags = flag_overrides(global, args);
    let mut worst = ExitCode::Ok;
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
    let token_available = global.token.is_some() || env.contains_key("SUNO_TOKEN");
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
                        "no account configured and no token provided; pass --token".to_owned()
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
        details_sidecar: args.details_sidecar.then_some(true),
        lyrics_sidecar: args.lyrics_sidecar.then_some(true),
        lrc_sidecar: args.lrc_sidecar.then_some(true),
    }
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
        eprintln!(
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
                eprintln!("error: {err}");
                return Ok(ExitCode::Config);
            }
        }
    };

    let Some(token) = settings.token.clone() else {
        eprintln!(
            "error: no token for account '{}'; pass --token or set it in config",
            target.label
        );
        return Ok(ExitCode::Config);
    };

    if settings.format == suno_core::AudioFormat::Wav && verbosity >= -1 {
        eprintln!(
            "warning: WAV carries limited metadata; lyrics and album art will be omitted (use flac or mp3 for full tags)"
        );
    }

    let http = ReqwestHttp::new().context("failed to build the HTTP client")?;
    let dest = &target.dest;
    let mut auth = ClerkAuth::new(&token);
    if let Err(err) = auth.authenticate(&http).await {
        return Ok(report_auth_failure(&target.label, &err));
    }
    let account = auth.display_name().to_owned();
    crate::cli::expiry::warn_token_expiry(&target.label, &auth, verbosity);
    // Fail closed: the identity guard cannot run without an authenticated id,
    // and proceeding would delete against an unverified account. authenticate()
    // already errors on a missing id; this makes the invariant explicit.
    let Some(user_id) = auth.user_id().map(str::to_owned) else {
        eprintln!(
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
            eprintln!(
                "error: the configured account_id ({}) does not match the authenticated account (id {}). Refusing to run to protect the library.",
                short_id(settings.account_id.as_deref().unwrap_or_default()),
                short_id(&user_id)
            );
            return Ok(ExitCode::Safety);
        }
        OwnerGate::AbortMismatch => {
            let pinned = store.owner().expect("mismatch implies a pinned owner");
            eprintln!(
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
            store.pin_owner(Owner {
                user_id: user_id.clone(),
                display_name: account.clone(),
            });
            owner_dirty = true;
            pending_pin = Some(PendingPin {
                action: "REPIN",
                notice: format!(
                    "notice: re-pinned this library from {} to {} (id {}); this run is additive (no deletions). Run 'sync' again to mirror.",
                    previous,
                    account,
                    short_id(&user_id)
                ),
            });
        }
        OwnerGate::Proceed => {
            if store.refresh_display_name(&account) {
                owner_dirty = true;
            }
            if args.allow_account_change && verbosity >= 0 {
                eprintln!(
                    "notice: --allow-account-change had no effect; this library already belongs to {} (id {}).",
                    account,
                    short_id(&user_id)
                );
            }
        }
        OwnerGate::FirstUse => {}
    }

    let mut client = SunoClient::new(auth, TokioClock);

    // A scoped run (`--liked` and/or `--playlist`) lists only a subset of the
    // library, so it can never delete: `enumerated` below folds `scoped` into the
    // same not-fully-enumerated verdict a `--limit`/`--since` narrowing uses.
    let scoped = is_scoped(args.liked, &args.playlist);
    let (clips, complete) = if scoped {
        match list_scoped_clips(&mut client, &http, &target.label, args, verbosity).await {
            ScopedListing::Clips(clips) => (clips, false),
            ScopedListing::Empty => {
                if verbosity >= -1 {
                    eprintln!(
                        "notice: nothing to do; the requested scope holds no downloadable clips."
                    );
                }
                return Ok(ExitCode::Ok);
            }
            ScopedListing::Failed(code) => return Ok(code),
        }
    } else {
        match client.list_clips(&http, false, args.limit).await {
            Ok(result) => result,
            Err(err) => return Ok(report_listing_failure(&target.label, &err)),
        }
    };

    // Resolve every listed clip's root ancestor (roots need the whole set as
    // the universe). Resolution is best-effort: a hard IO failure degrades to
    // the last-known-good roots already in the durable store rather than
    // aborting the sync or rewriting the library from a dropped call (H3).
    let resolution = match resolve_roots(&clips, &mut client, &http, ResolveOpts::default()).await {
        Ok(resolution) => Some(resolution),
        Err(err) => {
            if verbosity >= -1 {
                eprintln!(
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
    let narrowed = is_narrowed(args.limit, args.since.as_deref());
    let enumerated = fully_enumerated(complete, narrowed || scoped);

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
                store.pin_owner(Owner {
                    user_id: user_id.clone(),
                    display_name: account.clone(),
                });
                owner_dirty = true;
                pending_pin = Some(PendingPin {
                    action: "PIN",
                    notice: format!(
                        "notice: pinned this library to {} (id {}).",
                        account,
                        short_id(&user_id)
                    ),
                });
            }
            AdoptDecision::PinAdopt => {
                store.pin_owner(Owner {
                    user_id: user_id.clone(),
                    display_name: account.clone(),
                });
                owner_dirty = true;
                pending_pin = Some(PendingPin {
                    action: "ADOPT",
                    notice: format!(
                        "notice: adopted this existing library for {} (id {}).",
                        account,
                        short_id(&user_id)
                    ),
                });
            }
            AdoptDecision::AdoptForced => {
                store.pin_owner(Owner {
                    user_id: user_id.clone(),
                    display_name: account.clone(),
                });
                owner_dirty = true;
                pending_pin = Some(PendingPin {
                    action: "ADOPT",
                    notice: format!(
                        "notice: adopted this library for {} (id {}) despite no overlap; this run is additive (no deletions). Run 'sync' again to mirror.",
                        account,
                        short_id(&user_id)
                    ),
                });
            }
            AdoptDecision::Abort => {
                eprintln!(
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

    // A re-pin run (Mismatch + --allow-account-change) must never delete the
    // previous account's files this invocation, so it runs additively (Copy
    // semantics) regardless of the verb; a subsequent normal sync, now pinned
    // to the new account, will mirror.
    let mode = if force_additive {
        SourceMode::Copy
    } else if let Some(configured) = settings.mode {
        configured
    } else {
        verb.mode()
    };

    let since = match args.since.as_deref().map(RecencySpec::parse).transpose() {
        Ok(since) => since,
        Err(message) => {
            eprintln!("error: {message}");
            return Ok(ExitCode::Config);
        }
    };
    let params = SelectParams {
        limit: args.limit,
        since,
        min_newest: settings.min_newest as usize,
        now: now_secs(),
        last_run: read_last_run(dest),
    };
    let selected = select(&clips, &params);
    let contexts: HashMap<String, LineageContext> = selected
        .iter()
        .map(|clip| (clip.id.clone(), store.context_for(clip)))
        .collect();
    let desired = build_desired(
        &selected,
        settings.format,
        mode,
        &contexts,
        &colliding_albums,
        ArtifactToggles {
            animated_covers: settings.animated_covers,
            details: settings.details_sidecar,
            lyrics: settings.lyrics_sidecar,
            lrc: settings.lrc_sidecar,
        },
    );
    // Folder-level album art is keyed on the stable root id and chosen purely
    // from the selected clips (most-played for folder.jpg, first-created animated
    // for cover.webp); --animated-covers gates the webp.
    let albums_desired = album_desired(&desired, settings.animated_covers);

    // Playlists (.m3u8) are reconciled only on a fully-enumerated run: a narrowed
    // or truncated audio listing cannot authoritatively render a playlist (its
    // members outside the selection would look absent), so leave every existing
    // .m3u8 untouched rather than rewrite it to a comment-only stub (B2 spirit).
    // Within a full run the fetch is best-effort per HARDENING B2: a failed
    // /api/playlist/me listing yields an empty desired and playlists_enumerated
    // = false (no writes, no deletes); a failed single-playlist member fetch adds
    // that id to `protected`, excluding it from BOTH the desired writes and the
    // stale-delete candidate set so its file is neither rewritten nor removed.
    let mut protected_playlists: BTreeSet<String> = BTreeSet::new();
    let (playlist_desired, playlists_enumerated) = if enumerated {
        fetch_playlist_desired(
            &mut client,
            &http,
            &desired,
            &mut protected_playlists,
            verbosity,
        )
        .await
    } else {
        (Vec::new(), false)
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
    // created and no lock is taken. A missing manifest reads as empty.
    if dry_run {
        let (_manifest, plan) = load_and_reconcile(
            dest,
            &desired,
            &albums_desired,
            &store.albums,
            &playlist_desired,
            &stored_playlists,
            enumerated,
            playlists_enumerated,
            mode,
        )?;
        if verbosity >= 1 {
            let no_failures = HashSet::new();
            for line in output::action_lines(&plan, &no_failures, verbosity) {
                eprintln!("{line}");
            }
        }
        if verbosity >= -1 {
            eprintln!("{}", output::dry_summary(&account, &plan));
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
    let (manifest, plan) = load_and_reconcile(
        dest,
        &desired,
        &albums_desired,
        &store.albums,
        &playlist_desired,
        &stored_playlists,
        enumerated,
        playlists_enumerated,
        mode,
    )?;

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
            eprintln!("{}", pin.notice);
        }
        if let Some(owner) = store.owner() {
            logs::append_owner_pin(dest, pin.action, &owner.user_id, &owner.display_name)?;
        }
    }

    let is_sync = verb == Verb::Sync && !force_additive;
    // The mass-delete cap counts every destructive action, audio and sidecar
    // alike (HARDENING B2), so a run that would mass-delete artifacts aborts too.
    let delete_count = plan.deletes() + plan.artifact_deletes();
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
        eprintln!(
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
                eprintln!("Aborted; no changes made.");
                return Ok(ExitCode::Ok);
            }
        }
        Confirm::RefuseNonInteractive => {
            eprintln!(
                "error: sync would delete {} file(s) but stdin is not a TTY and --yes was not passed\n  Pass --yes to confirm, or use 'copy' to skip deletions.",
                delete_count
            );
            return Ok(ExitCode::Safety);
        }
    }

    if verbosity == 0 {
        eprintln!(
            "{}",
            output::progress_start(verb.progress_word(), &account, &plan)
        );
    }

    execute_plan(
        verb,
        &plan,
        &desired,
        manifest,
        &mut store,
        &mut client,
        &http,
        dest,
        &settings,
        &account,
        verbosity,
        enumerated,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_plan(
    verb: Verb,
    plan: &suno_core::Plan,
    desired: &[suno_core::Desired],
    mut manifest: suno_core::Manifest,
    store: &mut suno_core::LineageStore,
    client: &mut SunoClient<TokioClock>,
    http: &ReqwestHttp,
    dest: &Path,
    settings: &suno_core::EffectiveSettings,
    account: &str,
    verbosity: i8,
    enumerated: bool,
) -> Result<ExitCode> {
    let fs = FsAdapter::new(dest);
    let ffmpeg = FfmpegAdapter::new(dest);
    let clock = TokioClock;
    let opts = ExecOptions {
        max_retries: settings.retries,
        wav_poll_attempts: WAV_POLL_ATTEMPTS,
        wav_poll_interval: WAV_POLL_INTERVAL,
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
            out = suno_core::execute(plan, &mut manifest, &mut store.albums, &mut store.playlists, desired, ports, &opts) => Some(out),
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
        eprintln!(
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
            eprintln!(
                "{}",
                output::run_summary(
                    verb.summary_label(),
                    account,
                    &outcome,
                    started.elapsed().as_secs_f64()
                )
            );
        }
        eprintln!(
            "error: {} The library is unchanged for the failing action.",
            crate::diskspace::DISK_FULL_HINT
        );
        if let Some(last) = outcome.failures.last() {
            eprintln!("  {}", last.reason);
        }
        return Ok(ExitCode::DiskFull);
    }

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
    // manifest). Gated on `enumerated`, not playlist membership: a narrowed
    // `--limit`/`--since` run sees only a window of clips live, so it would null
    // the artist/tags/duration of every out-of-window clip and regress a richer
    // index from a prior full run; only a full run writes, avoiding that
    // live-field oscillation.
    if enumerated
        && let Err(err) = logs::save_index(dest, &manifest, store, &clips_by_id)
        && verbosity >= -1
    {
        eprintln!("warning: could not write {}: {err}", logs::INDEX_NAME);
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
    logs::append_audit(dest, plan, &failed, &rename_owner)?;
    write_last_run(dest);

    if verbosity >= 1 {
        for line in output::action_lines(plan, &failed, verbosity) {
            eprintln!("{line}");
        }
    }

    if !outcome.failures.is_empty() && verbosity >= -1 {
        eprintln!(
            "warning: {} clip(s) failed after retries\n  See {} for details.",
            outcome.failures.len(),
            dest.join(".suno-failures.log").display()
        );
    }
    if verbosity >= -1 {
        eprintln!(
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

/// The outcome of listing a scoped run's clips.
enum ScopedListing {
    /// The deduplicated union of every scoped source's downloadable clips.
    Clips(Vec<Clip>),
    /// The scope resolved but held no downloadable clips (an empty or
    /// fully-filtered playlist). The caller prints a notice and exits `Ok`; it
    /// must never fall through to a full-feed sync.
    Empty,
    /// A listing or resolution failure. Carries the exit code to return.
    Failed(ExitCode),
}

/// List the clips for a scoped run: the liked feed and/or named playlists.
///
/// `--liked` (and the `--playlist liked` alias) contributes the liked feed;
/// every other `--playlist` value is resolved against the account's own
/// non-trashed playlists ([`resolve_playlist`]) and its members are filtered
/// through [`is_downloadable`], since raw playlist members can include
/// streaming, infill, and artefact clips the feed path already screens out. The
/// sources are unioned and deduplicated by clip id, so a clip that appears in
/// several scopes is downloaded once.
///
/// An unknown or ambiguous `--playlist` value prints the resolution error and
/// the account's visible playlists, then fails with [`ExitCode::Config`] rather
/// than silently widening the run. An empty resolved scope returns
/// [`ScopedListing::Empty`] so a typo can never become a full sync.
async fn list_scoped_clips(
    client: &mut SunoClient<TokioClock>,
    http: &ReqwestHttp,
    label: &str,
    args: &SyncArgs,
    verbosity: i8,
) -> ScopedListing {
    // The `--playlist liked` alias unifies with the `--liked` synthetic source.
    let mut want_liked = args.liked;
    let mut playlist_values: Vec<&str> = Vec::new();
    for value in &args.playlist {
        if value == LIKED_PLAYLIST_ID {
            want_liked = true;
        } else {
            playlist_values.push(value.as_str());
        }
    }

    let mut union: Vec<Clip> = Vec::new();

    if want_liked {
        match client.list_clips(http, true, None).await {
            Ok((liked, _complete)) => union.extend(liked),
            Err(err) => return ScopedListing::Failed(report_listing_failure(label, &err)),
        }
    }

    if !playlist_values.is_empty() {
        let playlists = match client.get_playlists(http).await {
            Ok(playlists) => playlists,
            Err(err) => return ScopedListing::Failed(report_listing_failure(label, &err)),
        };
        for value in &playlist_values {
            let playlist = match resolve_playlist(value, &playlists) {
                Ok(playlist) => playlist,
                Err(err) => {
                    eprintln!("error: {err}.");
                    print_visible_playlists(&playlists, verbosity);
                    return ScopedListing::Failed(ExitCode::Config);
                }
            };
            match client.get_playlist_clips(http, &playlist.id).await {
                Ok(members) => union.extend(members.into_iter().filter(is_downloadable)),
                Err(err) => return ScopedListing::Failed(report_listing_failure(label, &err)),
            }
        }
    }

    let union = dedup_clips_by_id(union);
    if union.is_empty() {
        ScopedListing::Empty
    } else {
        ScopedListing::Clips(union)
    }
}

/// Print the account's own playlists to help a user correct a `--playlist` typo.
fn print_visible_playlists(playlists: &[suno_core::Playlist], verbosity: i8) {
    if verbosity < -1 {
        return;
    }
    if playlists.is_empty() {
        eprintln!("no playlists are visible for this account.");
        return;
    }
    eprintln!("visible playlists:");
    for playlist in playlists {
        eprintln!("  {} ({})", playlist.name, playlist.id);
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
    client: &mut SunoClient<TokioClock>,
    http: &ReqwestHttp,
    desired: &[suno_core::Desired],
    protected: &mut BTreeSet<String>,
    verbosity: i8,
) -> (Vec<PlaylistDesired>, bool) {
    let playlists = match client.get_playlists(http).await {
        Ok(playlists) => playlists,
        Err(err) => {
            if verbosity >= -1 {
                eprintln!(
                    "warning: playlist listing failed ({err}); leaving existing .m3u8 files untouched"
                );
            }
            return (Vec::new(), false);
        }
    };

    // Own each playlist's members so the borrowed `PlaylistInput`s stay valid.
    let mut fetched: Vec<(String, String, Vec<Clip>)> = Vec::new();
    for playlist in &playlists {
        match client.get_playlist_clips(http, &playlist.id).await {
            Ok(members) => fetched.push((playlist.id.clone(), playlist.name.clone(), members)),
            Err(err) => {
                if verbosity >= -1 {
                    eprintln!(
                        "warning: playlist '{}' members failed to list ({err}); keeping its .m3u8 unchanged",
                        playlist.name
                    );
                }
                protected.insert(playlist.id.clone());
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
                eprintln!("warning: liked feed was truncated; keeping Liked Songs.m3u8 unchanged");
            }
            protected.insert(LIKED_PLAYLIST_ID.to_owned());
        }
        Err(err) => {
            if verbosity >= -1 {
                eprintln!(
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

/// Load the manifest beside `dest` and reconcile `desired` against it, then
/// append the folder-art and playlist plans.
///
/// Shared by the dry-run and executing paths. Reading a missing manifest yields
/// an empty one and statting absent files is harmless, so this never creates the
/// destination directory. The folder-art actions share the run's single deletion
/// verdict ([`deletion_allowed`]) so album art is never removed on an incomplete
/// listing, and they land on the same [`Plan`] so the mass-delete cap and the
/// confirmation prompt already cover them.
///
/// Playlists carry a second, independent gate: `playlists_enumerated` is true
/// only when the `/api/playlist/me` listing succeeded on a fully-enumerated run.
/// [`plan_playlist_artifacts`] emits a playlist delete only when BOTH the shared
/// `can_delete` verdict and `playlists_enumerated` hold, so a failed, empty, or
/// partial playlist listing never removes an existing `.m3u8` (HARDENING B2).
/// These deletes also count toward the mass-delete cap via [`Plan::artifact_deletes`].
#[allow(clippy::too_many_arguments)]
fn load_and_reconcile(
    dest: &Path,
    desired: &[suno_core::Desired],
    albums_desired: &[AlbumDesired],
    albums: &BTreeMap<String, AlbumArt>,
    playlist_desired: &[PlaylistDesired],
    playlists: &BTreeMap<String, PlaylistState>,
    enumerated: bool,
    playlists_enumerated: bool,
    mode: SourceMode,
) -> Result<(suno_core::Manifest, suno_core::Plan)> {
    let manifest = logs::load_manifest(dest)?;
    let local = stat_manifest(dest, &manifest);
    let sources = vec![SourceStatus {
        mode,
        fully_enumerated: enumerated,
    }];
    let can_delete = deletion_allowed(&sources);
    let mut plan = reconcile(&manifest, desired, &local, &sources);
    plan.actions
        .extend(plan_album_artifacts(albums_desired, albums, can_delete));
    plan.actions.extend(plan_playlist_artifacts(
        playlist_desired,
        playlists,
        can_delete,
        playlists_enumerated,
    ));
    Ok((manifest, plan))
}

/// Stat every manifest path so reconcile can spot missing or empty files.
fn stat_manifest(dest: &Path, manifest: &suno_core::Manifest) -> HashMap<String, LocalFile> {
    manifest
        .iter()
        .map(|(clip_id, entry)| {
            let stat = std::fs::metadata(dest.join(&entry.path)).ok();
            let local = LocalFile {
                exists: stat.is_some(),
                size: stat.map(|m| m.len()).unwrap_or(0),
            };
            (clip_id.clone(), local)
        })
        .collect()
}

/// True when the plan would change disk (anything but skips).
fn plan_has_changes(plan: &suno_core::Plan) -> bool {
    plan.downloads()
        + plan.reformats()
        + plan.retags()
        + plan.renames()
        + plan.deletes()
        + plan.artifact_writes()
        + plan.artifact_deletes()
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
            | suno_core::Action::DeleteArtifact { path, .. } => Some(path.clone()),
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
    eprintln!(
        "error: authentication failed for account '{label}'\n\nThe stored token may have expired. Re-authenticate with:\n  suno auth refresh {label}\n\nIf the token was rotated in Suno, update it with:\n  suno config add-account {label} --token <new-token>"
    );
    let _ = err;
    ExitCode::Auth
}

pub(crate) fn report_listing_failure(label: &str, err: &CoreError) -> ExitCode {
    match err {
        CoreError::Auth(_) => report_auth_failure(label, err),
        CoreError::Connection(_) | CoreError::RateLimited { .. } => {
            eprintln!(
                "error: could not list the library for '{label}': {err}\n  No files were written. Re-run when connectivity is restored."
            );
            ExitCode::Transient
        }
        other => {
            eprintln!("error: could not list the library for '{label}': {other}");
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

/// Pick the more severe of two exit codes (`Ok` is least severe).
fn worse(a: ExitCode, b: ExitCode) -> ExitCode {
    if b.code() >= a.code() { b } else { a }
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
        let _ = std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(PRIVATE_STATE_FILE_MODE),
        );
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

    #[test]
    fn load_and_reconcile_does_not_create_the_destination() {
        // The dry-run / check path reads through a missing destination as an
        // empty manifest without creating it, so it never touches disk.
        let dir =
            Path::new("target").join(format!("run-nodir-{}-{}", std::process::id(), now_secs()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!dir.exists());
        let (manifest, plan) = load_and_reconcile(
            &dir,
            &[],
            &[],
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
            false,
            false,
            SourceMode::Mirror,
        )
        .unwrap();
        assert!(manifest.is_empty());
        assert!(plan.actions.is_empty());
        assert!(
            !dir.exists(),
            "dry-run path must not create the destination directory"
        );
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
}
