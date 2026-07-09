//! The write-lifecycle skeleton shared by the per-clip sidecar
//! ([`artifact`](super::artifact)) and stem ([`stem`](super::stem)) commit and
//! move paths.
//!
//! Sidecars and stems ran near-mirrored `commit_*` / `move_*` functions: each
//! encoded the same superseded-old-path cleanup and the same in-place-rename
//! downgrade twice, so the two paths could silently drift (#369). The genuinely
//! shared, deletion-adjacent parts — the cross-slot co-reference guard (#76),
//! the committed-this-run guard (#142), and the exclusive-rename decrement —
//! live here once. The legitimately divergent parts (the owner-absent guard,
//! the slot read/write routing, the refetch source, and the commit fallback)
//! stay in each caller.

use super::*;

/// The outcome of an attempted in-place relocation (the #141 rename downgrade),
/// so [`move_artifact`](Ctx::move_artifact) and [`move_stem`](Ctx::move_stem)
/// share the exclusive-rename skeleton while keeping their own slot update,
/// disk message, refetch, and commit fallback.
pub(super) enum Relocate {
    /// The file was renamed `from` -> `to`; the caller records the new slot path
    /// (the vacated reference has already been decremented here).
    Renamed,
    /// The rename hit a full disk; the caller aborts the run (a disk-full,
    /// exit-9 [`Fail`]).
    DiskFull,
    /// No in-place move happened — `from` is co-referenced or committed, the
    /// path is unchanged, or the old file vanished/rename is unsupported — so
    /// the caller fetches fresh bytes and writes at `to`.
    Refetch,
}

impl<H, F, G, C> Ctx<'_, H, F, G, C>
where
    H: Http,
    F: Filesystem,
    G: Ffmpeg,
    C: Clock,
{
    /// Remove the file a just-written slot superseded, honouring the two
    /// deletion-safety guards shared by [`commit_artifact`](Ctx::commit_artifact)
    /// and [`commit_stem`](Ctx::commit_stem).
    ///
    /// When `old` is present, non-empty, and differs from the freshly written
    /// `new`, its tracked-path reference is decremented; the file is removed only
    /// when no other tracked slot still references it (a prior failed swap can
    /// leave two clips sharing one path, #76) AND no committed write this run has
    /// already placed a file there (#142). The short-circuit order and the
    /// `saturating_sub` decrement are byte-identical to the former inline blocks.
    ///
    /// A removal failure is classified through the shared [`disk_or_permanent`]
    /// verdict: a full disk aborts the run (exit 9), anything else is a per-clip
    /// skip. The real [`Filesystem::remove`] adapter never reports out-of-space
    /// today, so the disk arm is currently unreachable and this is byte-identical
    /// to the prior per-path classification; routing it through the shared helper
    /// keeps the two paths from drifting and is forward-safe (#406). `label`
    /// names the slot ("sidecar" / "stem") so the messages match verbatim.
    pub(super) fn remove_superseded(
        &self,
        id: &str,
        old: Option<&str>,
        new: &str,
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
        label: &str,
    ) -> Result<(), Fail> {
        if let Some(old) = old
            && !old.is_empty()
            && old != new
        {
            let still_referenced = tracked_paths
                .get_mut(old)
                .map(|count| {
                    *count = count.saturating_sub(1);
                    *count > 0
                })
                .unwrap_or(false);
            if !still_referenced && !committed.contains(old) {
                self.fs.remove(old).map_err(|err| {
                    disk_or_permanent(
                        id,
                        err.is_out_of_space(),
                        format!("disk full: no space left to remove old {label} {old}"),
                        format!("could not remove old {label} {old}: {err}"),
                    )
                })?;
            }
        }
        Ok(())
    }

    /// Attempt the #141 in-place rename shared by
    /// [`move_artifact`](Ctx::move_artifact) and [`move_stem`](Ctx::move_stem).
    ///
    /// The rename is taken only when `from` is this slot's alone to give up: no
    /// other tracked slot still references it (a prior failed swap can share a
    /// path, #76) and no committed write this run has already placed a file there
    /// (#142). On success the vacated reference is decremented (as the former
    /// inline blocks did, before the caller records the new slot); the caller
    /// then updates its own slot. A full-disk rename is systemic
    /// ([`Relocate::DiskFull`]); a vanished old file or an unsupported rename
    /// falls through to [`Relocate::Refetch`], where the caller fetches fresh
    /// bytes and writes at `to` (running the gated old-path cleanup), so a swap
    /// or co-reference is handled exactly as before.
    pub(super) fn try_relocate(
        &self,
        from: &str,
        to: &str,
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
    ) -> Relocate {
        let exclusive =
            tracked_paths.get(from).is_none_or(|count| *count <= 1) && !committed.contains(from);
        if from != to && exclusive {
            match self.fs.rename(from, to) {
                Ok(()) => {
                    if let Some(count) = tracked_paths.get_mut(from) {
                        *count = count.saturating_sub(1);
                    }
                    return Relocate::Renamed;
                }
                Err(err) if err.is_out_of_space() => return Relocate::DiskFull,
                // The old file has vanished, or the rename is unsupported: fall
                // through to a fetch-and-write at `to`.
                Err(_) => {}
            }
        }
        Relocate::Refetch
    }
}
