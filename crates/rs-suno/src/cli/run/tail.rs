//! The two run-mode tails: the dry-run/check report and the executing commit.

use super::*;

/// The dry-run / check tail: report the plan without modifying the library.
/// The existing destination is locked while it is observed and planned; a
/// missing destination is neither created nor locked.
/// Lyrics resolve through the SAME path an executing run uses, so the report
/// predicts the writes a run would make (#537); the durable markers it returns
/// are dropped here, so no `checked_unix` and no manifest state are persisted.
/// `check --exit-code` returns [`ExitCode::General`] on changes; a report whose
/// required lyric lookups failed is not authoritative, so it returns the more
/// severe [`ExitCode::Partial`] instead of claiming a verified plan.
pub(super) async fn dry_run_report(
    ctx: &RunCtx<'_>,
    assembled: &mut Assembled,
    store: &suno_core::LineageStore,
) -> Result<ExitCode> {
    let _lock = logs::acquire_existing_lock(ctx.dest)?;
    let manifest = logs::load_manifest(ctx.dest)?;
    let local = execute::stat_manifest(ctx.dest, &manifest, &store.albums, &store.playlists).await;

    let resolution_manifest = execute::observed_resolution_manifest(&manifest, &local);
    let resolved = synced_lyrics::resolve_synced_lyrics(
        &mut assembled.desired,
        &resolution_manifest,
        ctx.client,
        ctx.http,
        ctx.verbosity,
        ctx.settings.concurrency,
        ctx.settings.lyrics_timing,
    )
    .await;
    let inputs = assembled.reconcile_inputs(&manifest, &store.albums);
    let plan = execute::reconcile_run_with_local(&inputs, &local);
    if ctx.verbosity >= 1 {
        let no_failures = HashSet::new();
        for line in output::action_lines(&plan, &no_failures, ctx.verbosity) {
            eprint_t!("{line}");
        }
    }
    if ctx.verbosity >= -1 {
        eprint_t!("{}", output::dry_summary(ctx.account, &plan));
        if !resolved.is_complete() {
            // The counts above are a lower bound: the clips whose lookup failed
            // keep their stored state, so any lyric change they need is missing
            // from the report rather than guessed at.
            eprint_t!(
                "warning: this report is incomplete -- the failed lyric lookups above may hide pending tag or sidecar changes"
            );
        }
        // Read-only orphan report: audio files on disk that no manifest entry
        // tracks (moved or renamed by hand, or left from an older layout).
        // Listed only, never matched to a clip, renamed, or deleted (#146).
        let orphans = suno_core::untracked_audio(&manifest, &execute::walk_audio_files(ctx.dest));
        if !orphans.is_empty() {
            eprint_t!("{}", output::orphan_report(&orphans));
        }
    }
    let mut code = ExitCode::Ok;
    if ctx.verb == Verb::Check && ctx.exit_code && prompt::plan_has_changes(&plan) {
        code = worse(code, ExitCode::General);
    }
    if !resolved.is_complete() {
        code = worse(code, ExitCode::Partial);
    }
    if plan.unverifiable() > 0 {
        code = worse(code, ExitCode::Partial);
    }
    Ok(code)
}

/// The executing tail: create the destination, take the lock *before* loading
/// the manifest so a concurrent run cannot plan against it then execute a stale
/// plan, reconcile under the lock, persist the graph and any pin before execute
/// (durability H4), gate deletions (the mass-delete cap and the confirmation
/// prompt), then run the plan. The lock lives to the end of the function.
pub(super) async fn execute_run(
    ctx: &RunCtx<'_>,
    mut assembled: Assembled,
    store: &mut suno_core::LineageStore,
    identity: &Identity,
) -> Result<ExitCode> {
    let dest = ctx.dest;
    let settings = ctx.settings;
    let verbosity = ctx.verbosity;

    std::fs::create_dir_all(dest)
        .with_context(|| format!("could not create {}", dest.display()))?;
    let _lock = logs::acquire_lock(dest)?;
    let manifest = logs::load_manifest(dest)?;
    let local = execute::stat_manifest(dest, &manifest, &store.albums, &store.playlists).await;

    let resolution_manifest = execute::observed_resolution_manifest(&manifest, &local);
    // Resolve this run's lyrics before reconcile, through the same path `check`
    // and `--dry-run` use. Missing plain audio metadata is always checked (until
    // a negative result is recorded); optional `.lrc` and `.lyrics.txt` slots are
    // resolved from the same response. Reconcile therefore sees the actual
    // body/hash, while the executor separately gates timed ID3 `SYLT` on
    // `lrc_sidecar`.
    let resolved = synced_lyrics::resolve_synced_lyrics(
        &mut assembled.desired,
        &resolution_manifest,
        ctx.client,
        ctx.http,
        verbosity,
        settings.concurrency,
        settings.lyrics_timing,
    )
    .await;
    let inputs = assembled.reconcile_inputs(&manifest, &store.albums);
    let plan = execute::reconcile_run_with_local(&inputs, &local);

    // Persist the lineage graph *before* execute (durability H4), under the same
    // lock as the manifest. This run refreshed it when it folded in a fresh
    // resolution (`graph_changed`) or when the identity guard pinned or updated
    // the owner (`owner_dirty`); an owner-only change must persist even when
    // resolution failed, so a first-use adoption is durable.
    if assembled.graph_changed || identity.owner_dirty() {
        logs::save_graph(dest, store)?;
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

    let is_sync = ctx.verb == Verb::Sync && !identity.force_additive();
    // The mass-delete cap counts every destructive action, audio and sidecar
    // alike (HARDENING B2), so a run that would mass-delete artifacts aborts too.
    let delete_count = plan.deletes() + plan.artifact_deletes() + plan.stem_deletes();
    if is_sync
        && mass_delete_abort(
            assembled.desired.len(),
            manifest.len(),
            delete_count,
            settings.min_newest,
            ctx.args.min_newest == Some(0),
            ctx.global.yes,
        )
    {
        eprint_t!(
            "error: sync aborted -- deletion safety rule triggered\n\nThe listing yielded {} clip(s), which would delete {} of {} local file(s).\nThis is almost certainly a listing error. No files were deleted.\n\nIf you intended to delete everything, pass --min-newest 0 --yes to confirm.",
            assembled.desired.len(),
            delete_count,
            manifest.len()
        );
        return Ok(ExitCode::Safety);
    }

    match confirm_decision(
        is_sync,
        delete_count,
        ctx.global.yes,
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
            output::progress_start(ctx.verb.progress_word(), ctx.account, &plan)
        );
    }

    let lyrics_complete = resolved.is_complete();
    let code = execute::execute_plan(execute::ExecutePlan {
        summary_label: ctx.verb.summary_label(),
        plan,
        desired: &assembled.desired,
        manifest,
        synced: resolved.aligned,
        pending_checks: resolved.pending,
        store,
        client: ctx.client,
        http: ctx.http,
        dest,
        settings,
        account: ctx.account,
        verbosity,
        library_authoritative: assembled.library_authoritative,
        playlist_desired: &assembled.playlist_desired,
        stored_playlists: &assembled.stored_playlists,
        sources: &assembled.sources,
        playlists_enumerated: assembled.playlists_enumerated,
    })
    .await?;
    Ok(if lyrics_complete {
        code
    } else {
        worse(code, ExitCode::Partial)
    })
}
