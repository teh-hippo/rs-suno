//! The single downloadable-clip filter shared by feed listing and playlist scoping.

use crate::model::Clip;

/// Metadata `task` values that never yield a standalone downloadable track.
pub(crate) const EXCLUDED_TASKS: [&str; 2] = ["infill", "fixed_infill"];
/// Metadata `type` values that are rendering artefacts, not real tracks.
pub(crate) const EXCLUDED_TYPES: [&str; 1] = ["rendered_context_window"];

/// Whether a clip is a finished track worth downloading.
///
/// True only for a `complete` clip that is neither an infill task nor a
/// context-window artefact. This is the single source of truth for both the
/// account feed and scoped playlist members, so streaming, infill, and
/// artefact clips are excluded consistently wherever clips enter the pipeline.
///
/// It deliberately does not test `is_trashed`: a trashed clip is the delete
/// signal for a full run, so screening it out here would stop those clips from
/// ever reaching the reconciler as removals.
pub fn is_downloadable(clip: &Clip) -> bool {
    clip.status == "complete"
        && !EXCLUDED_TYPES.contains(&clip.clip_type.as_str())
        && !EXCLUDED_TASKS.contains(&clip.task.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(status: &str, clip_type: &str, task: &str) -> Clip {
        Clip {
            id: "x".to_owned(),
            status: status.to_owned(),
            clip_type: clip_type.to_owned(),
            task: task.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn complete_track_is_downloadable() {
        assert!(is_downloadable(&clip("complete", "gen", "")));
    }

    #[test]
    fn streaming_clip_is_not_downloadable() {
        assert!(!is_downloadable(&clip("streaming", "", "")));
    }

    #[test]
    fn infill_tasks_are_not_downloadable() {
        assert!(!is_downloadable(&clip("complete", "gen", "infill")));
        assert!(!is_downloadable(&clip("complete", "gen", "fixed_infill")));
    }

    #[test]
    fn context_window_artefact_is_not_downloadable() {
        assert!(!is_downloadable(&clip(
            "complete",
            "rendered_context_window",
            ""
        )));
    }

    #[test]
    fn trashed_but_complete_clip_is_still_downloadable() {
        // Trashed is the deletion signal for a full run, never a downloadability
        // screen, so a trashed complete track still passes this filter.
        let mut trashed = clip("complete", "gen", "");
        trashed.is_trashed = true;
        assert!(is_downloadable(&trashed));
    }
}
