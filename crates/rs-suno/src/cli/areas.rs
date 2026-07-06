//! Area enumeration: list every selected area (Library, Liked, playlists) as IO
//! adapters, and build playlist `.m3u8` desired state.
//!
//! Each listing yields a [`suno_core::AreaListing`] carrying the enumeration
//! completeness and filter flags the planner needs to decide deletion. A failed
//! secondary area suppresses deletion for the whole run without aborting, while
//! a failed sole-area library listing keeps today's hard abort.

use std::collections::BTreeSet;

use futures_util::stream::{self, StreamExt};
use suno_core::{
    AreaKind, AreaListing, Clip, LIKED_PLAYLIST_ID, PlaylistDesired, PlaylistInput, SourceMode,
    SunoClient, build_playlist_desired, is_downloadable,
};

use crate::cli::args::SyncArgs;
use crate::cli::desired::{
    ExitCode, PlaylistPolicy, ResolvedSelection, is_narrowed, resolve_playlist,
};
use crate::cli::failure;
use crate::cli::task_output::eprint_t;
use crate::clock::TokioClock;
use crate::http::ReqwestHttp;

/// List every selected area (IO), in canonical order Library > Liked > Playlist.
///
/// A failed *secondary* area (liked, a playlist, or the unfiltered library
/// protector) warns and contributes a non-enumerated, empty source so one
/// failure suppresses all deletion while successful areas still download (§6). A
/// failed *plain* library listing (the sole area of a classic run) keeps today's
/// hard abort, and an unresolvable explicit `--playlist X` typo keeps today's
/// hard [`ExitCode::Config`].
pub(crate) async fn enumerate_areas(
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
pub(crate) async fn fetch_playlist_desired(
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
