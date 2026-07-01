//! Pure decision logic for the sync/copy/check engine.
//!
//! Everything here is a pure function of its inputs: building the desired target
//! state from selected clips, the deletion-safety abort, the destructive-sync
//! confirmation gate, and the mapping from an [`ExecOutcome`] to a process exit
//! code. Keeping these out of the IO orchestration lets the safety-critical
//! rules be unit-tested directly, which is where the risk lives.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path};

use suno_core::{
    ArtifactKind, AudioFormat, Clip, Desired, DesiredArtifact, ExecOutcome, LineageContext,
    M3u8Entry, NamingConfig, NamingRequest, PlaylistDesired, RunStatus, SourceMode, art_hash,
    art_url_hash, content_hash, meta_hash, render_clip_names, render_m3u8, sanitise_name,
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

/// Build the desired target state for one source's selected clips.
///
/// Naming is rendered as a batch so collisions are disambiguated, then the
/// target format's extension is appended. `mode` is the source kind: a `sync`
/// verb yields [`SourceMode::Mirror`], a `copy` verb [`SourceMode::Copy`].
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
/// `animated_covers` mirrors the resolved `--animated-covers` setting: when set,
/// a clip with a video preview also gains a `cover.webp` sidecar (see
/// [`clip_artifacts`]).
pub fn build_desired(
    clips: &[&Clip],
    format: AudioFormat,
    mode: SourceMode,
    contexts: &HashMap<String, LineageContext>,
    colliding_albums: &BTreeSet<String>,
    animated_covers: bool,
) -> Vec<Desired> {
    let config = NamingConfig::default();
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
        render_clip_names(&requests, &config, colliding_albums)
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
            Desired {
                clip: (*clip).clone(),
                lineage,
                path,
                format,
                meta_hash,
                art_hash: art_hash(clip),
                modes: vec![mode],
                trashed: false,
                private: false,
                artifacts: clip_artifacts(clip, &base, animated_covers),
            }
        })
        .collect()
}

/// The per-clip cover sidecars desired alongside `base`, the extensionless audio
/// path (so `cover.jpg` and `cover.webp` sit next to the audio file).
///
/// A static `CoverJpg` is emitted whenever the clip has non-empty selected art;
/// an animated `CoverWebp` only when `animated_covers` is set and the clip
/// carries a video preview. An empty art URL emits NO `CoverJpg`: reconcile
/// reads a desired that simply lacks a cover as UNKNOWN => KEEP, never a delete,
/// so a transient empty URL cannot strand or remove an existing cover. The
/// `CoverJpg` hash tracks the art URL (`art_hash`); the `CoverWebp` hash tracks
/// the video URL, so a changed source re-transcodes.
fn clip_artifacts(clip: &Clip, base: &str, animated_covers: bool) -> Vec<DesiredArtifact> {
    let mut artifacts = Vec::new();
    if let Some(url) = clip.selected_image_url().filter(|u| !u.is_empty()) {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::CoverJpg,
            path: format!("{base}.jpg"),
            source_url: url.to_owned(),
            hash: art_hash(clip),
        });
    }
    if animated_covers && !clip.video_cover_url.is_empty() {
        artifacts.push(DesiredArtifact {
            kind: ArtifactKind::CoverWebp,
            path: format!("{base}.webp"),
            source_url: clip.video_cover_url.clone(),
            hash: art_url_hash(&clip.video_cover_url),
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
///
/// A source is only authoritative for deletion when its listing fully drained
/// (`complete` — the feed reported no more pages, with no transport error or
/// page-cap truncation) *and* no narrowing filter (`--limit` / `--since`) was
/// applied: a partial or filtered listing omits clips that may still exist
/// upstream, so a missing clip cannot be read as a deletion. The reconcile
/// engine refuses every delete unless all sources report `true`.
pub fn fully_enumerated(complete: bool, narrowed: bool) -> bool {
    complete && !narrowed
}

/// Whether a `--limit` or `--since` filter narrows a listing.
pub fn is_narrowed(limit: Option<usize>, since: Option<&str>) -> bool {
    limit.is_some() || since.is_some()
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

    #[test]
    fn build_desired_appends_extension_and_mode() {
        let a = clip("id-a", "Song A", "alice");
        let clips = [&a];
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            SourceMode::Mirror,
            &no_contexts(),
            &no_collisions(),
            false,
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
            parent_id: "root-1".to_owned(),
            edge_type: None,
            status: suno_core::ResolveStatus::Resolved,
        };
        let contexts: HashMap<String, LineageContext> =
            [(a.id.clone(), lineage.clone())].into_iter().collect();
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            SourceMode::Mirror,
            &contexts,
            &no_collisions(),
            false,
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
            SourceMode::Mirror,
            &contexts_of(&store),
            &store.colliding_root_titles(),
            false,
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
            SourceMode::Mirror,
            &contexts_of(&store),
            &store.colliding_root_titles(),
            false,
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
            SourceMode::Copy,
            &no_contexts(),
            &no_collisions(),
            false,
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
            SourceMode::Mirror,
            &no_contexts(),
            &no_collisions(),
            false,
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
            SourceMode::Mirror,
            &no_contexts(),
            &no_collisions(),
            false,
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
            SourceMode::Mirror,
            &no_contexts(),
            &no_collisions(),
            true,
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
            SourceMode::Mirror,
            &no_contexts(),
            &no_collisions(),
            false,
        );
        assert_eq!(desired[0].artifacts.len(), 1);
        assert_eq!(desired[0].artifacts[0].kind, ArtifactKind::CoverJpg);

        // Enabled with a video: a CoverWebp joins the CoverJpg, pathed .webp,
        // sourced from the video URL and hashed by art_url_hash.
        let desired = build_desired(
            &clips,
            AudioFormat::Flac,
            SourceMode::Mirror,
            &no_contexts(),
            &no_collisions(),
            true,
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
            SourceMode::Mirror,
            &no_contexts(),
            &no_collisions(),
            true,
        );
        assert!(
            desired[0]
                .artifacts
                .iter()
                .all(|art| art.kind != ArtifactKind::CoverWebp)
        );
    }

    #[test]
    fn fully_enumerated_requires_ok_and_unnarrowed() {
        assert!(fully_enumerated(true, false));
        assert!(!fully_enumerated(false, false));
        assert!(!fully_enumerated(true, true));
        assert!(!fully_enumerated(false, true));
    }

    #[test]
    fn truncated_listing_is_never_authoritative_for_deletion() {
        // A `complete == false` listing (transport error or page-cap
        // truncation) must never be treated as fully enumerated, even with no
        // narrowing filter, so reconcile emits no deletes against it.
        assert!(!fully_enumerated(false, false));
    }

    #[test]
    fn is_narrowed_tracks_limit_and_since() {
        assert!(!is_narrowed(None, None));
        assert!(is_narrowed(Some(5), None));
        assert!(is_narrowed(None, Some("7d")));
        assert!(is_narrowed(Some(5), Some("7d")));
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
            SourceMode::Mirror,
            &no_contexts(),
            &no_collisions(),
            false,
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
            SourceMode::Mirror,
            &no_contexts(),
            &no_collisions(),
            false,
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
}
