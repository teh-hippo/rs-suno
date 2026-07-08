//! The two run-mode tails: the dry-run/check report and the executing commit.

use super::*;

/// The dry-run / check tail: report the plan without touching disk. No lock is
/// taken and the destination is not created; a missing manifest reads as empty.
/// The synced `.lrc` preview reflects which clips would be written, with no
/// network fetch. `check --exit-code` returns [`ExitCode::General`] on changes.
pub(super) async fn dry_run_report(
    ctx: &RunCtx<'_>,
    assembled: &mut Assembled,
    store: &suno_core::LineageStore,
) -> Result<ExitCode> {
    let manifest = logs::load_manifest(ctx.dest)?;
    suno_core::preview_synced_lrc(
        &mut assembled.desired,
        &manifest,
        wallclock::now_secs(),
        ctx.settings.lrc_sidecar,
    );
    let plan =
        execute::reconcile_run(&assembled.reconcile_inputs(&manifest, ctx.dest, &store.albums))
            .await;
    if ctx.verbosity >= 1 {
        let no_failures = HashSet::new();
        for line in output::action_lines(&plan, &no_failures, ctx.verbosity) {
            eprint_t!("{line}");
        }
    }
    if ctx.verbosity >= -1 {
        eprint_t!("{}", output::dry_summary(ctx.account, &plan));
        // Read-only orphan report: audio files on disk that no manifest entry
        // tracks (moved or renamed by hand, or left from an older layout).
        // Listed only, never matched to a clip, renamed, or deleted (#146).
        let orphans = suno_core::untracked_audio(&manifest, &execute::walk_audio_files(ctx.dest));
        if !orphans.is_empty() {
            eprint_t!("{}", output::orphan_report(&orphans));
        }
    }
    if ctx.verb == Verb::Check && ctx.exit_code && prompt::plan_has_changes(&plan) {
        return Ok(ExitCode::General);
    }
    Ok(ExitCode::Ok)
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
    // Resolve this run's synced lyrics before reconcile: fetch Suno's alignment
    // for the clips that need it (gated by the per-clip marker, so a steady-state
    // re-sync fetches nothing and the feature being off fetches nothing), and
    // fill each clip's `.lrc` artifact with its content-hashed body. Reconcile
    // then plans the `.lrc` writes from the ACTUAL body, and the executor embeds
    // the same alignment as MP3 `SYLT`/plain-lyric tags.
    let (synced, pending_checks) = synced_lyrics::resolve_synced_lyrics(
        &mut assembled.desired,
        &manifest,
        ctx.client,
        ctx.http,
        settings.lrc_sidecar,
        verbosity,
        settings.concurrency,
    )
    .await;
    let plan =
        execute::reconcile_run(&assembled.reconcile_inputs(&manifest, dest, &store.albums)).await;

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

    execute::execute_plan(execute::ExecutePlan {
        summary_label: ctx.verb.summary_label(),
        plan,
        desired: &assembled.desired,
        manifest,
        synced,
        pending_checks,
        store,
        client: ctx.client,
        http: ctx.http,
        dest,
        settings,
        account: ctx.account,
        verbosity,
        library_authoritative: assembled.library_authoritative,
    })
    .await
}
