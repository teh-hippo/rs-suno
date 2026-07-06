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
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::stream::{self, StreamExt};
use suno_core::select::{RecencySpec, SelectParams, select};
use suno_core::{
    AdoptDecision, AlbumArt, AlbumDesired, AlignedLyrics, AreaKind, AreaListing, ArtifactToggles,
    ClerkAuth, Clip, Config, ExecOptions, Filesystem, FlagOverrides, LIKED_PLAYLIST_ID,
    LineageContext, LocalFile, Manifest, NamingConfig, Owner, OwnerGate, PlaylistDesired,
    PlaylistInput, PlaylistState, Ports, ResolveOpts, RunStatus, SourceMode, SourceStatus, Stem,
    SunoClient, adopt_decision, adoption_enumerated, album_desired, area_mode, build_desired,
    build_modes_by_id, build_playlist_desired, build_scoped_playlist_desired, clip_stems,
    deletion_allowed, is_downloadable, library_authoritative, narrows_downloads, owner_gate,
    plan_album_artifacts, plan_playlist_artifacts, reconcile, resolve_roots, source_statuses,
    union_clips,
};

use crate::cli::account;
use crate::cli::args::{GlobalArgs, SyncArgs};
use crate::cli::commands::version;
use crate::cli::config_load;
use crate::cli::desired::{
    Confirm, ExitCode, PlaylistPolicy, ResolvedSelection, confirm_decision, confirmed, is_narrowed,
    mass_delete_abort, resolve_playlist, resolve_selection, run_exit_code, worse,
};
use crate::cli::failure;
use crate::cli::logs;
use crate::cli::output;
use crate::cli::task_output;
use crate::cli::task_output::eprint_t;
use crate::cli::token;
use crate::cli::wallclock;
use crate::clock::TokioClock;
use crate::download::cleanup_stale_parts;
use crate::ffmpeg::FfmpegAdapter;
use crate::fs::FsAdapter;
use crate::http::ReqwestHttp;

const WAV_POLL_ATTEMPTS: u32 = 24;
const WAV_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How many deletion paths the confirmation prompt lists before summarising.
const PROMPT_PATH_LIMIT: usize = 3;
const LAST_RUN_NAME: &str = ".suno-last-run";
/// Maximum number of accounts processed concurrently when `--all` targets
/// multiple accounts. Accounts share no mutable state (separate clients,
/// tokens, destination roots, manifests, and lineage files), so per-account
/// isolation is what makes this data-safe: each account's serial commit and
/// deletion-safety logic is entirely unaffected by the concurrency between
/// accounts.
const ACCOUNT_CONCURRENCY: usize = 4;

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

async fn run(
    verb: Verb,
    global: &GlobalArgs,
    args: &SyncArgs,
    exit_code: bool,
) -> Result<ExitCode> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let token_available = token::token_available(global, &env);

    let config = match config_load::load_config(global.config.as_deref())? {
        config_load::ConfigState::Loaded(cfg) => Some(cfg),
        config_load::ConfigState::Absent => None,
        config_load::ConfigState::Error(message) => {
            eprintln!("error: {message}");
            return Ok(ExitCode::Config);
        }
    };

    let sel = account::Selection {
        all: global.all,
        account: global.account.as_deref(),
        dest: args.dest.as_deref(),
        token_available,
    };
    let targets = match account::plan_targets(config.as_ref(), &sel) {
        Ok(targets) => targets,
        Err(message) => {
            eprintln!("error: {message}");
            return Ok(ExitCode::Config);
        }
    };

    let mut worst = ExitCode::Ok;
    if targets.len() <= 1 {
        // Single account: sequential with streaming output (unchanged behaviour).
        let flags = config_load::flag_overrides(global, args);
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
                task_output::capture_task_stderr();
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                let flags = config_load::flag_overrides(&g, &a);
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
                let lines = task_output::flush_task_stderr();
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

/// Strip the resolved format's extension from an audio path, giving the
/// extensionless base the sidecars and the `.stems` folder are built from.
/// Falls back to the whole path if the extension is somehow absent.
fn strip_format_ext(path: &str, format: suno_core::AudioFormat) -> &str {
    path.strip_suffix(&format!(".{}", format.ext()))
        .unwrap_or(path)
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
    target: &account::TargetSpec,
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
            account::synthetic_config().resolve("default", None, env, flags)
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

    let token = match token::resolve_token(&target.label, &settings).await {
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
        return Ok(failure::report_auth_failure(&target.label, &err));
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
        store.update(&clips, resolution, &wallclock::now_rfc3339());
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
                area.clips().iter().map(|clip| clip.id.clone()).collect(),
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
        now: wallclock::now_secs(),
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
            webp: settings.animated_cover_webp,
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
            settings.animated_cover_webp,
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
        suno_core::preview_synced_lrc(
            &mut desired,
            &manifest,
            wallclock::now_secs(),
            settings.lrc_sidecar,
        );
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
    let targets =
        suno_core::synced_lyrics_targets(desired, manifest, wallclock::now_secs(), enabled);
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
    let now = wallclock::now_secs();
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
                Ok((clips, complete, any_filtered)) => areas.push(AreaListing::listed(
                    AreaKind::Library,
                    lib.mode,
                    clips,
                    complete,
                    any_filtered,
                    // The protector ignores `--limit`/`--since` (narrowed=false)
                    // but still disarms on filter loss (#248).
                    false,
                )),
                Err(err) => {
                    if verbosity >= -1 {
                        eprint_t!(
                            "warning: library listing failed ({err}); suppressing deletion this run"
                        );
                    }
                    areas.push(AreaListing::failed(AreaKind::Library, lib.mode));
                }
            }
        } else {
            // Plain Library run: honours `--limit`, and a listing failure aborts
            // exactly as today (the run has no other data source).
            match client.list_clips(http, false, args.limit).await {
                Ok((clips, complete, any_filtered)) => areas.push(AreaListing::listed(
                    AreaKind::Library,
                    lib.mode,
                    clips,
                    complete,
                    any_filtered,
                    narrowed,
                )),
                Err(err) => return Err(failure::report_listing_failure(label, &err)),
            }
        }
    }

    if let Some(mode) = selection.liked {
        match client.list_clips(http, true, None).await {
            Ok((clips, complete, any_filtered)) => areas.push(AreaListing::listed(
                AreaKind::Liked,
                mode,
                clips,
                complete,
                any_filtered,
                narrowed,
            )),
            Err(err) => {
                if verbosity >= -1 {
                    eprint_t!(
                        "warning: liked feed failed to list ({err}); suppressing deletion this run"
                    );
                }
                areas.push(AreaListing::failed(AreaKind::Liked, mode));
            }
        }
    }

    if !matches!(selection.playlists, PlaylistPolicy::None) {
        // Resolve names and enumerate the `All` group via the account's playlists.
        let playlists = match client.get_playlists(http).await {
            Ok(playlists) => Some(playlists),
            Err(err) => {
                if selection.cli_scoped {
                    return Err(failure::report_listing_failure(label, &err));
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
                            areas.push(AreaListing::unresolved_playlist(*mode));
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
                    areas.push(AreaListing::unresolved_playlist(*mode));
                }
            }
            (PlaylistPolicy::All { default, .. }, None) => {
                areas.push(AreaListing::unresolved_playlist(*default));
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
            AreaListing::listed(
                AreaKind::Playlist {
                    id: id.to_owned(),
                    name: name.to_owned(),
                },
                mode,
                clips,
                complete,
                any_filtered,
                narrowed,
            )
        }
        Err(err) => {
            if verbosity >= -1 {
                eprint_t!(
                    "warning: playlist '{name}' members failed to list ({err}); suppressing deletion this run"
                );
            }
            AreaListing::failed(
                AreaKind::Playlist {
                    id: id.to_owned(),
                    name: name.to_owned(),
                },
                mode,
            )
        }
    }
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
        Ok((liked, true, _)) => {
            fetched.push((
                LIKED_PLAYLIST_ID.to_owned(),
                "Liked Songs".to_owned(),
                liked,
            ));
        }
        Ok((_, false, _)) => {
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

fn read_last_run(dest: &Path) -> Option<u64> {
    std::fs::read_to_string(dest.join(LAST_RUN_NAME))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write_last_run(dest: &Path) {
    let _ = std::fs::write(dest.join(LAST_RUN_NAME), wallclock::now_secs().to_string());
}

/// Resolve when a SIGINT (Ctrl-C) or, on Unix, a SIGTERM arrives.
///
/// `ctrl_c` is cross-platform; the extra `SIGTERM` arm is Unix-only because
/// Windows has no such signal.
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
    use std::path::PathBuf;
    use suno_core::area_enumerated;

    #[test]
    fn last_run_marker_round_trips() {
        let dir = Path::new("target").join(format!(
            "run-last-run-{}-{}",
            std::process::id(),
            wallclock::now_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        write_last_run(&dir);
        assert!(read_last_run(&dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reconcile_run_reads_a_missing_destination_as_empty() {
        // The dry-run / check path reads through a missing destination as an
        // empty manifest without creating it, so it never touches disk.
        let dir = Path::new("target").join(format!(
            "run-nodir-{}-{}",
            std::process::id(),
            wallclock::now_secs()
        ));
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
        let target = account::TargetSpec {
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
        let target = account::TargetSpec {
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
        AreaListing::listed(
            kind,
            mode,
            ids.iter().map(|id| tclip(id)).collect(),
            authoritative,
            false,
            false,
        )
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
                    a.clips().iter().map(|c| c.id.clone()).collect(),
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
}
