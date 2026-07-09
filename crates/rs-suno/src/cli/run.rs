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
use std::path::Path;

use anyhow::{Context, Result};
use suno_core::select::{RecencySpec, SelectParams, select};
use suno_core::{
    AlbumArt, ArtifactToggles, ClerkAuth, Config, FlagOverrides, LineageContext, NamingConfig,
    OwnerGate, PlaylistState, ResolveOpts, SourceMode, SunoClient, adopt_decision,
    adoption_enumerated, album_desired, assign_track_numbers, build_desired, build_modes_by_id,
    build_scoped_playlist_desired, clip_stems, deletion_allowed, library_authoritative,
    narrows_downloads, owner_gate, resolve_lead_ids, resolve_roots, source_statuses, union_clips,
};

use crate::cli::account;
use crate::cli::areas;
use crate::cli::args::{GlobalArgs, SyncArgs};
use crate::cli::commands::version;
use crate::cli::config_load;
use crate::cli::desired::{
    Confirm, ExitCode, ResolvedSelection, confirm_decision, is_narrowed, mass_delete_abort,
    resolve_selection, worse,
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

mod assemble;
mod preflight;
mod tail;

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
        // Single account: sequential with streaming output.
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
        // per-account buffered output flushed atomically on completion. The
        // per-account isolation that makes this data-safe is documented on
        // ACCOUNT_CONCURRENCY.
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

    // Resolve settings, authenticate, and load the durable store before a single
    // feed request (the identity guard runs against it below).
    let Preflight {
        settings,
        http,
        client,
        mut store,
        user_id,
        account,
    } = match preflight::preflight(target, config, flags, env, verbosity).await? {
        Ok(pre) => pre,
        Err(code) => return Ok(code),
    };
    let dest = &target.dest;

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

    let ctx = RunCtx {
        verb,
        global,
        args,
        settings: &settings,
        client: &client,
        http: &http,
        dest,
        account: &account,
        verbosity,
        exit_code,
    };

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

    // Assemble the reconcile inputs now the run's additivity is known. A copy
    // verb or a force-additive run (re-pin/adopt) rewrites every area to Copy,
    // so no Mirror source remains and deletion is impossible; the protector
    // already never armed anything.
    let force_copy = verb == Verb::Copy || identity.force_additive();
    let mut assembled = match assemble::assemble(
        &ctx,
        &areas,
        &clips,
        &store,
        &selection,
        force_copy,
        graph_changed,
    )
    .await
    {
        Ok(assembled) => assembled,
        Err(code) => return Ok(code),
    };

    // Dry-run and check report without touching disk; the executing run takes
    // the lock and commits. Both open with the same reconcile against the
    // manifest they load.
    if global.dry_run || verb == Verb::Check {
        tail::dry_run_report(&ctx, &mut assembled, &store).await
    } else {
        tail::execute_run(&ctx, assembled, &mut store, &identity).await
    }
}

/// The immutable context of a single account's run: the invocation flags plus
/// the resolved settings, authenticated client, and destination from
/// [`preflight`]. Built once and shared by [`assemble`] and the two run-mode
/// tails so each takes one context instead of a dozen positional arguments.
struct RunCtx<'a> {
    verb: Verb,
    global: &'a GlobalArgs,
    args: &'a SyncArgs,
    settings: &'a suno_core::EffectiveSettings,
    client: &'a SunoClient<TokioClock>,
    http: &'a ReqwestHttp,
    dest: &'a Path,
    account: &'a str,
    verbosity: i8,
    exit_code: bool,
}

/// The authenticated, IO-ready context produced by [`preflight`] before any
/// feed request: the resolved settings, HTTP adapter and Suno client, the
/// loaded lineage store, and the authenticated account's id and display name.
struct Preflight {
    settings: suno_core::EffectiveSettings,
    http: ReqwestHttp,
    client: SunoClient<TokioClock>,
    store: suno_core::LineageStore,
    user_id: String,
    account: String,
}

/// The plan inputs [`assemble`] produces once the run's additivity is known:
/// the selected desired set plus the folder-art and playlist desired state and
/// the deletion gates the reconcile and executor read.
struct Assembled {
    desired: Vec<suno_core::Desired>,
    albums_desired: Vec<suno_core::AlbumDesired>,
    playlist_desired: Vec<suno_core::PlaylistDesired>,
    stored_playlists: BTreeMap<String, PlaylistState>,
    sources: Vec<suno_core::SourceStatus>,
    library_authoritative: bool,
    playlists_enumerated: bool,
    graph_changed: bool,
}

impl Assembled {
    /// Borrow this run's reconcile inputs, pairing the assembled desired state
    /// with the per-run manifest, destination, and album art.
    fn reconcile_inputs<'a>(
        &'a self,
        manifest: &'a suno_core::Manifest,
        dest: &'a Path,
        albums: &'a BTreeMap<String, AlbumArt>,
    ) -> execute::ReconcileInputs<'a> {
        execute::ReconcileInputs {
            manifest,
            dest,
            desired: &self.desired,
            albums_desired: &self.albums_desired,
            albums,
            playlist_desired: &self.playlist_desired,
            playlists: &self.stored_playlists,
            sources: &self.sources,
            library_authoritative: self.library_authoritative,
            playlists_enumerated: self.playlists_enumerated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use suno_core::{AreaKind, AreaListing, Clip, area_enumerated, area_mode};

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

        let modes = build_modes_by_id(&areas, force_copy);
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
