//! Pure decision logic for the sync/copy/check engine.
//!
//! Building the desired target state, the deletion-safety abort, the
//! destructive-sync confirmation gate, and the mapping from an [`ExecOutcome`]
//! to a process exit code. Keeping these out of the IO orchestration lets the
//! safety-critical rules be unit-tested directly.

use std::collections::{BTreeMap, HashMap};

use suno_core::{
    AreaMode, AreasConfig, ExecOutcome, LIKED_PLAYLIST_ID, Playlist, RunStatus, SourceMode,
};

/// Below this manifest size the mass-deletion fraction rule does not fire; a
/// small library legitimately churns its whole contents, and the empty-listing
/// rule still covers the catastrophic case.
const MASS_DELETE_FLOOR: usize = 8;

/// Process exit codes, mirroring the guide (docs/src/scheduling-and-exit-codes.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Ok = 0,
    General = 1,
    /// A usage error the CLI raises itself, such as `--allow-account-change` on
    /// a non-executing verb. clap emits its own parse failures with this code
    /// too; kept so the enum mirrors the full exit-code table in the guide
    /// (docs/src/scheduling-and-exit-codes.md).
    Usage = 2,
    Config = 3,
    Auth = 4,
    Partial = 5,
    Transient = 6,
    Safety = 7,
    Interrupted = 8,
    DiskFull = 9,
}

impl ExitCode {
    /// The numeric code passed to [`std::process::exit`].
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// Whether a `--limit` or `--since` filter narrows a listing.
pub fn is_narrowed(limit: Option<usize>, since: Option<&str>) -> bool {
    limit.is_some() || since.is_some()
}

/// The library area's plan: its mode and how it lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibrarySpec {
    /// How the library area treats its clips.
    pub mode: SourceMode,
    /// List the full feed unfiltered, ignoring `--limit`/`--since` (D2). False
    /// only for the classic plain library run, where `--limit` narrows the
    /// listing and disarms deletion exactly as today.
    pub unfiltered: bool,
    /// True when this area was injected as a copy-protector rather than
    /// user-selected, so it lists the whole library purely to keep
    /// library-exclusive files out of a Mirror area's deletion candidates (D1).
    pub protector: bool,
}

/// Which playlists a run selects and how they are moded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistPolicy {
    /// No playlist areas.
    None,
    /// Specific playlists, by CLI value or config id, each with its mode.
    Explicit(Vec<(String, SourceMode)>),
    /// Every one of the account's playlists at `default`, with per-id
    /// `overrides` (config `playlists = ...` group default).
    All {
        default: SourceMode,
        overrides: BTreeMap<String, SourceMode>,
    },
}

/// The fully resolved set of areas for a run, before any network listing.
///
/// This is a pure function of the verb, CLI scope flags, `--mode`, the account's
/// `[areas]` config, and whether the run is force-additive (copy verb, re-pin,
/// or first-use adoption). The caller enumerates each present area and assembles
/// the union, `modes_by_id`, and per-area [`SourceStatus`] from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSelection {
    /// The library area, or `None` when `library = "off"` deliberately arms
    /// deletion of library-exclusive files (no copy-protector).
    pub library: Option<LibrarySpec>,
    /// The liked feed's mode, or `None` when it is not selected.
    pub liked: Option<SourceMode>,
    /// The playlist selection policy.
    pub playlists: PlaylistPolicy,
    /// True when the run was driven by transient CLI scope flags rather than
    /// `[areas]` config, so an unresolvable `--playlist X` is a hard typo error
    /// and a playlist-listing failure aborts (today's behaviour); config-driven
    /// runs degrade such failures to a protected, non-deleting area instead.
    pub cli_scoped: bool,
}

impl ResolvedSelection {
    /// Whether any selected area is a `Mirror` (so the run can delete).
    ///
    /// The copy-protector is never a mirror, so a run is armed only by a
    /// user-selected or configured Mirror library, liked feed, or playlist.
    fn is_armed(&self) -> bool {
        let lib_mirror = self
            .library
            .is_some_and(|l| l.mode == SourceMode::Mirror && !l.protector);
        let liked_mirror = self.liked == Some(SourceMode::Mirror);
        let pl_mirror = match &self.playlists {
            PlaylistPolicy::None => false,
            PlaylistPolicy::Explicit(list) => list.iter().any(|(_, m)| *m == SourceMode::Mirror),
            PlaylistPolicy::All { default, overrides } => {
                *default == SourceMode::Mirror
                    || overrides.values().any(|m| *m == SourceMode::Mirror)
            }
        };
        lib_mirror || liked_mirror || pl_mirror
    }

    /// The classic whole-account run: a single Library area listed at the verb's
    /// mode (honouring `--limit`), with no scoped areas and no injected
    /// protector. Only this shape may walk every account playlist; every scoped
    /// or `[areas]` run instead maintains just the playlist areas it enumerated
    /// and protects the rest, so an injected copy-protector never promotes an
    /// unselected playlist's `.m3u8` to a deletion candidate (D3).
    pub(crate) fn is_plain_library(&self) -> bool {
        self.library.is_some_and(|l| !l.unfiltered && !l.protector)
            && self.liked.is_none()
            && matches!(self.playlists, PlaylistPolicy::None)
            && !self.cli_scoped
    }
}

/// Resolve the areas a run touches and their modes (pure).
///
/// Precedence:
/// - CLI scope flags (`--liked`/`--playlist`) select a transient run in
///   [`SourceMode::Copy`] unless `--mode mirror` is given; they override any
///   `[areas]` config.
/// - Otherwise `[areas]` config drives the run: `library`/`liked`/`playlists`
///   and per-playlist overrides.
/// - Otherwise the classic plain library run at the verb's mode.
///
/// After the base modes are set, a force-additive run (copy verb, re-pin, or
/// adoption) rewrites every mode to [`SourceMode::Copy`], so nothing is armed
/// and no protector is injected. Finally, when any selected area is a Mirror and
/// the library is neither explicitly selected nor `"off"`, an implicit
/// full-library copy-protector is injected (D1) so a Mirror area can never
/// delete a library-exclusive file.
pub fn resolve_selection(
    verb_mode: SourceMode,
    transient_mode: Option<SourceMode>,
    cli_liked: bool,
    cli_playlists: &[String],
    areas_cfg: Option<&AreasConfig>,
    force_copy: bool,
) -> ResolvedSelection {
    let want_liked = cli_liked || cli_playlists.iter().any(|v| v == LIKED_PLAYLIST_ID);
    let cli_pls: Vec<&str> = cli_playlists
        .iter()
        .map(String::as_str)
        .filter(|v| *v != LIKED_PLAYLIST_ID)
        .collect();
    let has_cli_scope = want_liked || !cli_pls.is_empty();

    // `library = "off"` is expressible only via config; it suppresses the
    // protector so library-exclusive files become deletion candidates.
    let mut library_off = false;
    let (mut library, mut liked, mut playlists) = if has_cli_scope {
        // Transient scoped run: CLI flags win over config.
        let mode = transient_mode.unwrap_or(SourceMode::Copy);
        let liked = want_liked.then_some(mode);
        let playlists = if cli_pls.is_empty() {
            PlaylistPolicy::None
        } else {
            PlaylistPolicy::Explicit(cli_pls.iter().map(|v| ((*v).to_owned(), mode)).collect())
        };
        (None, liked, playlists)
    } else if let Some(cfg) = areas_cfg {
        // Config-driven run.
        let library = match cfg.library {
            Some(AreaMode::Off) => {
                library_off = true;
                None
            }
            Some(AreaMode::Mode(mode)) => Some(LibrarySpec {
                mode,
                unfiltered: true,
                protector: false,
            }),
            None => None,
        };
        let liked = cfg.liked;
        let playlists = match cfg.playlists {
            Some(default) => PlaylistPolicy::All {
                default,
                overrides: cfg.playlist.clone().into_iter().collect(),
            },
            None if cfg.playlist.is_empty() => PlaylistPolicy::None,
            None => PlaylistPolicy::Explicit(
                cfg.playlist
                    .clone()
                    .into_iter()
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
        };
        (library, liked, playlists)
    } else {
        // Plain library run at the verb's mode (or a --mode override).
        let mode = transient_mode.unwrap_or(verb_mode);
        (
            Some(LibrarySpec {
                mode,
                unfiltered: false,
                protector: false,
            }),
            None,
            PlaylistPolicy::None,
        )
    };

    if force_copy {
        rewrite_all_copy(&mut library, &mut liked, &mut playlists);
    }

    let mut selection = ResolvedSelection {
        library,
        liked,
        playlists,
        cli_scoped: has_cli_scope,
    };

    // D1: inject the implicit full-library copy-protector whenever a Mirror area
    // is armed and the library is neither explicitly selected nor "off".
    if selection.is_armed() && selection.library.is_none() && !library_off {
        selection.library = Some(LibrarySpec {
            mode: SourceMode::Copy,
            unfiltered: true,
            protector: true,
        });
    }

    selection
}

/// Rewrite every area mode to [`SourceMode::Copy`] for a force-additive run.
fn rewrite_all_copy(
    library: &mut Option<LibrarySpec>,
    liked: &mut Option<SourceMode>,
    playlists: &mut PlaylistPolicy,
) {
    if let Some(lib) = library {
        lib.mode = SourceMode::Copy;
    }
    if liked.is_some() {
        *liked = Some(SourceMode::Copy);
    }
    match playlists {
        PlaylistPolicy::None => {}
        PlaylistPolicy::Explicit(list) => {
            for (_, mode) in list.iter_mut() {
                *mode = SourceMode::Copy;
            }
        }
        PlaylistPolicy::All { default, overrides } => {
            *default = SourceMode::Copy;
            for mode in overrides.values_mut() {
                *mode = SourceMode::Copy;
            }
        }
    }
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

/// Why a `--playlist` value could not be resolved to one of the account's own
/// playlists. Both variants map to [`ExitCode::Config`] at the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistResolveError {
    /// No playlist matched the value by id or name.
    NotFound(String),
    /// The value matched more than one playlist by name.
    Ambiguous(String),
}

impl std::fmt::Display for PlaylistResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlaylistResolveError::NotFound(value) => {
                write!(f, "no playlist matches '{value}'")
            }
            PlaylistResolveError::Ambiguous(value) => {
                write!(
                    f,
                    "'{value}' matches more than one playlist; use the playlist id instead"
                )
            }
        }
    }
}

/// Resolve a `--playlist` value to one of the account's own playlists.
///
/// Matching is tried in order: exact id, then exact name, then case-insensitive
/// name. Because `get_playlists` already excludes shared and trashed playlists,
/// the search space is the user's own non-trashed playlists, so a raw id that is
/// not listed is rejected rather than fetched behind the scenes. An unknown
/// value, or one that matches more than one playlist by name, is an error.
pub fn resolve_playlist<'a>(
    value: &str,
    playlists: &'a [Playlist],
) -> std::result::Result<&'a Playlist, PlaylistResolveError> {
    if let Some(hit) = playlists.iter().find(|playlist| playlist.id == value) {
        return Ok(hit);
    }
    let exact: Vec<&Playlist> = playlists
        .iter()
        .filter(|playlist| playlist.name == value)
        .collect();
    match exact.as_slice() {
        [one] => return Ok(one),
        [_, _, ..] => return Err(PlaylistResolveError::Ambiguous(value.to_owned())),
        [] => {}
    }
    let ci: Vec<&Playlist> = playlists
        .iter()
        .filter(|playlist| playlist.name.eq_ignore_ascii_case(value))
        .collect();
    match ci.as_slice() {
        [one] => Ok(one),
        [_, _, ..] => Err(PlaylistResolveError::Ambiguous(value.to_owned())),
        [] => Err(PlaylistResolveError::NotFound(value.to_owned())),
    }
}

/// The belt-and-suspenders empty-listing / mass-deletion abort (exit 7).
///
/// Even though reconcile only emits deletes when every source was fully
/// enumerated, an empty or near-empty listing of a fully-enumerated source
/// would still wipe the library. This refuses that unless the user explicitly
/// confirmed an intentional mass deletion with `--min-newest 0 --yes`.
///
/// The empty-listing case (an `Ok(vec![])` from an auth glitch or API bug) is
/// the crown-jewel risk, so its waiver is stricter: it accepts only an explicit
/// per-invocation `--min-newest 0` (`explicit_min_newest_zero`), never a value
/// resolved from persisted config or the environment. That stops a stored
/// `min_newest = 0` or a habitual `SUNO_YES`/`--yes` in cron from silently
/// disarming the guard. The large-fraction case stays waivable by the resolved
/// `min_newest`.
pub fn mass_delete_abort(
    desired_count: usize,
    manifest_len: usize,
    delete_count: usize,
    min_newest: u32,
    explicit_min_newest_zero: bool,
    yes: bool,
) -> bool {
    if delete_count == 0 || manifest_len == 0 {
        return false;
    }
    if desired_count == 0 {
        return !(explicit_min_newest_zero && yes);
    }
    if min_newest == 0 && yes {
        return false;
    }
    is_large_fraction(delete_count, manifest_len)
}

/// True when `delete_count` is at least half of a non-trivial manifest.
fn is_large_fraction(delete_count: usize, manifest_len: usize) -> bool {
    manifest_len >= MASS_DELETE_FLOOR && delete_count.saturating_mul(2) >= manifest_len
}

/// The outcome of the destructive-sync confirmation gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    /// No deletions, `copy`, or `--yes`: run without prompting.
    Proceed,
    /// Deletions pending on an interactive terminal: ask `[y/N]`.
    Prompt,
    /// Deletions pending without a TTY and without `--yes`: refuse.
    RefuseNonInteractive,
}

/// Decide how to gate a run that may delete files.
///
/// `copy` never deletes and never prompts. A `sync` with pending deletions
/// prompts on a TTY, and refuses in a non-interactive context unless `--yes`
/// was passed.
pub fn confirm_decision(
    is_sync: bool,
    delete_count: usize,
    yes: bool,
    stdin_is_tty: bool,
) -> Confirm {
    if !is_sync || delete_count == 0 || yes {
        return Confirm::Proceed;
    }
    if stdin_is_tty {
        Confirm::Prompt
    } else {
        Confirm::RefuseNonInteractive
    }
}

/// Whether a typed confirmation response means "go ahead".
pub fn confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Map an [`ExecOutcome`] to a process exit code (docs/src/scheduling-and-exit-codes.md).
///
/// A disk-full abort is 9 and an auth abort is 4, both checked before the
/// failures list. A clean run is 0. With failures, the run is "transient
/// exhausted" (6) when nothing at all progressed, otherwise "partial" (5).
pub fn run_exit_code(outcome: &ExecOutcome) -> ExitCode {
    if outcome.status == RunStatus::DiskFull {
        return ExitCode::DiskFull;
    }
    if outcome.status == RunStatus::AuthAborted {
        return ExitCode::Auth;
    }
    if outcome.failures.is_empty() {
        return ExitCode::Ok;
    }
    let progressed = outcome.downloaded
        + outcome.reformatted
        + outcome.retagged
        + outcome.renamed
        + outcome.deleted
        + outcome.skipped
        + outcome.artifacts_written
        + outcome.artifacts_deleted;
    if progressed == 0 {
        ExitCode::Transient
    } else {
        ExitCode::Partial
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use suno_core::Failure;

    #[test]
    fn is_narrowed_tracks_limit_and_since() {
        assert!(!is_narrowed(None, None));
        assert!(is_narrowed(Some(5), None));
        assert!(is_narrowed(None, Some("7d")));
        assert!(is_narrowed(Some(5), Some("7d")));
    }

    fn areas(toml_body: &str) -> suno_core::AreasConfig {
        let toml = format!("[accounts.a]\ntoken=\"t\"\n[accounts.a.areas]\n{toml_body}");
        suno_core::Config::from_toml(&toml).unwrap().accounts["a"]
            .areas
            .clone()
            .unwrap()
    }

    // Test 1: a bare `--playlist X` (no `--mode`, no config) is Copy, injects no
    // protector, and arms nothing, so it can never delete.
    #[test]
    fn resolve_bare_playlist_is_copy_and_unarmed() {
        let sel = resolve_selection(
            SourceMode::Mirror,
            None,
            false,
            &["holiday".to_owned()],
            None,
            false,
        );
        assert_eq!(
            sel.playlists,
            PlaylistPolicy::Explicit(vec![("holiday".to_owned(), SourceMode::Copy)])
        );
        assert!(sel.library.is_none());
        assert!(!sel.is_armed());
        assert!(sel.cli_scoped);
    }

    // Test 2: a bare `--liked` is Copy and unarmed.
    #[test]
    fn resolve_bare_liked_is_copy_and_unarmed() {
        let sel = resolve_selection(SourceMode::Mirror, None, true, &[], None, false);
        assert_eq!(sel.liked, Some(SourceMode::Copy));
        assert!(sel.library.is_none());
        assert!(!sel.is_armed());
    }

    // Test 3 / D1: `--playlist X --mode mirror` arms deletion and injects the
    // implicit full-library copy protector, unfiltered.
    #[test]
    fn resolve_playlist_mirror_injects_unfiltered_protector() {
        let sel = resolve_selection(
            SourceMode::Mirror,
            Some(SourceMode::Mirror),
            false,
            &["holiday".to_owned()],
            None,
            false,
        );
        assert!(sel.is_armed());
        let lib = sel.library.expect("protector injected");
        assert_eq!(lib.mode, SourceMode::Copy);
        assert!(lib.protector);
        assert!(lib.unfiltered);
    }

    // D1: a transient `--mode mirror` with no scope flags still runs the plain
    // library as a Mirror (this is the classic full sync spelled explicitly).
    // Test 17: the classic N=1 plain library run resolves to exactly one library
    // area at the verb's mode, filtered (honours `--limit`/`--since`), not
    // cli-scoped, and with no liked or playlist areas, so its data flow is
    // byte-identical to today's single-source path.
    #[test]
    fn resolve_plain_library_is_backwards_compatible() {
        let sync = resolve_selection(SourceMode::Mirror, None, false, &[], None, false);
        let lib = sync.library.expect("library present");
        assert_eq!(lib.mode, SourceMode::Mirror);
        assert!(!lib.unfiltered, "plain library honours --limit/--since");
        assert!(!lib.protector);
        assert!(!sync.cli_scoped);
        assert_eq!(sync.liked, None);
        assert!(matches!(sync.playlists, PlaylistPolicy::None));
        assert!(sync.is_armed());

        let copy = resolve_selection(SourceMode::Copy, None, false, &[], None, true);
        let lib = copy.library.expect("library present");
        assert_eq!(lib.mode, SourceMode::Copy);
        assert!(!copy.is_armed(), "a copy run deletes nothing");
    }

    #[test]
    fn resolve_mode_mirror_no_scope_is_plain_library_mirror() {
        let sel = resolve_selection(
            SourceMode::Mirror,
            Some(SourceMode::Mirror),
            false,
            &[],
            None,
            false,
        );
        let lib = sel.library.expect("library present");
        assert_eq!(lib.mode, SourceMode::Mirror);
        assert!(!lib.protector);
        assert!(!lib.unfiltered);
        assert!(sel.is_armed());
    }

    // Only the classic whole-account run walks every playlist; a scoped run, an
    // injected protector, or any `[areas]` run must not, so an unselected
    // playlist's `.m3u8` is never promoted to a deletion candidate (D3).
    #[test]
    fn is_plain_library_only_for_the_whole_account_run() {
        let plain_sync = resolve_selection(SourceMode::Mirror, None, false, &[], None, false);
        assert!(
            plain_sync.is_plain_library(),
            "plain sync walks all playlists"
        );

        let plain_copy = resolve_selection(SourceMode::Copy, None, false, &[], None, true);
        assert!(
            plain_copy.is_plain_library(),
            "plain copy walks all playlists"
        );

        let explicit_mirror = resolve_selection(
            SourceMode::Mirror,
            Some(SourceMode::Mirror),
            false,
            &[],
            None,
            false,
        );
        assert!(
            explicit_mirror.is_plain_library(),
            "`--mode mirror` with no scope is the classic full sync"
        );

        // A scoped Mirror injects a protector but selects only its playlist.
        let scoped_mirror = resolve_selection(
            SourceMode::Mirror,
            Some(SourceMode::Mirror),
            false,
            &["holiday".to_owned()],
            None,
            false,
        );
        assert!(scoped_mirror.library.unwrap().protector);
        assert!(!scoped_mirror.is_plain_library());

        // A bare scoped Copy selects one playlist and no library at all.
        let scoped_copy = resolve_selection(
            SourceMode::Mirror,
            None,
            false,
            &["holiday".to_owned()],
            None,
            false,
        );
        assert!(!scoped_copy.is_plain_library());

        // Config that mirrors playlists injects the protector but is area-driven.
        let config_playlists = resolve_selection(
            SourceMode::Mirror,
            None,
            false,
            &[],
            Some(&areas("playlists = \"mirror\"\n")),
            false,
        );
        assert!(!config_playlists.is_plain_library());

        // An explicit library mirror lists unfiltered, so it is area-driven too.
        let config_library = resolve_selection(
            SourceMode::Mirror,
            None,
            false,
            &[],
            Some(&areas("library = \"mirror\"\n")),
            false,
        );
        assert!(!config_library.is_plain_library());
    }

    // Test 4: an armed config playlist with no `library` key injects the Copy
    // protector; `library="off"` suppresses it and leaves no library area.
    #[test]
    fn resolve_config_playlists_mirror_protector_and_off() {
        let with = resolve_selection(
            SourceMode::Mirror,
            None,
            false,
            &[],
            Some(&areas("playlists = \"mirror\"\n")),
            false,
        );
        let lib = with.library.expect("protector injected");
        assert!(lib.protector);
        assert_eq!(lib.mode, SourceMode::Copy);

        let off = resolve_selection(
            SourceMode::Mirror,
            None,
            false,
            &[],
            Some(&areas("library = \"off\"\nplaylists = \"mirror\"\n")),
            false,
        );
        assert!(off.library.is_none(), "library=off leaves no library area");
        assert!(off.is_armed());
    }

    // Test 10: a copy verb rewrites every configured mode to Copy and never arms.
    #[test]
    fn resolve_copy_verb_rewrites_all_to_copy() {
        let sel = resolve_selection(
            SourceMode::Copy,
            None,
            false,
            &[],
            Some(&areas(
                "library = \"mirror\"\nliked = \"mirror\"\nplaylists = \"mirror\"\n",
            )),
            true,
        );
        assert_eq!(sel.library.unwrap().mode, SourceMode::Copy);
        assert_eq!(sel.liked, Some(SourceMode::Copy));
        match sel.playlists {
            PlaylistPolicy::All { default, .. } => assert_eq!(default, SourceMode::Copy),
            other => panic!("expected All, got {other:?}"),
        }
        assert!(!sel.is_armed());
    }

    // CLI scope flags win over `[areas]` config.
    #[test]
    fn resolve_cli_scope_overrides_areas_config() {
        let sel = resolve_selection(
            SourceMode::Mirror,
            None,
            false,
            &["holiday".to_owned()],
            Some(&areas("library = \"mirror\"\n")),
            false,
        );
        // The library mirror from config is ignored; only the CLI playlist (Copy)
        // is selected, so nothing is armed.
        assert!(sel.library.is_none());
        assert!(!sel.is_armed());
    }

    // Config per-playlist overrides ride on the `All` group default.
    #[test]
    fn resolve_config_playlist_overrides() {
        let sel = resolve_selection(
            SourceMode::Mirror,
            None,
            false,
            &[],
            Some(&areas(
                "playlists = \"copy\"\n[accounts.a.areas.playlist]\n\"pl_1\" = \"mirror\"\n",
            )),
            false,
        );
        match &sel.playlists {
            PlaylistPolicy::All { default, overrides } => {
                assert_eq!(*default, SourceMode::Copy);
                assert_eq!(overrides["pl_1"], SourceMode::Mirror);
            }
            other => panic!("expected All, got {other:?}"),
        }
        // A mirror override arms the run, so the protector is injected.
        assert!(sel.is_armed());
        assert!(sel.library.unwrap().protector);
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

    // Test 11: two distinct clips from two areas render two distinct paths in one
    // build_desired pass (global disambiguation), each carrying its area's modes.
    fn playlist(id: &str, name: &str) -> Playlist {
        Playlist {
            id: id.to_owned(),
            name: name.to_owned(),
            num_clips: 0,
        }
    }

    #[test]
    fn resolve_playlist_matches_by_id_first() {
        let playlists = vec![playlist("id-1", "Chill"), playlist("id-2", "id-1")];
        // The literal id wins even though another playlist is named "id-1".
        assert_eq!(resolve_playlist("id-1", &playlists).unwrap().name, "Chill");
    }

    #[test]
    fn resolve_playlist_matches_by_exact_name() {
        let playlists = vec![playlist("id-1", "Chill"), playlist("id-2", "Focus")];
        assert_eq!(resolve_playlist("Focus", &playlists).unwrap().id, "id-2");
    }

    #[test]
    fn resolve_playlist_matches_case_insensitively() {
        let playlists = vec![playlist("id-1", "Chill Beats")];
        assert_eq!(
            resolve_playlist("chill beats", &playlists).unwrap().id,
            "id-1"
        );
    }

    #[test]
    fn resolve_playlist_rejects_an_unknown_value() {
        let playlists = vec![playlist("id-1", "Chill")];
        assert_eq!(
            resolve_playlist("missing", &playlists),
            Err(PlaylistResolveError::NotFound("missing".to_owned()))
        );
    }

    #[test]
    fn resolve_playlist_rejects_an_ambiguous_name() {
        let playlists = vec![playlist("id-1", "Mix"), playlist("id-2", "mix")];
        // Two playlists collide case-insensitively and neither id was given.
        assert_eq!(
            resolve_playlist("MIX", &playlists),
            Err(PlaylistResolveError::Ambiguous("MIX".to_owned()))
        );
    }

    #[test]
    fn a_scoped_run_never_deletes_orphans() {
        // THE deletion-safety guard: a bare `--playlist X` resolves to Copy (no
        // `--mode`), so no source is a Mirror and reconciling it against a
        // manifest full of orphans yields zero deletes.
        use suno_core::{
            AudioFormat, LocalFile, Manifest, ManifestEntry, SourceMode, SourceStatus, reconcile,
        };
        let selection = resolve_selection(
            SourceMode::Mirror,
            None,
            false,
            &["holiday".to_owned()],
            None,
            false,
        );
        assert_eq!(
            selection.playlists,
            PlaylistPolicy::Explicit(vec![("holiday".to_owned(), SourceMode::Copy)])
        );
        assert!(selection.library.is_none(), "no protector without a mirror");
        assert!(!selection.is_armed());

        let mut manifest = Manifest::new();
        for i in 0..5 {
            let id = format!("orphan-{i}");
            manifest.insert(
                &id,
                ManifestEntry {
                    path: format!("{id}.flac"),
                    format: AudioFormat::Flac,
                    size: 100,
                    ..Default::default()
                },
            );
        }
        // The one playlist source is Copy, so deletion is never allowed.
        let sources = vec![SourceStatus {
            mode: SourceMode::Copy,
            fully_enumerated: true,
        }];
        let local: HashMap<String, LocalFile> = HashMap::new();
        let plan = reconcile(&manifest, &[], &local, &sources);
        assert_eq!(plan.deletes(), 0);
    }

    #[test]
    fn mass_delete_abort_fires_on_empty_listing() {
        // Desired empty but deletions pending against a non-empty manifest.
        assert!(mass_delete_abort(0, 147, 147, 1, false, false));
    }

    #[test]
    fn mass_delete_abort_skips_when_nothing_deleted() {
        assert!(!mass_delete_abort(0, 147, 0, 1, false, false));
    }

    #[test]
    fn mass_delete_abort_skips_empty_manifest() {
        assert!(!mass_delete_abort(0, 0, 0, 1, false, false));
    }

    #[test]
    fn empty_listing_waiver_requires_explicit_cli_min_newest() {
        // A min_newest=0 resolved from config/env plus --yes must NOT waive an
        // empty listing: the guard would otherwise be permanently disarmed.
        assert!(mass_delete_abort(0, 147, 147, 0, false, true));
        // Only an explicit per-invocation --min-newest 0 together with --yes
        // waives the empty-listing catastrophe.
        assert!(!mass_delete_abort(0, 147, 147, 0, true, true));
        // Explicit --min-newest 0 alone, without --yes, still aborts.
        assert!(mass_delete_abort(0, 147, 147, 0, true, false));
    }

    #[test]
    fn large_fraction_waiver_accepts_resolved_min_newest_zero() {
        // The large-fraction guard (desired > 0) stays waivable by the resolved
        // setting, so a configured min_newest=0 plus --yes is enough.
        assert!(!mass_delete_abort(2, 10, 5, 0, false, true));
        // Without --yes it still aborts.
        assert!(mass_delete_abort(2, 10, 5, 0, false, false));
        // And --yes without min_newest=0 still aborts.
        assert!(mass_delete_abort(2, 10, 5, 1, false, true));
    }

    #[test]
    fn mass_delete_abort_large_fraction() {
        // Deleting half or more of a non-trivial manifest, even with some desired.
        assert!(mass_delete_abort(2, 10, 5, 1, false, false));
        assert!(mass_delete_abort(3, 10, 6, 1, false, false));
    }

    #[test]
    fn mass_delete_abort_small_fraction_ok() {
        // A couple of deletions out of many is normal churn, not a wipe.
        assert!(!mass_delete_abort(98, 100, 2, 1, false, false));
    }

    #[test]
    fn mass_delete_abort_small_library_below_floor() {
        // Below the floor only the empty-listing rule applies, not the fraction.
        assert!(!mass_delete_abort(2, 4, 2, 1, false, false));
        assert!(mass_delete_abort(0, 4, 4, 1, false, false));
    }

    #[test]
    fn mass_delete_abort_counts_audio_and_artifact_deletes_together() {
        use suno_core::{Action, ArtifactKind, Plan};
        // HARDENING B2: the cap counts every destructive action. Three audio
        // deletes plus three sidecar deletes is 6 of a 10-entry manifest, over
        // the half threshold; the audio deletes alone (3 of 10) are under it.
        let del = |id: &str| Action::Delete {
            path: format!("{id}.flac"),
            clip_id: id.to_owned(),
        };
        let del_art = |id: &str| Action::DeleteArtifact {
            kind: ArtifactKind::CoverJpg,
            path: format!("{id}/cover.jpg"),
            owner_id: id.to_owned(),
        };
        let plan = Plan {
            actions: vec![
                del("a"),
                del("b"),
                del("c"),
                del_art("a"),
                del_art("b"),
                del_art("c"),
            ],
        };
        // run.rs feeds exactly this sum into the cap.
        let delete_count = plan.deletes() + plan.artifact_deletes();
        assert_eq!(delete_count, 6);
        assert!(mass_delete_abort(7, 10, delete_count, 1, false, false));
        // The audio deletes on their own would not trip it.
        assert_eq!(plan.deletes(), 3);
        assert!(!mass_delete_abort(7, 10, plan.deletes(), 1, false, false));
    }

    #[test]
    fn mass_delete_abort_fires_on_sidecar_only_mass_delete() {
        use suno_core::{Action, ArtifactKind, Plan};
        // A run with no audio deletes but a mass of removed-kind sidecar deletes
        // (5 of 10) still aborts once run.rs folds them into the count.
        let plan = Plan {
            actions: (0..5)
                .map(|i| Action::DeleteArtifact {
                    kind: ArtifactKind::CoverJpg,
                    path: format!("clip{i}/cover.jpg"),
                    owner_id: format!("clip{i}"),
                })
                .collect(),
        };
        let delete_count = plan.deletes() + plan.artifact_deletes();
        assert_eq!(plan.deletes(), 0);
        assert_eq!(delete_count, 5);
        assert!(mass_delete_abort(9, 10, delete_count, 1, false, false));
    }

    #[test]
    fn artifact_deletes_on_incomplete_listing_never_reach_the_cap() {
        use suno_core::{
            Action, ArtifactState, AudioFormat, LocalFile, Manifest, ManifestEntry, SourceMode,
            SourceStatus, reconcile,
        };
        // End-to-end B2: a manifest full of sidecars whose clips are all absent
        // from an INCOMPLETE listing must yield zero deletes of either kind, so
        // the count run.rs hands the cap is 0 and no wipe is possible.
        let mut manifest = Manifest::new();
        for i in 0..10 {
            let id = format!("c{i}");
            manifest.insert(
                &id,
                ManifestEntry {
                    path: format!("{id}.flac"),
                    format: AudioFormat::Flac,
                    size: 100,
                    cover_jpg: Some(ArtifactState {
                        path: format!("{id}/cover.jpg"),
                        hash: "h".to_owned(),
                    }),
                    ..Default::default()
                },
            );
        }
        let sources = vec![SourceStatus {
            mode: SourceMode::Mirror,
            fully_enumerated: false,
        }];
        let local: HashMap<String, LocalFile> = HashMap::new();
        let plan = reconcile(&manifest, &[], &local, &sources);
        // Nothing is deletable on an unreliable listing, sidecars included.
        assert_eq!(plan.deletes(), 0);
        assert_eq!(plan.artifact_deletes(), 0);
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::Delete { .. } | Action::DeleteArtifact { .. }))
        );
        let delete_count = plan.deletes() + plan.artifact_deletes();
        assert!(!mass_delete_abort(
            0,
            manifest.len(),
            delete_count,
            1,
            false,
            false
        ));
    }

    #[test]
    fn confirm_copy_never_prompts() {
        assert_eq!(confirm_decision(false, 9, false, true), Confirm::Proceed);
        assert_eq!(confirm_decision(false, 9, false, false), Confirm::Proceed);
    }

    #[test]
    fn confirm_sync_no_deletes_proceeds() {
        assert_eq!(confirm_decision(true, 0, false, false), Confirm::Proceed);
    }

    #[test]
    fn confirm_sync_yes_proceeds() {
        assert_eq!(confirm_decision(true, 3, true, false), Confirm::Proceed);
    }

    #[test]
    fn confirm_sync_tty_prompts() {
        assert_eq!(confirm_decision(true, 3, false, true), Confirm::Prompt);
    }

    #[test]
    fn confirm_sync_non_tty_refuses() {
        assert_eq!(
            confirm_decision(true, 3, false, false),
            Confirm::RefuseNonInteractive
        );
    }

    #[test]
    fn confirmed_accepts_y_and_yes() {
        assert!(confirmed("y"));
        assert!(confirmed("Y"));
        assert!(confirmed(" yes "));
        assert!(confirmed("YES"));
        assert!(!confirmed("n"));
        assert!(!confirmed(""));
        assert!(!confirmed("yeah"));
    }

    fn outcome(
        downloaded: usize,
        skipped: usize,
        failures: usize,
        status: RunStatus,
    ) -> ExecOutcome {
        ExecOutcome {
            downloaded,
            skipped,
            failures: (0..failures)
                .map(|i| Failure {
                    clip_id: format!("c{i}"),
                    reason: "boom".to_owned(),
                })
                .collect(),
            status,
            ..Default::default()
        }
    }

    #[test]
    fn exit_code_auth_abort() {
        let o = outcome(3, 0, 1, RunStatus::AuthAborted);
        assert_eq!(run_exit_code(&o), ExitCode::Auth);
    }

    #[test]
    fn exit_code_disk_full_abort() {
        // A disk-full abort maps to 9, ahead of the failures-based partial logic
        // even though one clip is recorded as failed.
        let o = outcome(3, 0, 1, RunStatus::DiskFull);
        assert_eq!(run_exit_code(&o), ExitCode::DiskFull);
    }

    #[test]
    fn exit_code_clean_run() {
        let o = outcome(12, 100, 0, RunStatus::Completed);
        assert_eq!(run_exit_code(&o), ExitCode::Ok);
    }

    #[test]
    fn exit_code_partial_when_some_progress() {
        let o = outcome(10, 0, 2, RunStatus::Completed);
        assert_eq!(run_exit_code(&o), ExitCode::Partial);
    }

    #[test]
    fn exit_code_partial_counts_skips_as_progress() {
        let o = outcome(0, 5, 2, RunStatus::Completed);
        assert_eq!(run_exit_code(&o), ExitCode::Partial);
    }

    #[test]
    fn exit_code_transient_when_nothing_progressed() {
        let o = outcome(0, 0, 5, RunStatus::Completed);
        assert_eq!(run_exit_code(&o), ExitCode::Transient);
    }

    #[test]
    fn exit_code_values_match_spec() {
        assert_eq!(ExitCode::Ok.code(), 0);
        assert_eq!(ExitCode::General.code(), 1);
        assert_eq!(ExitCode::Usage.code(), 2);
        assert_eq!(ExitCode::Config.code(), 3);
        assert_eq!(ExitCode::Auth.code(), 4);
        assert_eq!(ExitCode::Partial.code(), 5);
        assert_eq!(ExitCode::Transient.code(), 6);
        assert_eq!(ExitCode::Safety.code(), 7);
        assert_eq!(ExitCode::Interrupted.code(), 8);
        assert_eq!(ExitCode::DiskFull.code(), 9);
    }
}
