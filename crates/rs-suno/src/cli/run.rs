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
use std::io::IsTerminal;

use anyhow::{Context, Result};
use suno_core::select::{RecencySpec, SelectParams, select};
use suno_core::{
    ArtifactToggles, ClerkAuth, Config, FlagOverrides, LineageContext, NamingConfig, OwnerGate,
    PlaylistState, ResolveOpts, SourceMode, SunoClient, adopt_decision, adoption_enumerated,
    album_desired, area_mode, build_desired, build_modes_by_id, build_scoped_playlist_desired,
    clip_stems, deletion_allowed, library_authoritative, narrows_downloads, owner_gate,
    resolve_roots, source_statuses, union_clips,
};

use crate::cli::account;
use crate::cli::areas;
use crate::cli::args::{GlobalArgs, SyncArgs};
use crate::cli::commands::version;
use crate::cli::config_load;
use crate::cli::desired::{
    Confirm, ExitCode, confirm_decision, is_narrowed, mass_delete_abort, resolve_selection, worse,
};
use crate::cli::execute;
use crate::cli::failure;
use crate::cli::identity::{Identity, IdentityContext, IdentityOutcome};
use crate::cli::last_run;
use crate::cli::logs;
use crate::cli::output;
use crate::cli::prompt;
use crate::cli::stems;
use crate::cli::synced_lyrics;
use crate::cli::task_output;
use crate::cli::task_output::eprint_t;
use crate::cli::token;
use crate::cli::wallclock;
use crate::clock::TokioClock;
use crate::http::ReqwestHttp;

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
    let mut identity = Identity::default();
    let identity_ctx = IdentityContext {
        configured_id: settings.account_id.as_deref(),
        user_id: &user_id,
        account: &account,
        dest,
        allow_account_change: args.allow_account_change,
        verbosity,
    };

    // PHASE 1: decide identity with no network via the pure gate, then apply
    // the side-effects (pin, refresh, abort) via `identity`.
    let gate = owner_gate(
        store.owner(),
        settings.account_id.as_deref(),
        &user_id,
        args.allow_account_change,
    );
    match identity.apply_owner_gate(&mut store, gate, &identity_ctx) {
        IdentityOutcome::Abort { code, message } => {
            eprint_t!("{message}");
            return Ok(code);
        }
        IdentityOutcome::Continue { notice } => {
            if let Some(notice) = notice {
                eprint_t!("{notice}");
            }
        }
    }

    let client = SunoClient::new(auth, TokioClock);

    // Resolve which areas this run touches and their modes (pure). CLI scope
    // flags win over `[areas]` config; a copy verb or a force-additive run
    // rewrites every mode to Copy. When any Mirror area is armed and the library
    // is neither explicitly selected nor `"off"`, an implicit full-library copy
    // protector is injected so a Mirror area can never delete a library-exclusive
    // file (D1).
    let force_copy_initial = verb == Verb::Copy || identity.force_additive();
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
    let areas = match areas::enumerate_areas(
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
        if let IdentityOutcome::Abort { code, message } =
            identity.apply_adopt_decision(&mut store, decision, &identity_ctx)
        {
            eprint_t!("{message}");
            return Ok(code);
        }
    }

    // Assemble the final per-area view now the run's additivity is known. A copy
    // verb or a force-additive run (re-pin/adopt) rewrites every area to Copy, so
    // no Mirror source remains and deletion is impossible; the protector already
    // never armed anything.
    let force_copy = verb == Verb::Copy || identity.force_additive();
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
        last_run: last_run::read_last_run(dest),
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
    let stems_by_id = stems::list_existing_stems(
        settings.download_stems,
        &selected,
        &client,
        &http,
        settings.concurrency,
    )
    .await;
    if settings.download_stems {
        for d in &mut desired {
            let base = stems::strip_format_ext(&d.path, settings.format);
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
            areas::fetch_playlist_desired(
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
        let plan = execute::reconcile_run(
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
            let orphans = suno_core::untracked_audio(&manifest, &execute::walk_audio_files(dest));
            if !orphans.is_empty() {
                eprint_t!("{}", output::orphan_report(&orphans));
            }
        }
        if verb == Verb::Check && exit_code && prompt::plan_has_changes(&plan) {
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
    let (synced, pending_checks) = synced_lyrics::resolve_synced_lyrics(
        &mut desired,
        &manifest,
        &client,
        &http,
        settings.lrc_sidecar,
        verbosity,
        settings.concurrency,
    )
    .await;
    let plan = execute::reconcile_run(
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
    if graph_changed || identity.owner_dirty() {
        logs::save_graph(dest, &store)?;
    }
    // Announce and audit an actual pin only now, on the executing path, so a
    // notice is never printed for a pin that check/dry-run would not persist
    // (F1). The full id goes to the audit file, never to stderr.
    if let Some(pin) = identity.pending_pin() {
        if verbosity >= -1 {
            eprint_t!("{}", pin.notice);
        }
        if let Some(owner) = store.owner() {
            logs::append_owner_pin(dest, pin.action, &owner.user_id, &owner.display_name)?;
        }
    }

    let is_sync = verb == Verb::Sync && !identity.force_additive();
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
            if !prompt::prompt_delete(&plan, verbosity)? {
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

    execute::execute_plan(
        verb.summary_label(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use suno_core::{AreaKind, AreaListing, Clip, area_enumerated};

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
        use suno_core::{LocalFile, Manifest, ManifestEntry, SourceStatus, reconcile};

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
