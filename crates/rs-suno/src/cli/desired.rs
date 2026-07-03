//! Pure decision logic for the sync/copy/check engine.
//!
//! Everything here is a pure function of its inputs: building the desired target
//! state from selected clips, the deletion-safety abort, the destructive-sync
//! confirmation gate, and the mapping from an [`ExecOutcome`] to a process exit
//! code. Keeping these out of the IO orchestration lets the safety-critical
//! rules be unit-tested directly, which is where the risk lives.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path};

use suno_core::{
    AreaMode, AreasConfig, ArtifactKind, AudioFormat, Clip, Desired, DesiredArtifact, ExecOutcome,
    LineageContext, M3u8Entry, NamingConfig, NamingRequest, Playlist, PlaylistDesired, RunStatus,
    SourceMode, art_hash, art_url_hash, content_hash, meta_hash, render_clip_details,
    render_clip_lyrics, render_clip_names, render_m3u8, sanitise_name, synced_lrc_source_hash,
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

/// The per-song sidecar toggles resolved for a run.
///
/// Each mirrors one resolved setting: `animated_covers` gates the `cover.webp`,
/// `details` the `.details.txt` dump, `lyrics` the `.lyrics.txt` file, `lrc`
/// the synced `.lrc` sidecar (Suno's word/line-level timed lyrics, which also
/// drives the MP3 `SYLT` frame and the plain lyric tag), and `video` the
/// standalone `.mp4` music video. All default off, matching the compiled config
/// defaults.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArtifactToggles {
    pub animated_covers: bool,
    pub details: bool,
    pub lyrics: bool,
    pub lrc: bool,
    pub video: bool,
}

/// Build the desired target state for a union of selected clips.
///
/// Naming is rendered as a batch so collisions are disambiguated globally in one
/// pass, then the target format's extension is appended. Each clip's `modes` is
/// stamped from `modes_by_id`: the list of every selected area (mirror and copy
/// alike) that currently holds that clip. A clip held by a `Mirror` and a `Copy`
/// area at once therefore carries both, so copy-wins protection (SYNC-8) holds.
///
/// Every clip in `clips` must have an entry in `modes_by_id` (the caller builds
/// the map from the same union), so `modes` is never empty; an empty `modes`
/// would silently drop that clip's copy protection, so it trips a `debug_assert`
/// (D6). In a release build a clip missing from the map defaults to an empty
/// list, which reconcile then treats as unprotected, so callers must never omit
/// a clip from the map.
///
/// `contexts` carries the resolved [`LineageContext`] for each clip (keyed by
/// clip id); it drives the album component, the embedded lineage tags, and the
/// change hash, so the same resolved values flow all the way to the executor. A
/// clip missing from `contexts` falls back to a self-rooted context.
///
/// `colliding_albums` is the store's authoritative set of root titles shared by
/// more than one distinct root; a clip whose album is in that set is folded into
/// a `[{root_id8}]`-suffixed folder so two distinct roots never share one,
/// regardless of which clips this batch happens to hold.
///
/// `toggles` carries the resolved per-song sidecar switches (animated cover,
/// details text, lyrics text); each gates the matching sidecar in
/// [`clip_artifacts`].
pub fn build_desired(
    clips: &[&Clip],
    format: AudioFormat,
    modes_by_id: &HashMap<String, Vec<SourceMode>>,
    contexts: &HashMap<String, LineageContext>,
    colliding_albums: &BTreeSet<String>,
    toggles: ArtifactToggles,
    naming: &NamingConfig,
) -> Vec<Desired> {
    let lineages: Vec<LineageContext> = clips
        .iter()
        .map(|clip| {
            contexts
                .get(&clip.id)
                .cloned()
                .unwrap_or_else(|| LineageContext::own_root(clip))
        })
        .collect();
    // The requests borrow `lineages`; scope them so the borrow ends before the
    // lineages are moved into the desired entries below.
    let names = {
        let requests: Vec<NamingRequest<'_>> = clips
            .iter()
            .zip(&lineages)
            .map(|(clip, lineage)| NamingRequest { clip, lineage })
            .collect();
        render_clip_names(&requests, naming, colliding_albums)
    };

    clips
        .iter()
        .zip(names)
        .zip(lineages)
        .map(|((clip, name), lineage)| {
            // The extensionless audio path; the sidecars swap the extension.
            let base = rel_to_string(&name.relative_path);
            let path = format!("{base}.{format}");
            let meta_hash = meta_hash(clip, &lineage);
            let modes = modes_by_id.get(&clip.id).cloned().unwrap_or_default();
            // D6: an empty modes vec would silently lose SYNC-8 copy protection
            // for this clip, so the caller must always list at least one area.
            debug_assert!(
                !modes.is_empty(),
                "clip {} has no modes in the union map",
                clip.id
            );
            // Bind the artifacts before the struct literal so `&lineage` is
            // borrowed (for the details render) before it is moved in below.
            let artifacts = clip_artifacts(clip, &base, &lineage, toggles);
            Desired {
                clip: (*clip).clone(),
                lineage,
                path,
                format,
                meta_hash,
                art_hash: art_hash(clip),
                modes,
                trashed: false,
                private: false,
                artifacts,
            }
        })
        .collect()
}

/// The per-clip sidecars desired alongside `base`, the extensionless audio path
/// (so each sidecar sits next to the audio file).
///
/// A static `CoverJpg` is emitted whenever the clip has non-empty selected art;
/// an animated `CoverWebp` only when `toggles.animated_covers` is set and the
/// clip carries a video preview. An empty art URL emits NO `CoverJpg`: reconcile
/// reads a desired that simply lacks a cover as UNKNOWN => KEEP, never a delete,
/// so a transient empty URL cannot strand or remove an existing cover. The
/// `CoverJpg` hash tracks the art URL (`art_hash`); the `CoverWebp` hash tracks
/// the video URL, so a changed source re-transcodes.
///
/// The generated text sidecars carry their body inline (`content`) and a
/// per-sidecar `content_hash`, so a change to what the file holds (a retitle for
/// details, or edited lyrics) rewrites it even when `meta_hash` is unchanged.
/// `DetailsTxt` is always emitted when `toggles.details` is set (the render is
/// total); `LyricsTxt` only when `toggles.lyrics` is set and the clip has
/// non-empty lyrics (the render is partial), so no empty lyrics file is written.
/// The synced `Lrc` is emitted under `toggles.lrc` for every clip (alignment
/// availability is knowable only from the endpoint, not the feed), carrying a
/// source-proxy hash and no inline body; its timed body is resolved from the
/// fetched alignment just before execution, and a clip with neither alignment
/// nor lyrics writes no file (its emptiness cached so it is not re-fetched).
fn clip_artifacts(
    clip: &Clip,
    base: &str,
    lineage: &LineageContext,
    toggles: ArtifactToggles,
) -> Vec<DesiredArtifact> {
    let mut artifacts = Vec::new();
    if let Some(url) = clip.selected_image_url().filter(|u| !u.is_empty()) {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::CoverJpg,
            path: format!("{base}.jpg"),
            source_url: url.to_owned(),
            hash: art_hash(clip),
            content: None,
        });
    }
    if toggles.animated_covers && !clip.video_cover_url.is_empty() {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::CoverWebp,
            path: format!("{base}.webp"),
            source_url: clip.video_cover_url.clone(),
            hash: art_url_hash(&clip.video_cover_url),
            content: None,
        });
    }
    if toggles.details {
        let text = render_clip_details(clip, lineage);
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::DetailsTxt,
            path: format!("{base}.details.txt"),
            source_url: String::new(),
            hash: content_hash(&text),
            content: Some(text),
        });
    }
    if toggles.lyrics
        && let Some(text) = render_clip_lyrics(clip)
    {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::LyricsTxt,
            path: format!("{base}.lyrics.txt"),
            source_url: String::new(),
            hash: content_hash(&text),
            content: Some(text),
        });
    }
    if toggles.lrc {
        // Emitted for every clip: alignment availability is knowable only from
        // the endpoint, not the feed (a clip can carry neither `lyrics` nor a
        // `prompt` yet still have full word/line alignment), so the fetch itself
        // decides. The artifact carries no inline body and a source-proxy hash
        // keyed on the (immutable) clip id plus the render version, so reconcile
        // skips an unchanged clip with no fetch while a version bump rewrites
        // every sidecar. The body is resolved just before execution (the untimed
        // lyrics when Suno has no alignment); a clip with neither alignment nor
        // lyrics resolves to nothing and writes no `.lrc`, its emptiness cached
        // on the manifest so it is not re-fetched every run.
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::Lrc,
            path: format!("{base}.lrc"),
            source_url: String::new(),
            hash: synced_lrc_source_hash(&clip.id),
            content: None,
        });
    }
    if toggles.video && !clip.video_url.is_empty() {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::VideoMp4,
            path: format!("{base}.mp4"),
            source_url: clip.video_url.clone(),
            hash: art_url_hash(&clip.video_url),
            content: None,
        });
    }
    artifacts
}

/// The synthetic playlist id for the liked feed, rendered as "Liked Songs".
///
/// Suno playlist ids are UUIDs, so this short literal never collides with a real
/// playlist id in the store keyspace.
pub const LIKED_PLAYLIST_ID: &str = "liked";

/// One fetched playlist to render: its stable id, display name, and ordered
/// member clips (already non-trashed, in Suno order).
pub struct PlaylistInput<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub members: &'a [Clip],
}

/// Build the desired `.m3u8` playlists for this run from the fetched playlists.
///
/// Each input is rendered, in Suno order, into an extended-M3U8 body: every
/// member clip id is looked up in this run's `desired` audio set and mapped to
/// its rendered relative path, title, and duration. A member **absent from the
/// desired set** is emitted as an L1 `# (not in library)` comment (an empty
/// relative path in the [`M3u8Entry`]), using the member's own title, rather
/// than a dangling path (HARDENING L1). The content hash is taken over the full
/// rendered body so a name, order, path, title, or duration change all trigger a
/// rewrite (HARDENING B1), and the file path is `<sanitised name>.m3u8` at the
/// library root.
///
/// This is pure; the caller (run) does the best-effort fetching, excludes any
/// playlist whose member fetch failed, and appends the synthetic liked feed as a
/// final input with id [`LIKED_PLAYLIST_ID`].
pub fn build_playlist_desired(
    inputs: &[PlaylistInput<'_>],
    desired: &[Desired],
) -> Vec<PlaylistDesired> {
    let by_id: HashMap<&str, &Desired> = desired.iter().map(|d| (d.clip.id.as_str(), d)).collect();
    inputs
        .iter()
        .map(|input| {
            let entries: Vec<M3u8Entry<'_>> = input
                .members
                .iter()
                .map(|member| match by_id.get(member.id.as_str()) {
                    Some(d) => M3u8Entry {
                        title: d.clip.title.as_str(),
                        duration_secs: d.clip.duration,
                        relative_path: d.path.as_str(),
                    },
                    None => M3u8Entry {
                        title: member.title.as_str(),
                        duration_secs: member.duration,
                        relative_path: "",
                    },
                })
                .collect();
            let content = render_m3u8(input.name, &entries);
            let hash = content_hash(&content);
            let path = format!("{}.m3u8", sanitise_name(input.name));
            PlaylistDesired {
                id: input.id.to_owned(),
                name: input.name.to_owned(),
                path,
                content,
                hash,
            }
        })
        .collect()
}

/// Render a relative path as a forward-slash string, dropping any non-normal
/// component so the stored path is portable and never escapes the root.
fn rel_to_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether a source counts as fully enumerated for deletion safety.
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

    fn clip(id: &str, title: &str, handle: &str) -> Clip {
        Clip {
            id: id.to_owned(),
            title: title.to_owned(),
            handle: handle.to_owned(),
            display_name: handle.to_owned(),
            ..Default::default()
        }
    }

    fn no_contexts() -> HashMap<String, LineageContext> {
        HashMap::new()
    }

    fn no_collisions() -> BTreeSet<String> {
        BTreeSet::new()
    }

    /// Assign every clip a single uniform mode, mirroring an area's union map.
    fn modes_for(clips: &[&Clip], mode: SourceMode) -> HashMap<String, Vec<SourceMode>> {
        clips.iter().map(|c| (c.id.clone(), vec![mode])).collect()
    }

    #[test]
    fn build_desired_appends_extension_and_mode() {
        let a = clip("id-a", "Song A", "alice");
        let clips = [&a];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert_eq!(desired.len(), 1);
        assert!(
            desired[0].path.ends_with(".flac"),
            "path: {}",
            desired[0].path
        );
        assert_eq!(desired[0].format, AudioFormat::Flac);
        assert_eq!(desired[0].modes, vec![SourceMode::Mirror]);
        assert!(!desired[0].trashed);
        assert!(!desired[0].private);
        let lineage = LineageContext::own_root(&a);
        assert_eq!(desired[0].meta_hash, meta_hash(&a, &lineage));
        assert_eq!(desired[0].art_hash, art_hash(&a));
        // A clip absent from the contexts map is treated as its own root.
        assert_eq!(desired[0].lineage, lineage);
    }

    #[test]
    fn build_desired_uses_supplied_lineage_context() {
        let a = clip("child-1", "Remix", "alice");
        let clips = [&a];
        let lineage = LineageContext {
            root_id: "root-1".to_owned(),
            root_title: "Original".to_owned(),
            root_date: String::new(),
            parent_id: "root-1".to_owned(),
            edge_type: None,
            status: suno_core::ResolveStatus::Resolved,
        };
        let contexts: HashMap<String, LineageContext> =
            [(a.id.clone(), lineage.clone())].into_iter().collect();
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &contexts,
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        // The album folders under the root title, and the hash/lineage carry the
        // resolved context, not a self-rooted fallback.
        assert!(
            desired[0].path.contains("/Original/"),
            "path: {}",
            desired[0].path
        );
        assert_eq!(desired[0].lineage, lineage);
        assert_eq!(desired[0].meta_hash, meta_hash(&a, &lineage));
    }

    #[test]
    fn lineage_is_stable_when_a_later_resolution_fails() {
        // HARDENING H3: album folders and the change hash come from the durable
        // store, not the live per-run resolution, so a second cycle whose
        // resolver dropped (or whose ancestor was purged) must not move a file
        // or force a retag. This drives the exact build_desired path the run
        // flow uses, only swapping the store update for a no-op on cycle 2.
        use suno_core::{LineageStore, Resolution, ResolveStatus, RootInfo};

        let root = Clip {
            id: "root-break".into(),
            title: "Break Through".into(),
            clip_type: "gen".into(),
            handle: "alice".into(),
            display_name: "alice".into(),
            ..Default::default()
        };
        let child = Clip {
            id: "child-remix".into(),
            title: "Remix".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            cover_clip_id: "root-break".into(),
            edited_clip_id: "root-break".into(),
            handle: "alice".into(),
            display_name: "alice".into(),
            ..Default::default()
        };
        let clips = [&root, &child];

        let contexts_of = |store: &LineageStore| -> HashMap<String, LineageContext> {
            clips
                .iter()
                .map(|c| (c.id.clone(), store.context_for(c)))
                .collect()
        };

        // Cycle 1: the resolver succeeds and the store is updated in memory.
        let mut roots = HashMap::new();
        for id in ["root-break", "child-remix"] {
            roots.insert(
                id.to_owned(),
                RootInfo {
                    root_id: "root-break".into(),
                    root_title: "Break Through".into(),
                    status: ResolveStatus::Resolved,
                },
            );
        }
        let resolution = Resolution {
            roots,
            gap_filled: Vec::new(),
        };
        let mut store = LineageStore::new();
        store.update(&[root.clone(), child.clone()], &resolution, "t1");

        let cycle1 = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &contexts_of(&store),
            &store.colliding_root_titles(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        let child1 = cycle1.iter().find(|d| d.clip.id == "child-remix").unwrap();
        assert!(
            child1.path.contains("/Break Through/"),
            "the remix should folder under its root album, got {}",
            child1.path
        );

        // Cycle 2: the resolver failed, so the persisted store is used as-is.
        let cycle2 = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &contexts_of(&store),
            &store.colliding_root_titles(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        for (a, b) in cycle1.iter().zip(&cycle2) {
            assert_eq!(a.path, b.path, "album path drifted for {}", a.clip.id);
            assert_eq!(
                a.meta_hash, b.meta_hash,
                "meta_hash drifted for {}",
                a.clip.id
            );
        }

        // The bug this guards against: the old own-root fallback on a dropped
        // resolution would fold the child under its OWN title and rewrite its
        // hash, i.e. exactly the rename/retag storm H3 forbids.
        let own = LineageContext::own_root(&child);
        assert_ne!(
            meta_hash(&child, &own),
            child1.meta_hash,
            "own-root fallback must differ from the store-driven hash"
        );
    }

    #[test]
    fn build_desired_disambiguates_collisions() {
        // Two clips with identical naming inputs must not share a path.
        let a = clip("id-a", "Same", "alice");
        let b = clip("id-b", "Same", "alice");
        let clips = [&a, &b];
        let desired = build_desired(
            &clips,
            AudioFormat::Mp3,
            &modes_for(&clips, SourceMode::Copy),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert_ne!(desired[0].path, desired[1].path);
        assert!(desired.iter().all(|d| d.path.ends_with(".mp3")));
        assert!(desired.iter().all(|d| d.modes == vec![SourceMode::Copy]));
    }

    #[test]
    fn build_desired_uses_forward_slashes() {
        let a = clip("id-a", "Song A", "alice");
        let clips = [&a];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(!desired[0].path.contains('\\'));
        assert!(desired[0].path.contains('/'));
    }

    fn art_clip(id: &str) -> Clip {
        Clip {
            image_large_url: format!("https://art.suno.ai/{id}/large.jpg"),
            ..clip(id, "Song", "alice")
        }
    }

    #[test]
    fn build_desired_emits_cover_jpg_next_to_audio() {
        // A clip with art gains a single CoverJpg whose path is the audio path
        // with a .jpg extension, sourced from the selected image and hashed by
        // art_hash. No CoverWebp without --animated-covers.
        let a = art_clip("id-a");
        let clips = [&a];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        let base = desired[0].path.strip_suffix(".flac").unwrap();
        assert_eq!(desired[0].artifacts.len(), 1);
        let jpg = &desired[0].artifacts[0];
        assert_eq!(jpg.kind, ArtifactKind::CoverJpg);
        assert_eq!(jpg.path, format!("{base}.jpg"));
        assert_eq!(jpg.source_url, a.selected_image_url().unwrap());
        assert_eq!(jpg.hash, art_hash(&a));
    }

    #[test]
    fn build_desired_omits_cover_jpg_when_art_is_empty() {
        // No selected art (all image/video URLs empty) => NO CoverJpg. Reconcile
        // reads the absence as UNKNOWN => KEEP, so a transient empty URL never
        // deletes an existing cover.
        let a = clip("id-a", "Song", "alice");
        assert!(a.selected_image_url().is_none());
        let clips = [&a];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                animated_covers: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        assert!(desired[0].artifacts.is_empty());
    }

    #[test]
    fn build_desired_emits_cover_webp_only_when_animated_and_video_present() {
        let with_video = Clip {
            video_cover_url: "https://cdn.suno.ai/id-a/video.mp4".to_owned(),
            ..art_clip("id-a")
        };
        let clips = [&with_video];

        // Off by default: only the static cover, even with a video present.
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert_eq!(desired[0].artifacts.len(), 1);
        assert_eq!(desired[0].artifacts[0].kind, ArtifactKind::CoverJpg);

        // Enabled with a video: a CoverWebp joins the CoverJpg, pathed .webp,
        // sourced from the video URL and hashed by art_url_hash.
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                animated_covers: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = desired[0].path.strip_suffix(".flac").unwrap();
        let webp = desired[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::CoverWebp)
            .expect("animated cover expected");
        assert_eq!(webp.path, format!("{base}.webp"));
        assert_eq!(webp.source_url, with_video.video_cover_url);
        assert_eq!(webp.hash, art_url_hash(&with_video.video_cover_url));

        // Enabled but no video: no CoverWebp is emitted.
        let no_video = art_clip("id-b");
        let clips = [&no_video];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                animated_covers: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        assert!(
            desired[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::CoverWebp)
        );
    }

    #[test]
    fn build_desired_emits_video_mp4_only_when_enabled_and_video_present() {
        let with_video = Clip {
            video_url: "https://cdn.suno.ai/id-a/video.mp4".to_owned(),
            ..art_clip("id-a")
        };
        let clips = [&with_video];

        // Off by default: no standalone video, even when the clip has one.
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(
            desired[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::VideoMp4)
        );

        // Enabled with a video: a VideoMp4 joins, pathed .mp4, sourced from
        // video_url and hashed by art_url_hash (a fetched binary, no inline body).
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                video: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = desired[0].path.strip_suffix(".flac").unwrap();
        let video = desired[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::VideoMp4)
            .expect("video expected");
        assert_eq!(video.path, format!("{base}.mp4"));
        assert_eq!(video.source_url, with_video.video_url);
        assert_eq!(video.hash, art_url_hash(&with_video.video_url));
        assert!(video.content.is_none());

        // Enabled but the clip has no video url: no VideoMp4 emitted.
        let no_video = art_clip("id-b");
        let clips = [&no_video];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                video: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        assert!(
            desired[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::VideoMp4)
        );
    }

    #[test]
    fn build_desired_emits_details_sidecar_only_when_enabled() {
        let a = clip("id-a", "Song", "alice");
        let clips = [&a];

        // Off by default: no details sidecar.
        let off = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(
            off[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::DetailsTxt)
        );

        // Enabled: a DetailsTxt is emitted next to the audio, with inline content
        // and a content hash, and never a source URL.
        let on = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                details: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = on[0].path.strip_suffix(".flac").unwrap();
        let details = on[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::DetailsTxt)
            .expect("details sidecar expected");
        assert_eq!(details.path, format!("{base}.details.txt"));
        assert_eq!(details.source_url, "");
        let body = render_clip_details(&a, &LineageContext::own_root(&a));
        assert_eq!(details.content.as_deref(), Some(body.as_str()));
        assert_eq!(details.hash, content_hash(&body));
    }

    #[test]
    fn build_desired_emits_lyrics_sidecar_only_when_enabled_and_present() {
        let with_lyrics = Clip {
            lyrics: "la la la".to_owned(),
            ..clip("id-a", "Song", "alice")
        };
        let clips = [&with_lyrics];

        // Off by default: no lyrics sidecar even when the clip has lyrics.
        let off = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(
            off[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::LyricsTxt)
        );

        // Enabled with lyrics: a LyricsTxt is emitted with the verbatim lyrics.
        let on = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                lyrics: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = on[0].path.strip_suffix(".flac").unwrap();
        let lyrics = on[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::LyricsTxt)
            .expect("lyrics sidecar expected");
        assert_eq!(lyrics.path, format!("{base}.lyrics.txt"));
        assert_eq!(lyrics.source_url, "");
        assert_eq!(lyrics.content.as_deref(), Some("la la la\n"));
        assert_eq!(lyrics.hash, content_hash("la la la\n"));
    }

    #[test]
    fn build_desired_emits_lrc_sidecar_only_when_enabled() {
        let with_lyrics = Clip {
            lyrics: "la la la".to_owned(),
            ..clip("id-a", "Song", "alice")
        };
        let clips = [&with_lyrics];

        // Off by default: no lrc sidecar even when the clip has lyrics.
        let off = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert!(
            off[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::Lrc)
        );

        // Enabled with a lyric signal: a synced Lrc is emitted next to the audio
        // with a source-proxy hash and NO inline content (the timed body is
        // resolved from the fetched alignment just before execution).
        let on = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                lrc: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = on[0].path.strip_suffix(".flac").unwrap();
        let lrc = on[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::Lrc)
            .expect("lrc sidecar expected");
        assert_eq!(lrc.path, format!("{base}.lrc"));
        assert_eq!(lrc.source_url, "");
        assert_eq!(lrc.content, None);
        assert_eq!(lrc.hash, synced_lrc_source_hash(&with_lyrics.id));
    }

    #[test]
    fn build_desired_emits_lrc_sidecar_from_prompt_when_feed_omits_lyrics() {
        // The v3 feed omits per-clip lyrics but carries the prompt; a clip with
        // only a prompt is still a synced-lyrics candidate, so a proxy-hashed Lrc
        // is emitted (its body is fetched later).
        let prompt_only = Clip {
            prompt: "the sung words live here".to_owned(),
            ..clip("id-a", "Song", "alice")
        };
        assert!(prompt_only.lyrics.is_empty());
        let clips = [&prompt_only];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                lrc: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let lrc = desired[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::Lrc)
            .expect("lrc sidecar expected");
        assert_eq!(lrc.content, None);
        assert_eq!(lrc.hash, synced_lrc_source_hash(&prompt_only.id));
    }

    #[test]
    fn build_desired_emits_lrc_sidecar_even_when_feed_has_no_lyrics_or_prompt() {
        // A clip can carry neither `lyrics` nor a `prompt` in the feed yet still
        // have full word/line alignment at the endpoint (observed live), so the
        // artifact must be emitted regardless; the fetch decides emptiness.
        let bare = clip("id-a", "Song", "alice");
        assert!(bare.lyrics.is_empty() && bare.prompt.is_empty());
        let clips = [&bare];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                lrc: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let lrc = desired[0]
            .artifacts
            .iter()
            .find(|art| art.kind == ArtifactKind::Lrc)
            .expect("lrc sidecar expected even with no feed lyrics/prompt");
        assert_eq!(lrc.content, None);
        assert_eq!(lrc.hash, synced_lrc_source_hash(&bare.id));
    }

    #[test]
    fn build_desired_omits_lyrics_sidecar_when_clip_has_no_lyrics() {
        // Enabled but the clip has empty lyrics: no LyricsTxt, so no empty file
        // is ever written. The render is partial, so absence is legitimate.
        let no_lyrics = clip("id-a", "Song", "alice");
        assert!(no_lyrics.lyrics.is_empty());
        let clips = [&no_lyrics];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                lyrics: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        assert!(
            desired[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::LyricsTxt)
        );
    }

    #[test]
    fn build_desired_text_sidecars_are_independent() {
        // Both toggles on with art and lyrics present: cover, details and lyrics
        // sidecars all appear, each at its own `<stem>.*` path.
        let full = Clip {
            lyrics: "words".to_owned(),
            ..art_clip("id-a")
        };
        let clips = [&full];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes_for(&clips, SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles {
                details: true,
                lyrics: true,
                ..Default::default()
            },
            &NamingConfig::default(),
        );
        let base = desired[0].path.strip_suffix(".flac").unwrap();
        let kinds: BTreeSet<ArtifactKind> = desired[0].artifacts.iter().map(|a| a.kind).collect();
        assert!(kinds.contains(&ArtifactKind::CoverJpg));
        assert!(kinds.contains(&ArtifactKind::DetailsTxt));
        assert!(kinds.contains(&ArtifactKind::LyricsTxt));
        let path_of = |k: ArtifactKind| {
            desired[0]
                .artifacts
                .iter()
                .find(|a| a.kind == k)
                .unwrap()
                .path
                .clone()
        };
        assert_eq!(
            path_of(ArtifactKind::DetailsTxt),
            format!("{base}.details.txt")
        );
        assert_eq!(
            path_of(ArtifactKind::LyricsTxt),
            format!("{base}.lyrics.txt")
        );
    }

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
    #[test]
    fn build_desired_one_pass_disambiguates_and_stamps_modes() {
        let a = clip("lib-1", "Song", "alice");
        let b = clip("pl-1", "Song", "alice");
        let clips = [&a, &b];
        let mut modes = HashMap::new();
        modes.insert("lib-1".to_owned(), vec![SourceMode::Copy]);
        modes.insert(
            "pl-1".to_owned(),
            vec![SourceMode::Mirror, SourceMode::Copy],
        );
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &modes,
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        assert_eq!(desired.len(), 2);
        assert_ne!(desired[0].path, desired[1].path);
        assert_eq!(desired[1].modes, vec![SourceMode::Mirror, SourceMode::Copy]);
    }

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
        use suno_core::{LocalFile, Manifest, ManifestEntry, SourceMode, SourceStatus, reconcile};
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
            Action, ArtifactState, LocalFile, Manifest, ManifestEntry, SourceMode, SourceStatus,
            reconcile,
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

    fn path_of<'a>(desired: &'a [Desired], id: &str) -> &'a str {
        desired
            .iter()
            .find(|d| d.clip.id == id)
            .map(|d| d.path.as_str())
            .expect("clip in desired set")
    }

    #[test]
    fn build_playlist_desired_orders_members_and_marks_absent() {
        let a = clip("id-a", "Song A", "alice");
        let b = clip("id-b", "Song B", "alice");
        let desired = build_desired(
            &[&a, &b],
            AudioFormat::Flac,
            &modes_for(&[&a, &b], SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        // A playlist with b, then a clip absent from the library, then a.
        let missing = clip("id-x", "Missing Song", "bob");
        let members = vec![b.clone(), missing.clone(), a.clone()];
        let inputs = vec![PlaylistInput {
            id: "pl1",
            name: "Road/Trip",
            members: &members,
        }];

        let out = build_playlist_desired(&inputs, &desired);
        assert_eq!(out.len(), 1);
        let pl = &out[0];
        assert_eq!(pl.id, "pl1");
        // The path is sanitised (slash folded); the #PLAYLIST body keeps the raw name.
        assert_eq!(pl.path, "Road Trip.m3u8");
        assert!(pl.content.starts_with("#EXTM3U\n#PLAYLIST:Road/Trip\n"));

        // Suno order is preserved: b, then the L1 comment, then a.
        let pos_b = pl.content.find(path_of(&desired, "id-b")).unwrap();
        let pos_missing = pl.content.find("# (not in library) Missing Song").unwrap();
        let pos_a = pl.content.find(path_of(&desired, "id-a")).unwrap();
        assert!(pos_b < pos_missing && pos_missing < pos_a);
        // The absent member is a comment, never a dangling path or EXTINF.
        assert!(!pl.content.contains("Missing Song\nbob/"));
        // The hash is over the full rendered body (B1).
        assert_eq!(pl.hash, content_hash(&pl.content));
    }

    #[test]
    fn build_playlist_desired_builds_liked_and_multiple_in_order() {
        let a = clip("id-a", "Song A", "alice");
        let desired = build_desired(
            &[&a],
            AudioFormat::Flac,
            &modes_for(&[&a], SourceMode::Mirror),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &NamingConfig::default(),
        );
        let members = vec![a.clone()];
        let inputs = vec![
            PlaylistInput {
                id: "pl1",
                name: "First",
                members: &members,
            },
            PlaylistInput {
                id: LIKED_PLAYLIST_ID,
                name: "Liked Songs",
                members: &members,
            },
        ];

        let out = build_playlist_desired(&inputs, &desired);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "pl1");
        assert_eq!(out[1].id, LIKED_PLAYLIST_ID);
        assert_eq!(out[1].path, "Liked Songs.m3u8");
        // Both reference the in-library audio path.
        assert!(out[0].content.contains(path_of(&desired, "id-a")));
        assert!(out[1].content.contains(path_of(&desired, "id-a")));
    }

    #[test]
    fn build_playlist_desired_is_empty_for_no_inputs() {
        assert!(build_playlist_desired(&[], &[]).is_empty());
    }

    #[test]
    fn build_desired_respects_custom_naming_config() {
        use suno_core::CharacterSet;

        let a = clip("abcdefgh-1234", "Song A", "alice");
        let clips = [&a];
        let custom = NamingConfig {
            template: "{title}/{id8}".to_owned(),
            character_set: CharacterSet::Ascii,
            ..NamingConfig::default()
        };
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            &HashMap::from([("abcdefgh-1234".to_owned(), vec![SourceMode::Mirror])]),
            &no_contexts(),
            &no_collisions(),
            ArtifactToggles::default(),
            &custom,
        );
        // The custom template places the title as a directory and id8 as the
        // file stem, different from the default layout.
        assert!(
            desired[0].path.starts_with("Song A/"),
            "path: {}",
            desired[0].path
        );
        assert!(desired[0].path.contains(&a.id[..8]));
    }
}
