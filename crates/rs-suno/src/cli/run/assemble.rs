//! The assemble phase: resolve per-area modes and deletion gates, select and
//! name the clips, thread in stems, and build the folder-art and playlist
//! desired state once the run's additivity is known.

use super::*;

/// Assemble the reconcile inputs once the run's additivity is known: resolve
/// the per-area modes and deletion gates, select and name the clips, thread in
/// existing stems, and build the folder-art and playlist desired state. Returns
/// [`ExitCode::Config`] for an unparseable `--since`.
pub(super) async fn assemble(
    ctx: &RunCtx<'_>,
    areas: &[suno_core::AreaListing],
    clips: &[suno_core::Clip],
    store: &suno_core::LineageStore,
    selection: &ResolvedSelection,
    force_copy: bool,
    graph_changed: bool,
) -> std::result::Result<Assembled, ExitCode> {
    let settings = ctx.settings;
    let args = ctx.args;
    let verbosity = ctx.verbosity;

    let sources = source_statuses(areas, force_copy);
    let can_delete = deletion_allowed(&sources);
    // Art, `.m3u8`, and the library index are gated on an authoritative Library:
    // a Library area present in the selection (the implicit protector counts;
    // `library="off"` does not) that fully enumerated.
    let library_authoritative = library_authoritative(areas, force_copy);
    let colliding_albums = store.colliding_root_titles();
    let colliding_ids = store.colliding_clip_ids();

    // Every clip's modes across the areas holding it, so each Desired carries the
    // Copy protection of any Copy area even when a Mirror area also holds it
    // (SYNC-8).
    let modes_by_id = build_modes_by_id(areas, force_copy);

    let since = match args.since.as_deref().map(RecencySpec::parse).transpose() {
        Ok(since) => since,
        Err(message) => {
            eprint_t!("error: {message}");
            return Err(ExitCode::Config);
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
        last_run: last_run::read_last_run(ctx.dest),
    };
    let selected = select(clips, &params);
    let mut contexts: HashMap<String, LineageContext> = selected
        .iter()
        .map(|clip| (clip.id.clone(), store.context_for(clip)))
        .collect();
    // Number each lineage album's tracks by creation order, promoting any
    // configured lead to track 1, and fold the result into the contexts so it
    // flows into the tag, change hash, and filename prefix.
    let leads = resolve_lead_ids(&selected, &settings.lead_tracks);
    if verbosity >= -1 {
        for entry in &leads.unmatched {
            eprint_t!("warning: lead_tracks entry '{entry}' matched no selected clip; ignoring");
        }
        for entry in &leads.ambiguous {
            eprint_t!(
                "warning: lead_tracks entry '{entry}' matched more than one clip; use a longer id"
            );
        }
    }
    for (id, assignment) in assign_track_numbers(
        &selected,
        &contexts,
        &leads.resolved,
        settings.number_singletons,
    ) {
        if let Some(context) = contexts.get_mut(&id) {
            context.track = assignment.track;
            context.track_total = assignment.total;
        }
    }
    let mut desired = build_desired(
        &selected,
        settings.format,
        &modes_by_id,
        &contexts,
        &colliding_albums,
        &colliding_ids,
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
        ctx.client,
        ctx.http,
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
                ctx.client,
                ctx.http,
                &desired,
                &mut protected_playlists,
                verbosity,
                settings.concurrency,
            )
            .await
        } else {
            build_scoped_playlist_desired(
                areas,
                &desired,
                store,
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

    Ok(Assembled {
        desired,
        albums_desired,
        playlist_desired,
        stored_playlists,
        sources,
        library_authoritative,
        playlists_enumerated,
        graph_changed,
    })
}
