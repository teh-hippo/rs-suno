//! Rendering: the `ls` table, `lsjson` NDJSON, progress lines, and the per-run
//! and dry-run summaries.
//!
//! Output rules: machine-readable payloads (`ls` rows, `lsjson` objects) go to
//! stdout; progress and summaries go to stderr so a piped `lsjson` stays clean.
//! Everything here is pure string building; the caller does the actual writing.

use std::collections::HashSet;

use serde::Serialize;
use suno_core::{Action, ArtifactKind, Clip, ExecOutcome, Plan, RunStatus};

/// Truncate `text` to `max` characters, appending an ellipsis when shortened.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The tab-separated `ls` header, printed only to a terminal.
pub fn ls_header() -> &'static str {
    "ID\tDURATION\tTITLE\tTAGS"
}

/// One tab-separated `ls` row: id, duration, 48-char title, tags.
pub fn ls_row(clip: &Clip) -> String {
    format!(
        "{}\t{:.1}s\t{}\t{}",
        clip.id,
        clip.duration,
        truncate(title_or_untitled(clip), 48),
        clip.tags
    )
}

/// The stable per-clip NDJSON object for `lsjson` (docs/src/commands-reference.md).
///
/// The field order is the on-disk order; the set is additive. Nullable fields
/// serialise as JSON `null` when the API supplied no value.
#[derive(Debug, Serialize)]
struct ClipJson<'a> {
    id: &'a str,
    title: &'a str,
    status: &'a str,
    duration: f64,
    created_at: &'a str,
    is_liked: bool,
    has_vocal: bool,
    clip_type: &'a str,
    tags: &'a str,
    prompt: Option<&'a str>,
    gpt_description_prompt: Option<&'a str>,
    lyrics: Option<&'a str>,
    model_name: &'a str,
    major_model_version: &'a str,
    display_name: &'a str,
    handle: &'a str,
    album_title: Option<&'a str>,
    root_ancestor_id: Option<&'a str>,
    lineage_status: Option<&'a str>,
    edited_clip_id: Option<&'a str>,
    audio_url: &'a str,
    image_url: &'a str,
    image_large_url: &'a str,
    video_url: &'a str,
    video_cover_url: &'a str,
}

impl<'a> ClipJson<'a> {
    fn from_clip(clip: &'a Clip) -> ClipJson<'a> {
        ClipJson {
            id: &clip.id,
            title: title_or_untitled(clip),
            status: &clip.status,
            duration: clip.duration,
            created_at: &clip.created_at,
            is_liked: clip.is_liked,
            has_vocal: clip.has_vocal,
            clip_type: &clip.clip_type,
            tags: &clip.tags,
            prompt: nullable(&clip.prompt),
            gpt_description_prompt: nullable(&clip.gpt_description_prompt),
            lyrics: nullable(&clip.lyrics),
            model_name: &clip.model_name,
            major_model_version: &clip.major_model_version,
            display_name: &clip.display_name,
            handle: &clip.handle,
            album_title: nullable(&clip.album_title),
            root_ancestor_id: nullable(&clip.root_ancestor_id),
            lineage_status: nullable(&clip.lineage_status),
            edited_clip_id: nullable(&clip.edited_clip_id),
            audio_url: &clip.audio_url,
            image_url: &clip.image_url,
            image_large_url: &clip.image_large_url,
            video_url: &clip.video_url,
            video_cover_url: &clip.video_cover_url,
        }
    }
}

/// Serialise one clip as a single NDJSON line (no trailing newline).
pub fn lsjson_line(clip: &Clip) -> String {
    serde_json::to_string(&ClipJson::from_clip(clip)).expect("clip JSON serialises")
}

/// `"Untitled"` when the clip's title is blank, otherwise the title.
fn title_or_untitled(clip: &Clip) -> &str {
    if clip.title.trim().is_empty() {
        "Untitled"
    } else {
        &clip.title
    }
}

/// Map an empty string to JSON `null`, keeping any non-empty value.
fn nullable(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

/// The single default-level progress line shown before a run applies its plan.
pub fn progress_start(verb: &str, label: &str, plan: &Plan) -> String {
    format!("[{verb}] {label}: applying {} action(s)…", plan.len())
}

/// Per-song lines for `-v` and above, annotating any failed clip.
///
/// The executor reports outcomes as counts plus a failure list, not as a live
/// stream, so these are rendered once from the plan after execution and marked
/// failed when the clip appears in `failed_ids`.
pub fn action_lines(plan: &Plan, failed_ids: &HashSet<&str>, verbosity: i8) -> Vec<String> {
    if verbosity < 1 {
        return Vec::new();
    }
    plan.actions
        .iter()
        .map(|action| action_line(action, failed_ids))
        .collect()
}

fn action_line(action: &Action, failed_ids: &HashSet<&str>) -> String {
    let failed = |id: &str| failed_ids.contains(id);
    let mark = |id: &str, body: String| -> String {
        if failed(id) {
            format!("  {body}  [FAILED]")
        } else {
            format!("  {body}")
        }
    };
    match action {
        Action::Download { clip, path, .. } => mark(
            &clip.id,
            format!(
                "download  {}  {}  {path}",
                short_id(&clip.id),
                truncate(title_or_untitled(clip), 40)
            ),
        ),
        Action::Reformat { clip, path, to, .. } => mark(
            &clip.id,
            format!(
                "reformat  {}  {}  -> {to} {path}",
                short_id(&clip.id),
                truncate(title_or_untitled(clip), 40)
            ),
        ),
        Action::Retag { clip, .. } => mark(
            &clip.id,
            format!(
                "tag       {}  {}  tags updated",
                short_id(&clip.id),
                truncate(title_or_untitled(clip), 40)
            ),
        ),
        Action::Rename { from, to } => format!("  rename    {from} -> {to}"),
        Action::Delete { path, clip_id } => mark(
            clip_id,
            format!(
                "delete    {}  {path}  removed (absent from source)",
                short_id(clip_id)
            ),
        ),
        Action::Skip { clip_id } => {
            format!("  skip      {}  already up to date", short_id(clip_id))
        }
        Action::WriteArtifact {
            kind,
            path,
            owner_id,
            ..
        } => mark(
            owner_id,
            format!(
                "artifact  {}  {}  -> {path}",
                short_id(owner_id),
                artifact_label(*kind)
            ),
        ),
        Action::DeleteArtifact {
            kind,
            path,
            owner_id,
        } => mark(
            owner_id,
            format!(
                "artifact  {}  {}  removed {path}",
                short_id(owner_id),
                artifact_label(*kind)
            ),
        ),
    }
}

/// A short, stable label for an artifact kind, for progress and dry-run lines.
fn artifact_label(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::CoverJpg => "cover.jpg",
        ArtifactKind::CoverWebp => "cover.webp",
        ArtifactKind::FolderJpg => "folder.jpg",
        ArtifactKind::FolderWebp => "folder.webp",
        ArtifactKind::Playlist => "playlist",
    }
}

/// The first eight characters of a clip id, for compact progress lines.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// The per-run summary printed after a `sync` or `copy` finishes.
///
/// A clean or per-clip-failed run reads "{verb} complete"; a run the engine
/// aborted (a full disk or a bad token) reads "{verb} aborted" and names why,
/// so the counters are never mistaken for a finished mirror.
pub fn run_summary(verb_label: &str, account: &str, outcome: &ExecOutcome, secs: f64) -> String {
    let downloaded = outcome.downloaded + outcome.reformatted;
    let tagged = outcome.retagged;
    let renamed = outcome.renamed;
    let deleted = outcome.deleted;
    let skipped = outcome.skipped;
    let failed = outcome.failed();
    let total = downloaded + tagged + renamed + deleted + skipped + failed;
    let header = match outcome.status {
        RunStatus::Completed => format!("{verb_label} complete: {account}"),
        RunStatus::DiskFull => {
            format!("{verb_label} aborted: {account} (disk full, run stopped early)")
        }
        RunStatus::AuthAborted => {
            format!("{verb_label} aborted: {account} (authentication failed, run stopped early)")
        }
    };
    format!(
        "{header}\n  downloaded  {downloaded:>4}\n  tagged      {tagged:>4}\n  renamed     {renamed:>4}\n  deleted     {deleted:>4}\n  skipped     {skipped:>4}\n  failed      {failed:>4}\n  total       {total:>4}\nDuration: {secs:.1}s"
    )
}

/// The dry-run / check summary derived from the plan, making no changes.
pub fn dry_summary(account: &str, plan: &Plan) -> String {
    let to_download = plan.downloads() + plan.reformats();
    let to_tag = plan.retags();
    let to_rename = plan.renames();
    let to_delete = plan.deletes();
    let up_to_date = plan.skips();
    let total = to_download + to_tag + to_rename + to_delete + up_to_date;
    format!(
        "Dry run: {account} (no changes made)\n  to download {to_download:>4}\n  to tag      {to_tag:>4}\n  to rename   {to_rename:>4}\n  to delete   {to_delete:>4}\n  up to date  {up_to_date:>4}\n  total       {total:>4}"
    )
}

/// The interactive destructive-sync prompt body (without the trailing `[y/N]`).
///
/// Lists up to `show` deletion paths, then a count of any remainder.
pub fn delete_prompt(paths: &[String], show: usize) -> String {
    let mut out = format!(
        "suno sync will delete {} local file(s) that are no longer in the source:\n",
        paths.len()
    );
    for path in paths.iter().take(show) {
        out.push_str(&format!("  {path}\n"));
    }
    if paths.len() > show {
        out.push_str(&format!(
            "  ... and {} more (run with -v to see the full list)\n",
            paths.len() - show
        ));
    }
    out.push_str("\nProceed?");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use suno_core::{AudioFormat, LineageContext};

    fn rich_clip() -> Clip {
        Clip {
            id: "3f2a1b4c".to_owned(),
            title: "Electric Storm".to_owned(),
            status: "complete".to_owned(),
            duration: 182.4,
            created_at: "2024-03-10T14:22:01Z".to_owned(),
            is_liked: true,
            has_vocal: false,
            clip_type: "gen".to_owned(),
            tags: "ambient, cinematic".to_owned(),
            prompt: "an orchestral storm".to_owned(),
            gpt_description_prompt: String::new(),
            lyrics: String::new(),
            model_name: "chirp-v4".to_owned(),
            major_model_version: "v4".to_owned(),
            display_name: "alice".to_owned(),
            handle: "alice".to_owned(),
            album_title: "Weather".to_owned(),
            root_ancestor_id: String::new(),
            lineage_status: "root".to_owned(),
            edited_clip_id: String::new(),
            audio_url: "https://cdn1.suno.ai/3f2a1b4c.mp3".to_owned(),
            image_url: "https://cdn1.suno.ai/i.jpeg".to_owned(),
            image_large_url: "https://cdn1.suno.ai/il.jpeg".to_owned(),
            video_url: String::new(),
            video_cover_url: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn lsjson_line_has_stable_schema_and_nulls() {
        let line = lsjson_line(&rich_clip());
        assert!(!line.contains('\n'));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["id"], "3f2a1b4c");
        assert_eq!(value["title"], "Electric Storm");
        assert_eq!(value["duration"], 182.4);
        assert_eq!(value["is_liked"], true);
        // Empty strings become null.
        assert!(value["gpt_description_prompt"].is_null());
        assert!(value["lyrics"].is_null());
        assert!(value["root_ancestor_id"].is_null());
        assert!(value["edited_clip_id"].is_null());
        // Present strings stay present.
        assert_eq!(value["prompt"], "an orchestral storm");
        assert_eq!(value["album_title"], "Weather");
        // Always-present string fields stay strings even when empty.
        assert_eq!(value["video_url"], "");
    }

    #[test]
    fn lsjson_blank_title_becomes_untitled() {
        let mut clip = rich_clip();
        clip.title = "   ".to_owned();
        let value: Value = serde_json::from_str(&lsjson_line(&clip)).unwrap();
        assert_eq!(value["title"], "Untitled");
    }

    #[test]
    fn lsjson_field_count_is_stable() {
        let value: Value = serde_json::from_str(&lsjson_line(&rich_clip())).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 25);
    }

    #[test]
    fn ls_row_truncates_title_and_formats_duration() {
        let mut clip = rich_clip();
        clip.title = "x".repeat(60);
        let row = ls_row(&clip);
        let cols: Vec<&str> = row.split('\t').collect();
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[1], "182.4s");
        assert_eq!(cols[2].chars().count(), 48);
        assert!(cols[2].ends_with('…'));
    }

    #[test]
    fn run_summary_total_is_the_sum() {
        let outcome = ExecOutcome {
            downloaded: 12,
            retagged: 3,
            renamed: 1,
            deleted: 2,
            skipped: 129,
            ..Default::default()
        };
        let text = run_summary("Sync", "alice", &outcome, 43.2);
        assert!(text.contains("Sync complete: alice"));
        assert!(text.contains("downloaded    12"));
        assert!(text.contains("total        147"));
        assert!(text.contains("Duration: 43.2s"));
    }

    #[test]
    fn run_summary_disk_full_reads_aborted_not_complete() {
        let outcome = ExecOutcome {
            downloaded: 4,
            failures: vec![suno_core::Failure {
                clip_id: "z".to_owned(),
                reason: "disk full: no space left to write z.flac".to_owned(),
            }],
            status: RunStatus::DiskFull,
            ..Default::default()
        };
        let text = run_summary("Sync", "alice", &outcome, 1.0);
        assert!(!text.contains("complete"));
        assert!(text.contains("aborted"));
        assert!(text.contains("disk full"));
        // The counters still render.
        assert!(text.contains("downloaded     4"));
    }

    #[test]
    fn dry_summary_reads_plan_counts() {
        let plan = Plan {
            actions: vec![
                Action::Download {
                    clip: rich_clip(),
                    path: "a.flac".to_owned(),
                    format: AudioFormat::Flac,
                    lineage: LineageContext::own_root(&rich_clip()),
                },
                Action::Skip {
                    clip_id: "b".to_owned(),
                },
                Action::Delete {
                    path: "c.flac".to_owned(),
                    clip_id: "c".to_owned(),
                },
            ],
        };
        let text = dry_summary("alice", &plan);
        assert!(text.contains("Dry run: alice (no changes made)"));
        assert!(text.contains("to download    1"));
        assert!(text.contains("to delete      1"));
        assert!(text.contains("up to date     1"));
        assert!(text.contains("total          3"));
    }

    #[test]
    fn action_lines_empty_below_verbose() {
        let plan = Plan {
            actions: vec![Action::Skip {
                clip_id: "abcdef".to_owned(),
            }],
        };
        assert!(action_lines(&plan, &HashSet::new(), 0).is_empty());
        assert_eq!(action_lines(&plan, &HashSet::new(), 1).len(), 1);
    }

    #[test]
    fn action_lines_mark_failures() {
        let plan = Plan {
            actions: vec![Action::Download {
                clip: rich_clip(),
                path: "a.flac".to_owned(),
                format: AudioFormat::Flac,
                lineage: LineageContext::own_root(&rich_clip()),
            }],
        };
        let mut failed = HashSet::new();
        failed.insert("3f2a1b4c");
        let lines = action_lines(&plan, &failed, 1);
        assert!(lines[0].contains("[FAILED]"));
    }

    #[test]
    fn delete_prompt_lists_and_summarises_remainder() {
        let paths: Vec<String> = (0..5).map(|i| format!("song-{i}.flac")).collect();
        let prompt = delete_prompt(&paths, 2);
        assert!(prompt.contains("will delete 5 local file(s)"));
        assert!(prompt.contains("song-0.flac"));
        assert!(prompt.contains("and 3 more"));
        assert!(prompt.trim_end().ends_with("Proceed?"));
    }
}
