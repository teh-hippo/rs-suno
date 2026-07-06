//! The multi-area sync planner: the pure decision logic for what is
//! authoritative and what may be deleted across a run's areas (library, liked
//! feed, playlists). Lifted from the CLI so the deletion-authority logic is
//! covered by the core suite, beside the leaf predicates it composes in
//! [`crate::reconcile`].

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::{
    Clip, Desired, LIKED_PLAYLIST_ID, LineageStore, PlaylistDesired, PlaylistInput, SourceMode,
    SourceStatus, area_authoritative, area_fully_enumerated, build_playlist_desired,
    deletion_allowed,
};

/// One area's listing outcome for the multi-area planner.
///
/// The `authoritative_ignoring_empty` flag is the area's completeness verdict
/// *before* the empty-mirror guard (§5), which [`area_enumerated`] applies later
/// against the final mode, so a copy-verb override that turns a Mirror area Copy
/// re-scores an empty area correctly. It is only ever produced by
/// [`area_authoritative`] via [`AreaListing::listed`], so the #248 filter-loss
/// guard cannot be bypassed by an out-of-band value.
pub struct AreaListing {
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
pub enum AreaKind {
    Library,
    Liked,
    Playlist { id: String, name: String },
}

impl AreaListing {
    /// A drained listing. The authority flag is computed from the raw listing
    /// signals via [`area_authoritative`], so the #248 guard is unbypassable
    /// from outside the crate: the fields are private, and although in-crate
    /// tests may construct directly, every production path goes through this
    /// constructor.
    pub fn listed(
        kind: AreaKind,
        mode: SourceMode,
        clips: Vec<Clip>,
        complete: bool,
        any_filtered: bool,
        narrowed: bool,
    ) -> Self {
        Self {
            kind,
            mode,
            clips,
            authoritative_ignoring_empty: area_authoritative(complete, any_filtered, narrowed),
        }
    }

    /// A failed or empty listing: it holds no clips and is never authoritative,
    /// so it suppresses deletion without ever vanishing from the sources (§6).
    pub fn failed(kind: AreaKind, mode: SourceMode) -> Self {
        Self {
            kind,
            mode,
            clips: Vec::new(),
            authoritative_ignoring_empty: false,
        }
    }

    /// A playlist area whose listing could not be resolved or fetched (§6).
    pub fn unresolved_playlist(mode: SourceMode) -> Self {
        Self::failed(
            AreaKind::Playlist {
                id: String::new(),
                name: String::new(),
            },
            mode,
        )
    }

    /// The area's downloadable clips.
    pub fn clips(&self) -> &[Clip] {
        &self.clips
    }
}

/// This area's mode after the copy-verb / force-additive override.
pub fn area_mode(area: &AreaListing, force_copy: bool) -> SourceMode {
    if force_copy {
        SourceMode::Copy
    } else {
        area.mode
    }
}

/// Whether this area is authoritative for deletion, applying the empty-mirror
/// guard (§5) against the final mode.
#[must_use]
pub fn area_enumerated(area: &AreaListing, force_copy: bool) -> bool {
    area_fully_enumerated(
        area.authoritative_ignoring_empty,
        area.clips.is_empty(),
        area_mode(area, force_copy),
    )
}

/// Whether a Library area is present and fully enumerated (the implicit
/// protector counts; `library="off"` leaves no Library area, so this is false).
#[must_use]
pub fn library_authoritative(areas: &[AreaListing], force_copy: bool) -> bool {
    areas
        .iter()
        .any(|a| matches!(a.kind, AreaKind::Library) && area_enumerated(a, force_copy))
}

/// The per-source enumeration status of every area, for the deletion verdict.
#[must_use]
pub fn source_statuses(areas: &[AreaListing], force_copy: bool) -> Vec<SourceStatus> {
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
#[must_use]
pub fn adoption_enumerated(areas: &[AreaListing], force_copy: bool) -> bool {
    library_authoritative(areas, force_copy)
        || deletion_allowed(&source_statuses(areas, force_copy))
}

/// Build the clip union across areas in canonical order, first area winning per
/// id so the Library payload is kept (H1).
pub fn union_clips(areas: &[AreaListing]) -> Vec<Clip> {
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

/// Build the `.m3u8` desired state for an area-scoped run (no authoritative
/// Library). Only the playlist and liked areas that fully enumerated their
/// members are rendered, and only when `members_intact` (the union was not
/// truncated by `--limit`/`--since`, so `desired` still holds every member);
/// every other stored playlist id is protected so no `.m3u8` is rewritten or
/// deleted from a partial view (B2/D3).
pub fn build_scoped_playlist_desired(
    areas: &[AreaListing],
    desired: &[Desired],
    store: &LineageStore,
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

/// Fold a union of per-area clip lists into `modes_by_id`, mapping each clip id
/// to the deduplicated, canonical-order list of every area mode holding it.
///
/// `areas` is processed in canonical area order (Library, Liked, Playlists), and
/// each clip's modes are normalised to `[Mirror, Copy]` order, mirroring
/// `aggregate_desired` so a clip held by both a mirror and a copy area is
/// copy-protected (SYNC-8).
pub fn build_modes_by_id(areas: &[(SourceMode, Vec<String>)]) -> HashMap<String, Vec<SourceMode>> {
    let mut map: HashMap<String, (bool, bool)> = HashMap::new();
    for (mode, ids) in areas {
        for id in ids {
            let entry = map.entry(id.clone()).or_insert((false, false));
            match mode {
                SourceMode::Mirror => entry.0 = true,
                SourceMode::Copy => entry.1 = true,
            }
        }
    }
    map.into_iter()
        .map(|(id, (mirror, copy))| {
            let mut modes = Vec::new();
            if mirror {
                modes.push(SourceMode::Mirror);
            }
            if copy {
                modes.push(SourceMode::Copy);
            }
            (id, modes)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Action, ArtifactToggles, AudioFormat, LocalFile, Manifest, ManifestEntry, NamingConfig,
        build_desired, narrows_downloads, reconcile,
    };

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
            area_authoritative(true, false, true),
        );
        assert!(!area_enumerated(&narrowed, false));
        assert!(!deletion_allowed(&source_statuses(&[narrowed], false)));

        let full = pl_area(
            SourceMode::Mirror,
            &["a"],
            area_authoritative(true, false, false),
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
                area_authoritative(true, false, true),
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
            area_authoritative(true, false, true),
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
            area_authoritative(true, false, true),
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
            AudioFormat::Flac,
            &modes,
            &HashMap::new(),
            &BTreeSet::new(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );

        let mut manifest = Manifest::new();
        // Orphans: one previously from the mirror area, one from the copy area.
        for id in ["m-live", "c-live", "m-orphan", "c-orphan"] {
            manifest.insert(
                id,
                ManifestEntry {
                    path: format!("{id}.flac"),
                    format: AudioFormat::Flac,
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
                Action::Delete { clip_id, .. } => Some(clip_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deleted, vec!["m-orphan"]);
    }

    // Test 7 (SYNC-8): a clip held by a Mirror and a Copy area is stamped
    // `[Mirror, Copy]`, so build_desired carries the Copy protection.
    #[test]
    fn build_modes_by_id_copy_wins_and_dedups() {
        let map = build_modes_by_id(&[
            (SourceMode::Mirror, vec!["a".to_owned(), "b".to_owned()]),
            (SourceMode::Copy, vec!["b".to_owned(), "c".to_owned()]),
        ]);
        assert_eq!(map["a"], vec![SourceMode::Mirror]);
        assert_eq!(map["b"], vec![SourceMode::Mirror, SourceMode::Copy]);
        assert_eq!(map["c"], vec![SourceMode::Copy]);
    }
}
