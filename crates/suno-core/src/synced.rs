//! Pure synced-lyrics resolution: which clips to fetch alignment for, and how
//! each fetched result maps onto a clip's desired `.lrc` and deferred
//! `.lyrics.txt` artifacts.
//!
//! The alignment fetch itself is IO and lives in the CLI (through the `Http`
//! port); everything here is pure so the fetch-gating, the timed/untimed body
//! choice, the "keep existing on failure" rule, and the instrumental "checked"
//! marker are unit-tested without a network.
//!
//! Suno's forced alignment for a clip is immutable in practice (the audio and
//! its lyrics are fixed once generated), so a clip is fetched at most once per
//! render [`SYNCED_LRC_VERSION`] — recorded by [`SyncedLyricsCheck`] on the
//! manifest — except that a clip that resolved to no lyrics (an instrumental) or
//! to an untimed plain-text fallback is re-checked after [`SYNCED_LRC_RECHECK_SECS`]
//! to pick up alignment Suno may compute after generation, and a clip whose audio
//! is renamed is re-fetched so its sidecar moves with it. A version bump
//! re-resolves everything.
//!
//! The `.lyrics.txt` sidecar (F1, #357) is resolved here too: its body is
//! `clip.lyrics` when the feed carries them, else the aligned plain text, so a
//! real-feed clip (whose `clip.lyrics` is empty) still gets a populated file.
//! Every desired lyric slot is an independent fetch trigger: a clip is fetched
//! when its `.lrc` OR its `.lyrics.txt` is unresolved, so enabling
//! `lyrics_sidecar` on a library whose `.lrc`s have already converged still
//! back-fills the `.lyrics.txt` (and the reverse), and a lyrics-only clip
//! (`lyrics_sidecar` on, `lrc_sidecar` off) is a target in its own right. The
//! single per-clip marker is stamped only once every slot the fetch wrote has
//! landed, so a partial write is retried; once every desired slot is resolved
//! the clip converges (fetched once, then skipped) instead of re-fetching
//! forever.
//!
//! Known limitation (tracked follow-up, not fixed here): #354's embedded-lyrics
//! back-fill is keyed on the `.lrc` artifact hash (`embedded_lyrics_hash`), so a
//! lyrics-only clip gets an aligned `.lyrics.txt` on disk but its MP3
//! `LYRICS`/`USLT` tag is NOT back-filled (the embed sentinel only runs when a
//! `.lrc` is desired). This does not regress today's behaviour (the tag was
//! empty for these clips before too); decoupling the embed from the `.lrc` is
//! #354's seam, tracked separately.

use std::collections::{BTreeSet, HashMap};

use crate::hash::{
    SYNCED_LRC_VERSION, content_hash, lyrics_txt_source_hash, synced_lrc_source_hash,
};
use crate::lyrics::{AlignedLyrics, render_clip_lrc, render_clip_lyrics, render_synced_lrc};
use crate::manifest::{Manifest, ManifestEntry};
use crate::model::Clip;
use crate::reconcile::Desired;
use crate::vocab::ArtifactKind;

/// How long a clip that resolved to no lyrics (instrumental) or to an untimed
/// plain-text fallback is trusted before its alignment is re-checked (14 days).
/// Bounds the re-fetch to catch alignment Suno may compute after generation.
pub const SYNCED_LRC_RECHECK_SECS: u64 = 14 * 24 * 60 * 60;

/// One clip's synced-lyrics outcome this run, for the caller to record as a
/// manifest [`SyncedLyricsCheck`](crate::SyncedLyricsCheck) once the sidecar
/// write (if any) has safely landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCheck {
    /// The clip this outcome concerns.
    pub clip_id: String,
    /// Whether the clip resolved to no lyrics (an instrumental). An instrumental
    /// writes no sidecar and is marked unconditionally.
    pub empty: bool,
    /// Whether the fetched alignment was timed (as opposed to an untimed
    /// plain-text fallback). Only meaningful when `empty` is false; a timed clip
    /// is exempt from the periodic re-check.
    pub timed: bool,
    /// Every lyric sidecar slot this clip's fetch produced a body for, paired
    /// with the content hash the manifest slot must reflect. The caller stamps
    /// the durable marker only once EVERY listed slot has landed, so a partial
    /// write (one slot ok, another failed non-fatally) leaves no marker and is
    /// retried next run. Empty for an instrumental (nothing written).
    /// Deterministically ordered ([`Lrc`](ArtifactKind::Lrc) before
    /// [`LyricsTxt`](ArtifactKind::LyricsTxt)).
    pub written_slots: Vec<(ArtifactKind, String)>,
}

/// The outcome of resolving one clip's desired lyric sidecar slot against a
/// fetched alignment. Feeds [`build_pending_check`], which folds a clip's per-slot
/// outcomes into the single durable marker.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotOutcome {
    /// The slot was not desired, or the clip was not fetched this run (its
    /// stored body, if any, was kept). Contributes nothing to the marker.
    Inert,
    /// The clip was fetched and resolved to no lyrics for this slot (an
    /// instrumental): the artifact was dropped and nothing written.
    Instrumental,
    /// The clip was fetched and a body was rendered for this slot, carrying the
    /// content hash the manifest slot must come to reflect.
    Wrote(String),
}

/// The `.lyrics.txt` body for a clip: its own `clip.lyrics` when the feed
/// carries them (preferred, for back-compat), else Suno's fetched aligned plain
/// text. `None` for an instrumental (both empty), so no empty sidecar is written.
///
/// Normalised to exactly one trailing newline to match the sidecar convention:
/// [`render_clip_lyrics`] already appends one, but
/// [`AlignedLyrics::plain_text`](crate::AlignedLyrics::plain_text) joins lines
/// with no trailing newline, so this appends it.
fn plain_lyrics(clip: &Clip, aligned: &AlignedLyrics) -> Option<String> {
    if let Some(text) = render_clip_lyrics(clip) {
        return Some(text);
    }
    let plain = aligned.plain_text();
    let plain = plain.trim_end();
    if plain.is_empty() {
        return None;
    }
    Some(format!("{plain}\n"))
}

/// Whether a clip's alignment must be (re)fetched this run to resolve one desired
/// lyric sidecar slot.
///
/// `desired_path`/`desired_kind` name the specific sidecar under test. Each slot
/// is an INDEPENDENT trigger: a converged `.lrc` does not exempt a still-missing
/// `.lyrics.txt` (and the reverse), so enabling a second sidecar back-fills it.
/// The rename-drift check anchors on the matching manifest slot so a resolved
/// clip converges instead of re-fetching every run.
fn needs_fetch(
    entry: Option<&ManifestEntry>,
    desired_path: &str,
    desired_kind: ArtifactKind,
    now_unix: u64,
) -> bool {
    let Some(entry) = entry else {
        return true; // never downloaded -> resolve on first sight
    };
    // One-time back-fill (#354): a persisted `.lrc` exists but the audio tag's
    // embed is missing or stale relative to it. Fetching re-embeds; once the
    // embed stamps `embedded_lyrics_hash = lrc.hash` this is false again, so the
    // back-fill is bounded to one run per drift. This clause is `.lrc`-keyed by
    // design (see the module note on the lyrics-only embed limitation): a clip
    // with no `.lrc` slot has both sides empty and never triggers it.
    if let Some(slot) = entry.lrc.as_ref()
        && slot.hash != entry.embedded_lyrics_hash
    {
        return true;
    }
    let Some(check) = entry.synced_lyrics.as_ref() else {
        return true; // never resolved (e.g. downloaded before the feature)
    };
    if check.version != SYNCED_LRC_VERSION {
        return true; // the render changed -> re-resolve and re-render
    }
    if check.empty {
        // Instrumental: writing no sidecar IS the converged state, so an absent
        // slot here is not a "missing desired slot" to back-fill. Re-check only
        // once the window elapses, to pick up alignment Suno adds later. This
        // clause MUST precede the slot-presence check below.
        return now_unix.saturating_sub(check.checked_unix) > SYNCED_LRC_RECHECK_SECS;
    }
    // The clip has lyrics: the SPECIFIC desired slot drives the decision, so each
    // sidecar is resolved on its own timeline. Match the kind explicitly so a
    // future artifact kind fails loud rather than silently reusing `.lyrics.txt`.
    let slot = match desired_kind {
        ArtifactKind::Lrc => entry.lrc.as_ref(),
        ArtifactKind::LyricsTxt => entry.lyrics_txt.as_ref(),
        _ => None,
    };
    match slot {
        // The desired sidecar was never written: a back-fill (its feature was
        // just enabled) or an interrupted prior write. This is the fix for a
        // clip whose OTHER slot had already converged.
        None => true,
        // Present but the audio was renamed: move the sidecar with it.
        Some(s) if s.path != desired_path => true,
        // Untimed fallback: re-check once the window elapses, to pick up
        // alignment Suno may compute after generation.
        Some(_) if !check.timed => {
            now_unix.saturating_sub(check.checked_unix) > SYNCED_LRC_RECHECK_SECS
        }
        // Timed and in place: converged, no re-fetch.
        Some(_) => false,
    }
}

/// The clip ids whose alignment must be fetched this run, in a stable order.
///
/// Empty when `enabled` is false, so the synced-lyrics feature being off means
/// zero alignment fetches. Only clips carrying a desired lyric artifact (a `.lrc`
/// or a deferred `.lyrics.txt`) are considered; such a clip is a target when ANY
/// of its desired lyric slots still needs a fetch (see [`needs_fetch`]), so
/// enabling one sidecar on a library whose other sidecar has long converged still
/// back-fills the new one. Each slot is fetched at most once per render version.
pub fn synced_lyrics_targets(
    desired: &[Desired],
    manifest: &Manifest,
    now_unix: u64,
    enabled: bool,
) -> BTreeSet<String> {
    if !enabled {
        return BTreeSet::new();
    }
    let mut out = BTreeSet::new();
    for d in desired {
        // Only a clip that desires at least one lyric sidecar is ever fetched.
        let mut lyric_slots = d
            .artifacts
            .iter()
            .filter(|a| matches!(a.kind, ArtifactKind::Lrc | ArtifactKind::LyricsTxt))
            .peekable();
        if lyric_slots.peek().is_none() {
            continue;
        }
        let entry = manifest.get(&d.clip.id);
        // Reformat re-embed (#354): a format change re-encodes the audio and
        // drops any embedded lyrics, so re-fetch when the format will change AND
        // a persisted `.lrc` is worth re-embedding (an already-migrated clip is
        // neither a back-fill nor a rename target, so nothing else would refetch
        // it). Pure and bounded: after the Reformat commits `entry.format ==
        // d.format`, so it fires once. A clip with no `.lrc` never fetches here.
        let reformat_reembed = entry.is_some_and(|e| e.format != d.format && e.lrc.is_some());
        // Fetch when ANY desired lyric slot is unresolved: the per-slot decision
        // makes each sidecar an independent trigger.
        let any_slot_needs =
            lyric_slots.any(|a| needs_fetch(entry, a.path.as_str(), a.kind, now_unix));
        if reformat_reembed || any_slot_needs {
            out.insert(d.clip.id.clone());
        }
    }
    out
}

/// Resolve each clip's desired `.lrc` and deferred `.lyrics.txt` artifacts from
/// the fetched alignment, returning the checks to persist for the clips that
/// were successfully fetched.
///
/// `successes` holds the alignment for clips whose fetch returned `200` (an empty
/// value for an instrumental); a clip absent from it either was not fetched
/// (resolved recently) or its fetch FAILED. In both of those cases the existing
/// sidecar is KEPT untouched — the artifact's hash is reset to the stored slot so
/// reconcile skips it (no rewrite, no downgrade of a timed file to untimed), or
/// the artifact is dropped when there is nothing on disk yet — and no check is
/// returned, so a failed fetch is simply retried next run.
///
/// For a successful fetch the `.lrc` body is the timed render when Suno has
/// alignment, else the untimed lyrics as a fallback; the `.lyrics.txt` body is
/// `clip.lyrics` when the feed carries them, else the aligned plain text. An
/// instrumental (no body for a slot) drops that artifact. A produced body sets
/// the artifact's content and its content hash, so reconcile rewrites only when
/// the body actually changes (including an untimed->timed upgrade after a
/// re-check). At most one check is returned per clip: its `timed` flag
/// distinguishes timed alignment from an untimed fallback (only timed clips are
/// exempt from the periodic re-check), and its `written_slots` list names every
/// slot a body was rendered for, so the caller marks the clip resolved only once
/// EVERY such slot has landed (a partial write is retried). An instrumental
/// records an empty `written_slots` and is unconditionally durable.
pub fn apply_synced_lrc(
    desired: &mut [Desired],
    manifest: &Manifest,
    successes: &HashMap<String, AlignedLyrics>,
) -> Vec<PendingCheck> {
    let mut pending = Vec::new();
    for d in desired.iter_mut() {
        // Carry-forward baseline (#354, the loop-freedom crux): every clip keeps
        // its persisted embed fingerprint unless it is actually fetched this run
        // (the `.lrc` override below). So a lyrics-driven `Retag` can only fire
        // when alignment was fetched, never stamping a matching hash over an
        // empty write. This assignment MUST stay ABOVE the per-slot resolution:
        // a clip with no desired `.lrc` (the sidecar off, an instrumental, or a
        // lyrics-only clip) must still carry its value forward, otherwise its
        // sentinel would drift to the default and it would spuriously retag with
        // an empty `synced` map.
        d.embedded_lyrics_hash = manifest
            .get(&d.clip.id)
            .map(|e| e.embedded_lyrics_hash.clone())
            .unwrap_or_default();

        let aligned = successes.get(&d.clip.id);
        // Resolve BOTH lyric sidecars from the same fetched alignment, each on
        // its own slot. The `.lrc` also drives the #354 embed (its resolution
        // stamps `embedded_lyrics_hash`); the `.lyrics.txt` carries no embed. The
        // two outcomes fold into a single durable marker that lists every slot
        // written, so back-filling one sidecar never masks an unwritten other.
        let lrc = apply_lrc_slot(d, manifest, aligned);
        let lyrics_txt = apply_lyrics_txt_slot(d, manifest, aligned);
        if let Some(check) = build_pending_check(&d.clip.id, aligned, &lrc, &lyrics_txt) {
            pending.push(check);
        }
    }
    pending
}

/// Fold a clip's per-slot resolution outcomes into the single durable marker to
/// persist, or `None` when the clip was not fetched this run (a resolved-but-
/// untouched clip records nothing, so an existing marker is left intact) or
/// desired no lyric slot at all.
///
/// `written_slots` lists every slot that rendered a body ([`Lrc`](ArtifactKind::Lrc)
/// before [`LyricsTxt`](ArtifactKind::LyricsTxt)); the caller stamps the marker
/// only once every listed slot has landed, so a partial write (one slot ok,
/// another failed non-fatally) records nothing and is retried. `empty` is set for
/// an instrumental (fetched, desired a sidecar, rendered no body for any slot).
/// `timed` mirrors the fetched alignment, gating the periodic re-check.
fn build_pending_check(
    clip_id: &str,
    aligned: Option<&AlignedLyrics>,
    lrc: &SlotOutcome,
    lyrics_txt: &SlotOutcome,
) -> Option<PendingCheck> {
    // Only a fetched clip records a marker; a miss keeps its stored state.
    let aligned = aligned?;
    // A clip that desired no lyric slot (both Inert) is not a lyric outcome.
    let desired_lyric =
        !matches!(lrc, SlotOutcome::Inert) || !matches!(lyrics_txt, SlotOutcome::Inert);
    if !desired_lyric {
        return None;
    }
    let mut written_slots = Vec::new();
    if let SlotOutcome::Wrote(hash) = lrc {
        written_slots.push((ArtifactKind::Lrc, hash.clone()));
    }
    if let SlotOutcome::Wrote(hash) = lyrics_txt {
        written_slots.push((ArtifactKind::LyricsTxt, hash.clone()));
    }
    Some(PendingCheck {
        clip_id: clip_id.to_string(),
        empty: written_slots.is_empty(),
        timed: !aligned.is_empty(),
        written_slots,
    })
}

/// Resolve a clip's desired `.lrc` artifact from the fetched alignment (or keep
/// the stored slot on a miss). Returns the slot's [`SlotOutcome`]: `Inert` when
/// the clip has no `.lrc` desired or was not fetched, `Wrote(hash)` when a body
/// was rendered, `Instrumental` when the fetch resolved to no lyrics. Also stamps
/// the #354 embed fingerprint from the same body.
fn apply_lrc_slot(
    d: &mut Desired,
    manifest: &Manifest,
    aligned: Option<&AlignedLyrics>,
) -> SlotOutcome {
    let Some(idx) = d.artifacts.iter().position(|a| a.kind == ArtifactKind::Lrc) else {
        return SlotOutcome::Inert;
    };
    let clip_id = d.clip.id.clone();
    let slot_hash = manifest
        .get(&clip_id)
        .and_then(|e| e.lrc.as_ref())
        .map(|slot| slot.hash.clone());
    let Some(aligned) = aligned else {
        // Not fetched this run (resolved recently) or the fetch failed: keep
        // whatever is already on disk. Reuse the stored slot hash so reconcile
        // skips the write; drop the artifact when nothing was ever written.
        // `embedded_lyrics_hash` keeps its carry-forward baseline, so a failed
        // back-fill neither retags nor stamps and is simply retried next run.
        match slot_hash {
            Some(hash) => {
                let artifact = &mut d.artifacts[idx];
                artifact.hash = hash;
                artifact.content = None;
            }
            None => {
                d.artifacts.remove(idx);
            }
        }
        return SlotOutcome::Inert;
    };
    let timed = !aligned.is_empty();
    let body = if timed {
        render_synced_lrc(&d.clip, &d.lineage, aligned)
    } else {
        render_clip_lrc(&d.clip, &d.lineage)
    };
    match body {
        Some(text) => {
            let hash = content_hash(&text);
            // The embed is rendered from this same fetched alignment, so its
            // fingerprint moves in lock-step with the `.lrc` body.
            d.embedded_lyrics_hash = hash.clone();
            let artifact = &mut d.artifacts[idx];
            artifact.hash = hash.clone();
            artifact.content = Some(text);
            SlotOutcome::Wrote(hash)
        }
        None => {
            // Fetched but the clip is an instrumental: nothing embedded.
            d.embedded_lyrics_hash = String::new();
            d.artifacts.remove(idx);
            SlotOutcome::Instrumental
        }
    }
}

/// Resolve a clip's deferred `.lyrics.txt` artifact from `clip.lyrics` (preferred)
/// else the fetched aligned plain text (or keep the stored slot on a miss).
/// Returns the slot's [`SlotOutcome`]: `Inert` when the clip has no `.lyrics.txt`
/// desired or was not fetched, `Wrote(hash)` when a body was rendered,
/// `Instrumental` when neither source yields lyrics. The `.lyrics.txt` carries no
/// embed, so it never touches `embedded_lyrics_hash` (#354's back-fill stays
/// `.lrc`-keyed; see the module note on the lyrics-only embed limitation).
fn apply_lyrics_txt_slot(
    d: &mut Desired,
    manifest: &Manifest,
    aligned: Option<&AlignedLyrics>,
) -> SlotOutcome {
    let Some(idx) = d
        .artifacts
        .iter()
        .position(|a| a.kind == ArtifactKind::LyricsTxt)
    else {
        return SlotOutcome::Inert;
    };
    let slot_hash = manifest
        .get(&d.clip.id)
        .and_then(|e| e.lyrics_txt.as_ref())
        .map(|slot| slot.hash.clone());
    let Some(aligned) = aligned else {
        match slot_hash {
            Some(hash) => {
                let artifact = &mut d.artifacts[idx];
                artifact.hash = hash;
                artifact.content = None;
            }
            None => {
                d.artifacts.remove(idx);
            }
        }
        return SlotOutcome::Inert;
    };
    match plain_lyrics(&d.clip, aligned) {
        Some(text) => {
            let hash = content_hash(&text);
            let artifact = &mut d.artifacts[idx];
            artifact.hash = hash.clone();
            artifact.content = Some(text);
            SlotOutcome::Wrote(hash)
        }
        None => {
            d.artifacts.remove(idx);
            SlotOutcome::Instrumental
        }
    }
}

/// Adjust each clip's desired `.lrc` and deferred `.lyrics.txt` artifacts for a
/// dry run, without any fetch.
///
/// Each desired lyric slot is previewed on its OWN fetch decision (mirroring
/// [`needs_fetch`]): a slot that WOULD be (re)fetched keeps a distinct pending
/// source hash so the previewed plan reports the write; a slot already resolved
/// reuses its stored slot hash (so it shows as skipped) or is dropped when it is
/// a known instrumental. Per-slot (not clip-level) so a back-fill clip previews
/// its converged `.lrc` as skipped while its missing `.lyrics.txt` shows as a
/// write. The preview is an upper bound on synced sidecar writes (it cannot know
/// which targets will turn out to be instrumentals).
pub fn preview_synced_lrc(
    desired: &mut [Desired],
    manifest: &Manifest,
    now_unix: u64,
    enabled: bool,
) {
    for d in desired.iter_mut() {
        let entry = manifest.get(&d.clip.id);
        // Carry-forward baseline (#354): mirror `apply_synced_lrc`. This MUST run
        // for every clip (including one with no desired `.lrc`: the sidecar off,
        // a lyrics-only clip, or an instrumental) so it keeps its persisted embed
        // fingerprint and previews as skipped, rather than drifting to the default
        // and reporting a spurious retag.
        d.embedded_lyrics_hash = entry
            .map(|e| e.embedded_lyrics_hash.clone())
            .unwrap_or_default();

        // `.lrc` preview: the pending case also previews the expected post-fetch
        // embed (the #354 back-fill), so it stays coupled to the `.lrc` slot.
        if let Some(idx) = d.artifacts.iter().position(|a| a.kind == ArtifactKind::Lrc) {
            let path = d.artifacts[idx].path.clone();
            if enabled && needs_fetch(entry, &path, ArtifactKind::Lrc, now_unix) {
                // A fetch target previews the expected post-fetch embed: the
                // persisted `.lrc` slot hash, so a pending back-fill reports drift
                // (not "up to date"). It converges to the apply value once
                // alignment is fetched.
                d.embedded_lyrics_hash = entry
                    .and_then(|e| e.lrc.as_ref())
                    .map(|s| s.hash.clone())
                    .unwrap_or_default();
                d.artifacts[idx].hash = synced_lrc_source_hash(&d.clip.id);
            } else {
                match entry.and_then(|e| e.lrc.as_ref()) {
                    Some(slot) => d.artifacts[idx].hash = slot.hash.clone(),
                    None => {
                        d.artifacts.remove(idx);
                    }
                }
            }
        }

        // `.lyrics.txt` preview (F1): the same per-slot treatment as the `.lrc`,
        // but no embed (the plain-text sidecar is not embedded in audio).
        if let Some(idx) = d
            .artifacts
            .iter()
            .position(|a| a.kind == ArtifactKind::LyricsTxt)
        {
            let path = d.artifacts[idx].path.clone();
            if enabled && needs_fetch(entry, &path, ArtifactKind::LyricsTxt, now_unix) {
                d.artifacts[idx].hash = lyrics_txt_source_hash(&d.clip.id);
            } else {
                match entry.and_then(|e| e.lyrics_txt.as_ref()) {
                    Some(slot) => d.artifacts[idx].hash = slot.hash.clone(),
                    None => {
                        d.artifacts.remove(idx);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lineage::LineageContext;
    use crate::lyrics::{AlignedLine, AlignedLineWord};
    use crate::manifest::{ArtifactState, SyncedLyricsCheck};
    use crate::model::Clip;
    use crate::reconcile::DesiredArtifact;
    use crate::vocab::AudioFormat;

    fn clip(id: &str, lyrics: &str) -> Clip {
        Clip {
            id: id.to_string(),
            title: "Song".to_string(),
            lyrics: lyrics.to_string(),
            prompt: "a prompt".to_string(),
            ..Default::default()
        }
    }

    fn lrc_artifact(clip_id: &str) -> DesiredArtifact {
        DesiredArtifact {
            kind: ArtifactKind::Lrc,
            path: format!("{clip_id}.lrc"),
            source_url: String::new(),
            hash: synced_lrc_source_hash(clip_id),
            content: None,
        }
    }

    fn desired(id: &str, lyrics: &str) -> Desired {
        let c = clip(id, lyrics);
        Desired {
            lineage: LineageContext::own_root(&c),
            path: format!("{id}.flac"),
            format: AudioFormat::Flac,
            meta_hash: "m".to_string(),
            art_hash: "a".to_string(),
            embedded_lyrics_hash: String::new(),
            modes: vec![crate::vocab::SourceMode::Mirror],
            trashed: false,
            private: false,
            artifacts: vec![lrc_artifact(id)],
            clip: c,
            stems: None,
        }
    }

    /// A deferred `.lyrics.txt` artifact in its placeholder state (no inline
    /// body, the source-hash sentinel), mirroring [`lrc_artifact`].
    fn lyrics_txt_artifact(clip_id: &str) -> DesiredArtifact {
        DesiredArtifact {
            kind: ArtifactKind::LyricsTxt,
            path: format!("{clip_id}.lyrics.txt"),
            source_url: String::new(),
            hash: lyrics_txt_source_hash(clip_id),
            content: None,
        }
    }

    /// A lyrics-only clip (F1): `lyrics_sidecar` on, `lrc_sidecar` off, so the
    /// only desired lyric artifact is the deferred `.lyrics.txt`.
    fn desired_lyrics_only(id: &str, lyrics: &str) -> Desired {
        let mut d = desired(id, lyrics);
        d.artifacts = vec![lyrics_txt_artifact(id)];
        d
    }

    /// A clip with both lyric sidecars desired (`.lrc` and `.lyrics.txt`), for
    /// asserting each slot is resolved and recorded independently.
    fn desired_both(id: &str, lyrics: &str) -> Desired {
        let mut d = desired(id, lyrics);
        d.artifacts = vec![lrc_artifact(id), lyrics_txt_artifact(id)];
        d
    }

    fn one_line_alignment() -> AlignedLyrics {
        AlignedLyrics {
            lines: vec![AlignedLine {
                text: "hi there".to_owned(),
                start_s: 0.5,
                end_s: 1.2,
                section: "Verse 1".to_owned(),
                words: vec![
                    AlignedLineWord {
                        text: "hi".to_owned(),
                        start_s: 0.5,
                        end_s: 0.8,
                    },
                    AlignedLineWord {
                        text: "there".to_owned(),
                        start_s: 0.9,
                        end_s: 1.2,
                    },
                ],
            }],
            ..Default::default()
        }
    }

    fn entry(lrc: Option<ArtifactState>, check: Option<SyncedLyricsCheck>) -> ManifestEntry {
        // Default to a fully-migrated clip: the embed fingerprint matches the
        // `.lrc` slot hash, so an ordinarily-resolved clip is NOT a #354 back-fill
        // target. Back-fill/instrumental tests override `embedded_lyrics_hash`.
        let embedded_lyrics_hash = lrc.as_ref().map(|s| s.hash.clone()).unwrap_or_default();
        ManifestEntry {
            path: "song.flac".to_string(),
            format: AudioFormat::Flac,
            lrc,
            embedded_lyrics_hash,
            synced_lyrics: check,
            ..Default::default()
        }
    }

    #[test]
    fn targets_empty_when_feature_off() {
        let d = vec![desired("a", "")];
        let manifest = Manifest::new();
        assert!(synced_lyrics_targets(&d, &manifest, 0, false).is_empty());
    }

    #[test]
    fn targets_new_clip_but_not_a_recently_resolved_one() {
        let d = vec![desired("new", ""), desired("done", "")];
        let mut manifest = Manifest::new();
        // `done` was timed-resolved at the current version; `new` is unseen.
        manifest.insert(
            "done",
            entry(
                Some(ArtifactState {
                    path: "done.lrc".to_string(),
                    hash: "h".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: true,
                }),
            ),
        );
        let targets = synced_lyrics_targets(&d, &manifest, 2_000, true);
        assert!(targets.contains("new"));
        assert!(!targets.contains("done"));
    }

    #[test]
    fn instrumental_is_rechecked_only_after_the_window() {
        let d = vec![desired("instr", "")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "instr",
            entry(
                None,
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: true,
                    timed: false,
                }),
            ),
        );
        // Within the window: not re-fetched (this is the fix for forever-refetch).
        let soon = 1_000 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, soon, true).is_empty());
        // Past the window: re-checked, to pick up late alignment.
        let later = 1_001 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, later, true).contains("instr"));
    }

    #[test]
    fn untimed_fallback_is_rechecked_after_the_window() {
        // A clip that previously resolved to an untimed fallback (empty alignment
        // but non-empty lyrics) must be re-checked after the window so a later
        // Suno alignment upgrades it to a timed `.lrc`.
        let d = vec![desired("a", "some lyrics")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry(
                Some(ArtifactState {
                    path: "a.lrc".to_string(),
                    hash: "untimed-hash".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: false,
                }),
            ),
        );
        // Within the window: no re-fetch (avoids churn on every run).
        let soon = 1_000 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, soon, true).is_empty());
        // Past the window: re-checked, to upgrade to timed if alignment arrived.
        let later = 1_001 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, later, true).contains("a"));
    }

    #[test]
    fn timed_clip_is_not_rechecked_without_rename() {
        // A timed clip must not be re-fetched just because the window elapsed;
        // only a rename (path drift) or missing slot should trigger a re-fetch.
        let d = vec![desired("a", "")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry(
                Some(ArtifactState {
                    path: "a.lrc".to_string(),
                    hash: "h".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 0, // maximally stale
                    empty: false,
                    timed: true,
                }),
            ),
        );
        // Even long after the window: still not re-fetched.
        let very_late = 2 * SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, very_late, true).is_empty());
    }

    #[test]
    fn version_bump_refetches_everything() {
        let d = vec![desired("done", "")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "done",
            entry(
                Some(ArtifactState {
                    path: "done.lrc".to_string(),
                    hash: "h".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION + 1, // resolved at a different version
                    checked_unix: 1_000,
                    empty: false,
                    timed: true,
                }),
            ),
        );
        assert!(synced_lyrics_targets(&d, &manifest, 2_000, true).contains("done"));
    }

    #[test]
    fn rename_refetches_a_written_clip() {
        let mut d = vec![desired("a", "")];
        // The audio (and so the `.lrc`) moved to a new path.
        d[0].artifacts[0].path = "new/a.lrc".to_string();
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry(
                Some(ArtifactState {
                    path: "old/a.lrc".to_string(),
                    hash: "h".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: true,
                }),
            ),
        );
        assert!(synced_lyrics_targets(&d, &manifest, 2_000, true).contains("a"));
    }

    #[test]
    fn apply_sets_timed_body_and_content_hash() {
        let mut d = vec![desired("a", "")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        let art = &d[0].artifacts[0];
        let body = art.content.as_deref().unwrap();
        assert!(body.contains("[00:00.50]hi there"));
        assert_eq!(art.hash, content_hash(body));
        assert_eq!(
            pending,
            vec![PendingCheck {
                clip_id: "a".to_string(),
                empty: false,
                timed: true,
                written_slots: vec![(ArtifactKind::Lrc, content_hash(body))],
            }]
        );
    }

    #[test]
    fn apply_untimed_fallback_marks_not_timed() {
        // When Suno returns empty alignment but the clip has lyrics, the untimed
        // plain-text fallback is written but `timed` is false so the check is
        // subject to the periodic re-check window.
        let mut d = vec![desired("a", "some lyrics")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), AlignedLyrics::default());
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        let art = &d[0].artifacts[0];
        assert!(art.content.is_some(), "untimed body written");
        let check = &pending[0];
        assert!(!check.empty, "clip has lyrics, not an instrumental");
        assert!(!check.timed, "alignment was empty -> untimed fallback");
    }

    #[test]
    fn apply_drops_instrumental_and_marks_empty() {
        let mut d = vec![desired("instr", "")];
        let mut successes = HashMap::new();
        successes.insert("instr".to_string(), AlignedLyrics::default());
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        assert!(d[0].artifacts.iter().all(|a| a.kind != ArtifactKind::Lrc));
        assert_eq!(
            pending,
            vec![PendingCheck {
                clip_id: "instr".to_string(),
                empty: true,
                timed: false,
                written_slots: vec![],
            }]
        );
    }

    #[test]
    fn apply_keeps_existing_on_fetch_failure_no_downgrade() {
        // The clip has an existing timed `.lrc` (slot present) but its fetch
        // failed this run (absent from successes). The artifact is reset to the
        // stored slot hash with no content, so reconcile skips it — the good
        // timed file is neither rewritten nor downgraded — and no check is
        // recorded, so it is retried next run.
        let mut d = vec![desired("a", "")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry(
                Some(ArtifactState {
                    path: "a.lrc".to_string(),
                    hash: "timed-hash".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: true,
                }),
            ),
        );
        let pending = apply_synced_lrc(&mut d, &manifest, &HashMap::new());

        let art = &d[0].artifacts[0];
        assert_eq!(art.hash, "timed-hash");
        assert_eq!(art.content, None);
        assert!(
            pending.is_empty(),
            "no check recorded on failure -> retried"
        );
    }

    #[test]
    fn apply_drops_write_on_failure_when_nothing_on_disk() {
        // A brand-new clip whose fetch failed: no slot to keep, so the write is
        // dropped (retried next run) rather than written empty.
        let mut d = vec![desired("a", "")];
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &HashMap::new());
        assert!(d[0].artifacts.iter().all(|a| a.kind != ArtifactKind::Lrc));
        assert!(pending.is_empty());
    }

    #[test]
    fn apply_upgrades_untimed_to_timed_when_alignment_appears() {
        // The clip previously resolved to an untimed fallback (empty alignment,
        // body written, timed: false). A re-check now returns alignment, so the
        // timed body's content hash differs and reconcile will rewrite.
        let mut d = vec![desired("a", "some lyrics")];
        let untimed_hash = "untimed".to_string();
        let mut manifest = Manifest::new();
        manifest.insert(
            "a",
            entry(
                Some(ArtifactState {
                    path: "a.lrc".to_string(),
                    hash: untimed_hash.clone(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: false,
                }),
            ),
        );
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        let pending = apply_synced_lrc(&mut d, &manifest, &successes);
        let art = &d[0].artifacts[0];
        assert!(
            art.content
                .as_deref()
                .unwrap()
                .contains("[00:00.50]hi there")
        );
        assert_ne!(art.hash, untimed_hash, "a changed body triggers a rewrite");
        assert!(pending[0].timed, "upgraded to timed");
    }

    #[test]
    fn preview_shows_write_for_targets_and_skips_resolved() {
        let mut d = vec![desired("new", ""), desired("done", "")];
        let mut manifest = Manifest::new();
        manifest.insert(
            "done",
            entry(
                Some(ArtifactState {
                    path: "done.lrc".to_string(),
                    hash: "slot-hash".to_string(),
                }),
                Some(SyncedLyricsCheck {
                    version: SYNCED_LRC_VERSION,
                    checked_unix: 1_000,
                    empty: false,
                    timed: true,
                }),
            ),
        );
        preview_synced_lrc(&mut d, &manifest, 2_000, true);
        // `new` keeps a pending hash (would write); `done` reuses its slot hash.
        assert_eq!(d[0].artifacts[0].hash, synced_lrc_source_hash("new"));
        assert_eq!(d[1].artifacts[0].hash, "slot-hash");
    }

    // ---- #354: embedded aligned-lyrics back-fill (embedded_lyrics_hash) ----

    /// A timed, resolved marker at the current version (the stable-clip baseline).
    fn timed_check() -> SyncedLyricsCheck {
        SyncedLyricsCheck {
            version: SYNCED_LRC_VERSION,
            checked_unix: 1_000,
            empty: false,
            timed: true,
        }
    }

    fn slot(path: &str, hash: &str) -> Option<ArtifactState> {
        Some(ArtifactState {
            path: path.to_string(),
            hash: hash.to_string(),
        })
    }

    #[test]
    fn needs_fetch_backfills_when_embed_missing() {
        // A timed, resolved clip whose `.lrc` slot has a hash but whose persisted
        // embed is empty is a one-time back-fill target.
        let mut e = entry(slot("a.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = String::new();
        assert!(needs_fetch(Some(&e), "a.lrc", ArtifactKind::Lrc, 2_000));
    }

    #[test]
    fn needs_fetch_skips_when_embed_matches_lrc() {
        // Already back-filled: the embed matches the `.lrc` slot, so no re-fetch.
        let mut e = entry(slot("a.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = "H".to_string();
        assert!(!needs_fetch(Some(&e), "a.lrc", ArtifactKind::Lrc, 2_000));
    }

    #[test]
    fn needs_fetch_no_backfill_without_lrc_slot() {
        // Instrumental: no `.lrc` slot and an empty embed, so the back-fill clause
        // is skipped (both sides empty) and, within the re-check window, the
        // instrumental clause is false too.
        let mut e = entry(
            None,
            Some(SyncedLyricsCheck {
                empty: true,
                timed: false,
                ..timed_check()
            }),
        );
        e.embedded_lyrics_hash = String::new();
        let within_window = 1_000 + SYNCED_LRC_RECHECK_SECS;
        assert!(!needs_fetch(
            Some(&e),
            "a.lrc",
            ArtifactKind::Lrc,
            within_window
        ));
    }

    #[test]
    fn apply_sets_embedded_lyrics_hash_from_body() {
        // A timed fetch stamps the sentinel with the `.lrc` body content hash,
        // equal to the resolved `.lrc` artifact hash (lock-step with the embed).
        let mut d = vec![desired("a", "")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        let art = &d[0].artifacts[0];
        let body = art.content.as_deref().unwrap();
        assert_eq!(d[0].embedded_lyrics_hash, content_hash(body));
        assert_eq!(d[0].embedded_lyrics_hash, art.hash);
    }

    #[test]
    fn apply_clears_embedded_lyrics_hash_for_instrumental() {
        // A previously-embedded clip that resolves to no lyrics this run drops the
        // artifact and clears the sentinel to "" (nothing embedded now).
        let mut d = vec![desired("instr", "")];
        let mut manifest = Manifest::new();
        let mut e = entry(slot("instr.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = "H".to_string();
        manifest.insert("instr", e);
        let mut successes = HashMap::new();
        successes.insert("instr".to_string(), AlignedLyrics::default());
        apply_synced_lrc(&mut d, &manifest, &successes);

        assert!(d[0].artifacts.iter().all(|a| a.kind != ArtifactKind::Lrc));
        assert_eq!(d[0].embedded_lyrics_hash, "");
    }

    #[test]
    fn apply_carries_forward_embedded_lyrics_hash_when_not_fetched() {
        // No fetch this run: the sentinel carries the PERSISTED embed value, not
        // the `.lrc` slot hash. They differ in the not-yet-embedded / failed
        // cases, which is exactly why the field is required (loop-freedom).
        let mut d = vec![desired("a", "")];
        let mut manifest = Manifest::new();
        let mut e = entry(slot("a.lrc", "slot"), Some(timed_check()));
        e.embedded_lyrics_hash = "embed".to_string();
        manifest.insert("a", e);
        apply_synced_lrc(&mut d, &manifest, &HashMap::new());

        assert_eq!(
            d[0].embedded_lyrics_hash, "embed",
            "carry-forward, not the slot hash"
        );
        // The `.lrc` artifact hash resets to the slot so reconcile skips the write.
        assert_eq!(d[0].artifacts[0].hash, "slot");
    }

    #[test]
    fn apply_carries_forward_for_clip_without_lrc_artifact() {
        // A clip with no desired `.lrc` artifact (feature off / instrumental) keeps
        // its persisted embed value and never spuriously retags.
        let mut d = vec![desired("a", "")];
        d[0].artifacts.clear();
        let mut manifest = Manifest::new();
        let mut e = entry(None, None);
        e.embedded_lyrics_hash = "H".to_string();
        manifest.insert("a", e);
        apply_synced_lrc(&mut d, &manifest, &HashMap::new());

        assert_eq!(d[0].embedded_lyrics_hash, "H");
    }

    #[test]
    fn preview_marks_backfill_target_as_pending() {
        // A stale-embed clip (lrc.hash = H, embed = "") previews the expected
        // post-fetch value H so `check` reports drift; a resolved clip carries its
        // value forward and matches the manifest (skipped).
        let mut d = vec![desired("stale", ""), desired("done", "")];
        let mut manifest = Manifest::new();
        let mut stale = entry(slot("stale.lrc", "H"), Some(timed_check()));
        stale.embedded_lyrics_hash = String::new();
        manifest.insert("stale", stale);
        let mut done = entry(slot("done.lrc", "D"), Some(timed_check()));
        done.embedded_lyrics_hash = "D".to_string();
        manifest.insert("done", done);

        preview_synced_lrc(&mut d, &manifest, 2_000, true);
        assert_eq!(
            d[0].embedded_lyrics_hash, "H",
            "target previews the back-fill"
        );
        assert_ne!(
            d[0].embedded_lyrics_hash, "",
            "differs from the manifest embed -> drift reported"
        );
        assert_eq!(
            d[1].embedded_lyrics_hash, "D",
            "resolved clip carries forward"
        );
    }

    #[test]
    fn reformat_makes_migrated_clip_a_target() {
        // An already-migrated clip (embed == lrc.hash == H, entry.format = FLAC) is
        // neither a back-fill nor a rename target, but a pending FLAC->MP3 reformat
        // re-encodes and drops the embed, so the reformat re-embed trigger fires.
        let mut manifest = Manifest::new();
        let mut e = entry(slot("a.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = "H".to_string(); // entry.format is FLAC (helper)
        manifest.insert("a", e);

        let mut reformat = vec![desired("a", "")];
        reformat[0].format = AudioFormat::Mp3;
        assert!(
            synced_lyrics_targets(&reformat, &manifest, 2_000, true).contains("a"),
            "a format change re-embeds a migrated clip"
        );

        // No format change: the same stable clip is not a target.
        let stable = vec![desired("a", "")]; // FLAC == entry.format
        assert!(
            synced_lyrics_targets(&stable, &manifest, 2_000, true).is_empty(),
            "no reformat, no back-fill -> no fetch"
        );

        // A clip with no persisted `.lrc` and a format change is not a target
        // (nothing to re-embed).
        let mut no_lrc = Manifest::new();
        no_lrc.insert(
            "a",
            entry(
                None,
                Some(SyncedLyricsCheck {
                    empty: true,
                    timed: false,
                    ..timed_check()
                }),
            ),
        );
        let mut reformat_no_lrc = vec![desired("a", "")];
        reformat_no_lrc[0].format = AudioFormat::Mp3;
        assert!(
            synced_lyrics_targets(&reformat_no_lrc, &no_lrc, 2_000, true).is_empty(),
            "no `.lrc` to re-embed -> not a target despite the reformat"
        );
    }

    // ---- F1 (#357): the deferred `.lyrics.txt` and the lyrics-only lifecycle ----

    #[test]
    fn apply_fills_lyrics_txt_from_aligned_when_clip_lyrics_empty() {
        // A real-feed lyrics-only clip: `clip.lyrics` is empty, so the deferred
        // `.lyrics.txt` body comes from Suno's fetched aligned plain text (the F1
        // fix for the previously-dead sidecar). The placeholder is replaced with
        // the resolved body and its content hash.
        let mut d = vec![desired_lyrics_only("a", "")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        let art = d[0]
            .artifacts
            .iter()
            .find(|a| a.kind == ArtifactKind::LyricsTxt)
            .expect("the `.lyrics.txt` survives a lyric fetch");
        assert_eq!(art.content.as_deref(), Some("hi there\n"));
        assert_eq!(art.hash, content_hash("hi there\n"));
        assert_eq!(
            pending,
            vec![PendingCheck {
                clip_id: "a".to_string(),
                empty: false,
                timed: true,
                written_slots: vec![(ArtifactKind::LyricsTxt, content_hash("hi there\n"))],
            }]
        );
    }

    #[test]
    fn apply_prefers_clip_lyrics_over_aligned_for_lyrics_txt() {
        // When the feed DOES carry `clip.lyrics`, they win over the aligned plain
        // text, matching the historical `.lyrics.txt` body (back-compat).
        let mut d = vec![desired_lyrics_only("a", "my own words")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment()); // "hi there"
        apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        let art = d[0]
            .artifacts
            .iter()
            .find(|a| a.kind == ArtifactKind::LyricsTxt)
            .unwrap();
        assert_eq!(art.content.as_deref(), Some("my own words\n"));
    }

    #[test]
    fn apply_drops_lyrics_txt_for_instrumental() {
        // Empty `clip.lyrics` AND empty alignment -> a genuine instrumental: the
        // `.lyrics.txt` is dropped (no empty file) and an empty marker recorded.
        let mut d = vec![desired_lyrics_only("a", "")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), AlignedLyrics::default());
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        assert!(
            d[0].artifacts
                .iter()
                .all(|a| a.kind != ArtifactKind::LyricsTxt),
            "no `.lyrics.txt` written for an instrumental"
        );
        assert_eq!(
            pending,
            vec![PendingCheck {
                clip_id: "a".to_string(),
                empty: true,
                timed: false,
                written_slots: vec![],
            }]
        );
    }

    #[test]
    fn apply_keeps_lyrics_txt_on_failed_fetch() {
        // A lyrics-only clip with an existing `.lyrics.txt` slot whose fetch
        // failed (absent from `successes`): the artifact resets to the stored slot
        // hash with no content, so reconcile skips it (the good file is kept), and
        // no marker is recorded, so it retries next run.
        let mut d = vec![desired_lyrics_only("a", "")];
        let mut manifest = Manifest::new();
        let mut e = entry(None, Some(timed_check()));
        e.lyrics_txt = Some(ArtifactState {
            path: "a.lyrics.txt".to_string(),
            hash: "stored".to_string(),
        });
        manifest.insert("a", e);
        let pending = apply_synced_lrc(&mut d, &manifest, &HashMap::new());

        let art = d[0]
            .artifacts
            .iter()
            .find(|a| a.kind == ArtifactKind::LyricsTxt)
            .unwrap();
        assert_eq!(art.hash, "stored", "reset to the stored slot -> skipped");
        assert_eq!(art.content, None);
        assert!(pending.is_empty(), "no marker on failure -> retried");
    }

    #[test]
    fn lyrics_txt_body_has_exactly_one_trailing_newline() {
        // Both body sources normalise to exactly one trailing newline (matching
        // the `render_clip_lyrics` convention): `clip.lyrics` via
        // `render_clip_lyrics`, and the aligned plain text via the explicit append.
        for lyrics in ["from the feed", ""] {
            let mut d = vec![desired_lyrics_only("a", lyrics)];
            let mut successes = HashMap::new();
            successes.insert("a".to_string(), one_line_alignment());
            apply_synced_lrc(&mut d, &Manifest::new(), &successes);
            let body = d[0]
                .artifacts
                .iter()
                .find(|a| a.kind == ArtifactKind::LyricsTxt)
                .unwrap()
                .content
                .clone()
                .unwrap();
            assert!(body.ends_with('\n'), "one trailing newline: {body:?}");
            assert!(!body.ends_with("\n\n"), "not two: {body:?}");
        }
    }

    #[test]
    fn lyrics_only_clip_is_a_fetch_target() {
        // A lyrics-only clip (only a deferred `.lyrics.txt` desired, no `.lrc`) is
        // a first-sight alignment-fetch target, so its body can be resolved.
        let d = vec![desired_lyrics_only("a", "")];
        assert!(
            synced_lyrics_targets(&d, &Manifest::new(), 1_000, true).contains("a"),
            "a fresh lyrics-only clip is fetched"
        );
    }

    #[test]
    fn lyrics_only_clip_is_a_stable_fetch_target_after_first_run() {
        // Convergence (F1): a lyrics-only clip is fetched once, then its
        // rename-drift check anchors on the `.lyrics.txt` slot, so it is NOT a
        // target on the next run. This is the fix for the old `unwrap_or(true)`
        // that re-fetched a lyrics-only clip on every run forever.
        let d = vec![desired_lyrics_only("a", "")];

        // First run: unseen clip -> a target.
        assert!(
            synced_lyrics_targets(&d, &Manifest::new(), 1_000, true).contains("a"),
            "fetched once"
        );

        // After the fetch resolved and the marker + `.lyrics.txt` slot landed:
        let mut manifest = Manifest::new();
        let mut e = entry(None, Some(timed_check()));
        e.lyrics_txt = Some(ArtifactState {
            path: "a.lyrics.txt".to_string(),
            hash: "body-hash".to_string(),
        });
        manifest.insert("a", e);
        assert!(
            synced_lyrics_targets(&d, &manifest, 2_000, true).is_empty(),
            "a resolved lyrics-only clip converges (no forever re-fetch)"
        );

        // But a rename still moves it: the drifted path re-fetches.
        let mut renamed = d.clone();
        renamed[0].artifacts[0].path = "new/a.lyrics.txt".to_string();
        assert!(
            synced_lyrics_targets(&renamed, &manifest, 2_000, true).contains("a"),
            "a rename re-fetches so the sidecar moves with the audio"
        );
    }

    #[test]
    fn lyrics_only_marker_anchors_on_lyrics_txt_slot() {
        // A lyrics-only clip records its single marker listing the `.lyrics.txt`
        // slot it wrote, so durability tracks the file actually written.
        let mut d = vec![desired_lyrics_only("a", "")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);
        assert_eq!(
            pending[0].written_slots,
            vec![(ArtifactKind::LyricsTxt, content_hash("hi there\n"))]
        );
    }

    #[test]
    fn both_slots_recorded_in_the_single_pending_check_when_both_desired() {
        // A clip with BOTH sidecars desired records exactly ONE marker whose
        // `written_slots` lists BOTH the `.lrc` and the `.lyrics.txt` (each body
        // resolved from the same fetched alignment). The marker is durable only
        // once every listed slot has landed, so back-filling one never masks an
        // unwritten other.
        let mut d = vec![desired_both("a", "")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        assert_eq!(pending.len(), 1, "one marker per clip");
        let lrc = &d[0]
            .artifacts
            .iter()
            .find(|a| a.kind == ArtifactKind::Lrc)
            .expect("the `.lrc` body is resolved");
        assert!(lrc.content.is_some(), "the `.lrc` body is resolved");
        assert_eq!(
            pending[0].written_slots,
            vec![
                (ArtifactKind::Lrc, lrc.hash.clone()),
                (ArtifactKind::LyricsTxt, content_hash("hi there\n")),
            ],
            "both slots recorded, `.lrc` first"
        );
        assert!(
            d[0].artifacts
                .iter()
                .any(|a| a.kind == ArtifactKind::LyricsTxt && a.content.is_some()),
            "the `.lyrics.txt` body is resolved too"
        );
    }

    #[test]
    fn preview_marks_lyrics_txt_pending() {
        // Preview mirrors the `.lrc`: a lyrics-only target keeps the placeholder
        // source hash (previews as a write); a resolved one reuses its stored slot
        // hash (previews as skipped).
        let mut d = vec![
            desired_lyrics_only("new", ""),
            desired_lyrics_only("done", ""),
        ];
        let mut manifest = Manifest::new();
        let mut done = entry(None, Some(timed_check()));
        done.lyrics_txt = Some(ArtifactState {
            path: "done.lyrics.txt".to_string(),
            hash: "slot-hash".to_string(),
        });
        manifest.insert("done", done);

        preview_synced_lrc(&mut d, &manifest, 2_000, true);
        let new_art = d[0]
            .artifacts
            .iter()
            .find(|a| a.kind == ArtifactKind::LyricsTxt)
            .unwrap();
        let done_art = d[1]
            .artifacts
            .iter()
            .find(|a| a.kind == ArtifactKind::LyricsTxt)
            .unwrap();
        assert_eq!(new_art.hash, lyrics_txt_source_hash("new"));
        assert_eq!(done_art.hash, "slot-hash");
    }

    // ---- F1 blocker (#357 review): back-fill a `.lyrics.txt` for a clip whose
    // `.lrc` has already converged, and never mark resolved on a partial write ----

    #[test]
    fn both_sidecars_lrc_already_resolved_backfills_lyrics_txt() {
        // The blocker: a clip whose `.lrc` has FULLY converged (timed marker at
        // the current version, matching path, embed in sync) but whose
        // `.lyrics.txt` was never written (lyrics_sidecar newly enabled) MUST
        // still be a fetch target so the `.lyrics.txt` back-fills, INDEPENDENT of
        // the converged `.lrc`. Then a second run converges to no re-fetch.
        let mut manifest = Manifest::new();
        let mut e = entry(slot("a.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = "H".to_string(); // the `.lrc` is fully back-filled
        // ...but there is no `.lyrics.txt` slot yet.
        manifest.insert("a", e);

        let d = vec![desired_both("a", "")];

        // 1) The clip IS a target despite the converged `.lrc`.
        assert!(
            synced_lyrics_targets(&d, &manifest, 2_000, true).contains("a"),
            "an unresolved `.lyrics.txt` re-targets even when the `.lrc` converged"
        );

        // 2) The fetch back-fills the `.lyrics.txt` (from the aligned plain text).
        let mut d2 = d.clone();
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        let pending = apply_synced_lrc(&mut d2, &manifest, &successes);
        let txt = d2[0]
            .artifacts
            .iter()
            .find(|a| a.kind == ArtifactKind::LyricsTxt)
            .expect("the `.lyrics.txt` is back-filled");
        assert_eq!(txt.content.as_deref(), Some("hi there\n"));
        assert_eq!(pending.len(), 1, "one marker");
        assert!(
            pending[0]
                .written_slots
                .iter()
                .any(|(k, _)| *k == ArtifactKind::LyricsTxt),
            "the marker lists the back-filled `.lyrics.txt` slot"
        );

        // 3) Once the `.lyrics.txt` slot has landed, the clip converges: BOTH
        //    slots are resolved, so no forever re-fetch.
        let mut converged = Manifest::new();
        let mut e2 = entry(slot("a.lrc", "H"), Some(timed_check()));
        e2.embedded_lyrics_hash = "H".to_string();
        e2.lyrics_txt = Some(ArtifactState {
            path: "a.lyrics.txt".to_string(),
            hash: content_hash("hi there\n"),
        });
        converged.insert("a", e2);
        assert!(
            synced_lyrics_targets(&d, &converged, 3_000, true).is_empty(),
            "both slots resolved -> converged (no re-fetch loop)"
        );
    }

    #[test]
    fn inline_lyrics_clip_still_gets_lyrics_txt() {
        // Regression: the deferred `.lyrics.txt` must still be produced for a clip
        // whose feed carries inline `clip.lyrics` (the old eager emit wrote it
        // directly; the deferred model resolves it via the fetch path). A fresh
        // clip with both sidecars is a target, and the fetch writes the
        // `.lyrics.txt` from the inline lyrics.
        let d0 = desired_both("a", "hello world");
        assert!(
            synced_lyrics_targets(std::slice::from_ref(&d0), &Manifest::new(), 1_000, true)
                .contains("a"),
            "a fresh clip with both sidecars is a fetch target"
        );

        let mut d = vec![d0];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        apply_synced_lrc(&mut d, &Manifest::new(), &successes);
        let txt = d[0]
            .artifacts
            .iter()
            .find(|a| a.kind == ArtifactKind::LyricsTxt)
            .expect("the `.lyrics.txt` is written");
        assert_eq!(
            txt.content.as_deref(),
            Some("hello world\n"),
            "inline `clip.lyrics` win over the aligned text"
        );
    }

    #[test]
    fn lyrics_txt_marker_lists_both_slots_so_a_partial_write_is_retried() {
        // Marker durability across both slots (the secondary hole): when a fetch
        // writes BOTH sidecars, the returned marker lists BOTH, so the caller (see
        // `record_synced_lyrics_checks`) only stamps the clip resolved once every
        // listed slot has landed. If the `.lrc` write lands but the `.lyrics.txt`
        // fails non-fatally, no marker is recorded and the missing `.lyrics.txt`
        // slot re-targets next run (proven by the back-fill test above).
        let mut d = vec![desired_both("a", "")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        let lrc_hash = d[0]
            .artifacts
            .iter()
            .find(|a| a.kind == ArtifactKind::Lrc)
            .unwrap()
            .hash
            .clone();
        assert_eq!(
            pending[0].written_slots,
            vec![
                (ArtifactKind::Lrc, lrc_hash),
                (ArtifactKind::LyricsTxt, content_hash("hi there\n")),
            ],
            "the marker enumerates every written slot for the caller to gate on"
        );
    }
}
