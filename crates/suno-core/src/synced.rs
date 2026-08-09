//! Pure synced-lyrics resolution: which clips to fetch alignment for, and how
//! each fetched result maps onto a clip's desired `.lrc` and deferred
//! `.lyrics.txt` artifacts.
//!
//! The alignment fetch itself is IO and lives in the CLI (through the `Http`
//! port); everything here is pure so the fetch-gating, the timed/untimed body
//! choice, the "keep existing on failure" rule, and the instrumental "checked"
//! marker are unit-tested without a network.
//!
//! The audio embed and optional timed sidecar have separate refresh rules. A
//! track with no inline lyrics and no aligned fallback fingerprint is checked on
//! every run until words appear. Once plain lyrics are embedded, the fingerprint
//! converges. Separately, a clip with inline plain lyrics but no timing is
//! re-checked after [`SYNCED_LRC_RECHECK_SECS`] when `.lrc` is enabled, so later
//! alignment can upgrade the sidecar and `SYLT`.
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
//! Plain lyrics embedded in audio are resolved independently of either sidecar.
//! A real-feed clip whose inline `clip.lyrics` is empty is fetched whenever its
//! manifest has no aligned fallback fingerprint, so a later-available lyric is
//! back-filled on the next run even when both sidecar features are off.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use crate::hash::{
    SYNCED_LRC_VERSION, TIMED_LYRICS_VERSION, content_hash, lyrics_txt_source_hash,
    synced_lrc_source_hash_with_timing,
};
use crate::lyrics::{
    AlignedLyrics, render_clip_lrc, render_clip_lyrics, render_synced_lrc_with_timing,
};
use crate::manifest::{Manifest, ManifestEntry};
use crate::model::Clip;
use crate::reconcile::Desired;
use crate::vocab::{ArtifactKind, LyricsTiming};

/// How long an untimed sidecar fallback is trusted before checking for timing
/// again (14 days). Tracks missing plain embedded lyrics bypass this window and
/// are checked every run.
pub const SYNCED_LRC_RECHECK_SECS: u64 = 14 * 24 * 60 * 60;

/// One clip's synced-lyrics outcome this run, for the caller to record as a
/// manifest [`SyncedLyricsCheck`](crate::SyncedLyricsCheck) once the sidecar
/// write (if any) has safely landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCheck {
    /// The clip this outcome concerns.
    pub clip_id: String,
    /// Whether the clip resolved to no lyrics. No sidecar is written; the audio
    /// embed gate still retries while its fingerprint remains empty.
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
    /// Timed granularity resolved for this fetch when an LRC surface was
    /// requested. `None` for plain `.lyrics.txt` or embed-only probes.
    pub timing: Option<LyricsTiming>,
    /// Exact timed embed fingerprint that must land before the mode/version can
    /// be committed. Present only for timed MP3/WAV output.
    pub timed_embed_hash: Option<String>,
}

fn entry_has_timed_surface(entry: &ManifestEntry) -> bool {
    !entry.embedded_timed_lyrics_hash.is_empty()
        || (entry.lrc.is_some()
            && entry
                .synced_lyrics
                .as_ref()
                .is_some_and(|check| check.timed))
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
    timing: LyricsTiming,
) -> bool {
    let Some(entry) = entry else {
        return true; // never downloaded -> resolve on first sight
    };
    let Some(check) = entry.synced_lyrics.as_ref() else {
        return true; // never resolved (e.g. downloaded before the feature)
    };
    if check.version != SYNCED_LRC_VERSION {
        return true; // the render changed -> re-resolve and re-render
    }
    if desired_kind == ArtifactKind::Lrc
        && (check.timed_version != TIMED_LYRICS_VERSION || check.timing != Some(timing))
    {
        return true;
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

/// Whether the audio file needs Suno's aligned lyrics as a plain-text fallback.
///
/// Inline `clip.lyrics` already travels through `meta_hash`, so it needs no
/// alignment lookup. Otherwise a new or unfilled track is probed every run until
/// a successful audio write stamps `embedded_lyrics_hash`. A format change also
/// re-fetches a prior fallback because the new container must recreate the text.
fn embed_needs_fetch(d: &Desired, entry: Option<&ManifestEntry>) -> bool {
    if !d.clip.lyrics.trim().is_empty() {
        return false;
    }
    match entry {
        None => true,
        Some(entry) => entry.embedded_lyrics_hash.is_empty() || entry.format != d.format,
    }
}

/// Whether this desired audio format and sidecar set writes an ID3 `SYLT`.
fn wants_timed_embed(d: &Desired) -> bool {
    matches!(
        d.format,
        crate::vocab::AudioFormat::Mp3 | crate::vocab::AudioFormat::Wav
    ) && d.artifacts.iter().any(|a| a.kind == ArtifactKind::Lrc)
}

/// Whether timed lyrics need fetching for an MP3/WAV embed.
///
/// A format change must recreate the frame. The second clause is the additive
/// manifest migration: old files with a timed sidecar marker predate the
/// dedicated `embedded_timed_lyrics_hash`, so they get one bounded back-fill.
fn timed_embed_needs_fetch(
    d: &Desired,
    entry: Option<&ManifestEntry>,
    timing: LyricsTiming,
) -> bool {
    if !wants_timed_embed(d) {
        return false;
    }
    match entry {
        None => true,
        Some(entry) => {
            entry.format != d.format
                || entry.synced_lyrics.as_ref().is_none_or(|check| {
                    check.timed_version != TIMED_LYRICS_VERSION || check.timing != Some(timing)
                })
                || (entry.embedded_timed_lyrics_hash.is_empty()
                    && entry
                        .synced_lyrics
                        .as_ref()
                        .is_some_and(|check| check.timed))
        }
    }
}

/// Whether a manifest entry is expected to carry an existing `SYLT`.
fn entry_has_timed_embed(entry: &ManifestEntry) -> bool {
    !entry.embedded_timed_lyrics_hash.is_empty()
        || (matches!(
            entry.format,
            crate::vocab::AudioFormat::Mp3 | crate::vocab::AudioFormat::Wav
        ) && entry.lrc.is_some()
            && entry
                .synced_lyrics
                .as_ref()
                .is_some_and(|check| check.timed))
}

/// The clip ids whose alignment must be fetched this run, in a stable order.
///
/// A clip is a target when its audio still lacks an aligned plain-text fallback
/// or when ANY desired lyric sidecar slot needs resolution. The embed check is
/// independent of sidecar settings and retries a lyric-less result every run;
/// sidecars retain their own version, path, and timed-upgrade rules.
pub fn synced_lyrics_targets(
    desired: &[Desired],
    manifest: &Manifest,
    now_unix: u64,
) -> BTreeSet<String> {
    synced_lyrics_targets_with_timing(desired, manifest, now_unix, LyricsTiming::Line)
}

/// Mode-aware variant of [`synced_lyrics_targets`].
pub fn synced_lyrics_targets_with_timing(
    desired: &[Desired],
    manifest: &Manifest,
    now_unix: u64,
    timing: LyricsTiming,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for d in desired {
        let entry = manifest.get(&d.clip.id);
        let embed_needs = embed_needs_fetch(d, entry);
        let timed_embed_needs = timed_embed_needs_fetch(d, entry, timing);
        let any_slot_needs = d
            .artifacts
            .iter()
            .filter(|a| matches!(a.kind, ArtifactKind::Lrc | ArtifactKind::LyricsTxt))
            .any(|a| needs_fetch(entry, a.path.as_str(), a.kind, now_unix, timing));
        if embed_needs || timed_embed_needs || any_slot_needs {
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
    apply_synced_lrc_with_timing(desired, manifest, successes, LyricsTiming::Line)
}

/// Mode-aware variant of [`apply_synced_lrc`].
pub fn apply_synced_lrc_with_timing(
    desired: &mut [Desired],
    manifest: &Manifest,
    successes: &HashMap<String, AlignedLyrics>,
    timing: LyricsTiming,
) -> Vec<PendingCheck> {
    let mut pending = Vec::new();
    for d in desired.iter_mut() {
        let entry = manifest.get(&d.clip.id);
        // Carry forward the persisted embed fingerprint unless this run fetched
        // usable fallback text. A failed or empty lookup never invents a
        // successful state and therefore remains eligible for the next run.
        d.embedded_lyrics_hash = entry
            .map(|e| e.embedded_lyrics_hash.clone())
            .unwrap_or_default();
        d.embedded_timed_lyrics_hash = entry
            .map(|e| e.embedded_timed_lyrics_hash.clone())
            .unwrap_or_default();

        let aligned = successes.get(&d.clip.id);
        let plain_fallback = aligned_plain_fallback(&d.clip, aligned);
        if let Some(text) = &plain_fallback {
            d.embedded_lyrics_hash = content_hash(text);
        }
        // Re-encoding starts from fresh audio bytes. If the current file carries
        // aligned fallback lyrics but this run could not recover their text,
        // keep the old container rather than silently dropping the embed.
        let format_changes = entry.is_some_and(|entry| entry.format != d.format);
        if format_changes
            && (!matches!(
                d.format,
                crate::vocab::AudioFormat::Mp3 | crate::vocab::AudioFormat::Wav
            ) || !wants_timed_embed(d))
        {
            d.embedded_timed_lyrics_hash = String::new();
        }
        let plain_reencode_unsafe = entry.is_some_and(|entry| {
            format_changes
                && d.clip.lyrics.trim().is_empty()
                && !entry.embedded_lyrics_hash.is_empty()
                && plain_fallback.is_none()
        });

        // Resolve BOTH lyric sidecars from the same fetched alignment, each on
        // its own slot. The audio embed is independent and was resolved above.
        // The two sidecar outcomes fold into one durable marker that lists every
        // slot written, so back-filling one never masks an unwritten other.
        let lrc_desired = d.artifacts.iter().any(|a| a.kind == ArtifactKind::Lrc);
        let lrc = apply_lrc_slot(d, manifest, aligned, timing);
        let lyrics_txt = apply_lyrics_txt_slot(d, manifest, aligned);
        let existing_timed_surface = entry.is_some_and(entry_has_timed_surface);
        let mut timed_embed_hash = None;
        if wants_timed_embed(d)
            && matches!(lrc, SlotOutcome::Wrote(_))
            && let Some(aligned) = aligned.filter(|aligned| !aligned.is_empty())
        {
            let hash = timed_embed_fingerprint(aligned, timing);
            d.embedded_timed_lyrics_hash = hash.clone();
            timed_embed_hash = Some(hash);
        }
        let timed_reencode_unsafe = entry.is_some_and(|entry| {
            format_changes
                && wants_timed_embed(d)
                && entry_has_timed_embed(entry)
                && aligned.is_none_or(AlignedLyrics::is_empty)
        });
        d.lyrics_reencode_safe = !(plain_reencode_unsafe || timed_reencode_unsafe);
        if let Some(check) = build_pending_check(
            &d.clip.id,
            aligned,
            &lrc,
            &lyrics_txt,
            (lrc_desired
                && aligned.is_some_and(|aligned| !aligned.is_empty() || !existing_timed_surface))
            .then_some(timing),
            timed_embed_hash,
        ) {
            pending.push(check);
        } else if d.clip.lyrics.trim().is_empty()
            && entry.is_none_or(|entry| entry.embedded_lyrics_hash.is_empty())
            && aligned.is_some_and(AlignedLyrics::is_empty)
        {
            // Embed-only negative result: persist the fact that an executing run
            // checked and found no lyrics. Execution still probes again next run,
            // but no-fetch `check` can avoid reporting a permanent false-positive
            // retag for known instrumentals.
            pending.push(PendingCheck {
                clip_id: d.clip.id.clone(),
                empty: true,
                timed: false,
                written_slots: Vec::new(),
                timing: None,
                timed_embed_hash: None,
            });
        }
    }
    pending
}

/// Aligned plain text for the audio-tag fallback, only when inline lyrics are
/// absent and the fetched alignment carries non-whitespace content.
fn aligned_plain_fallback(clip: &Clip, aligned: Option<&AlignedLyrics>) -> Option<String> {
    if !clip.lyrics.trim().is_empty() {
        return None;
    }
    let text = aligned?.plain_text();
    (!text.trim().is_empty()).then_some(text)
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
    timing: Option<LyricsTiming>,
    timed_embed_hash: Option<String>,
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
        timing,
        timed_embed_hash,
    })
}

/// Resolve a clip's desired `.lrc` artifact from the fetched alignment (or keep
/// the stored slot on a miss). Returns the slot's [`SlotOutcome`]: `Inert` when
/// the clip has no `.lrc` desired or was not fetched, `Wrote(hash)` when a body
/// was rendered, `Instrumental` when the fetch resolved to no lyrics. Also stamps
fn apply_lrc_slot(
    d: &mut Desired,
    manifest: &Manifest,
    aligned: Option<&AlignedLyrics>,
    timing: LyricsTiming,
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
    if aligned.is_empty()
        && manifest
            .get(&clip_id)
            .is_some_and(|entry| entry.lrc.is_some() && entry_has_timed_surface(entry))
        && let Some(hash) = slot_hash
    {
        let artifact = &mut d.artifacts[idx];
        artifact.hash = hash;
        artifact.content = None;
        return SlotOutcome::Inert;
    }
    let timed = !aligned.is_empty();
    let body = if timed {
        render_synced_lrc_with_timing(&d.clip, &d.lineage, aligned, timing)
    } else {
        render_clip_lrc(&d.clip, &d.lineage)
    };
    match body {
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

fn timed_embed_fingerprint(aligned: &AlignedLyrics, timing: LyricsTiming) -> String {
    let mut source = format!("timed-lyrics/v{TIMED_LYRICS_VERSION}/{timing}\n");
    for (offset_ms, text) in aligned.sylt_entries_with_timing(timing) {
        let _ = writeln!(source, "{offset_ms}:{}:{text}", text.len());
    }
    content_hash(&source)
}

/// Resolve a clip's deferred `.lyrics.txt` artifact from `clip.lyrics` (preferred)
/// else the fetched aligned plain text (or keep the stored slot on a miss).
/// Returns the slot's [`SlotOutcome`]: `Inert` when the clip has no `.lyrics.txt`
/// desired or was not fetched, `Wrote(hash)` when a body was rendered,
/// `Instrumental` when neither source yields lyrics. The `.lyrics.txt` carries no
/// embed, which is resolved independently before either sidecar.
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

/// Adjust embedded-lyric fingerprints and optional sidecars for a dry run,
/// without fetching alignment.
///
/// Unknown plain or timed embeds receive stable placeholder hashes so `check`
/// reports the potential retag. A prior empty result suppresses the plain
/// placeholder, preventing known instrumentals from making every preview dirty;
/// executing runs still probe them again. Sidecar slots follow their own fetch
/// decisions and use pending source hashes. The result is an upper bound because
/// a no-fetch preview cannot know whether Suno will return empty alignment.
pub fn preview_synced_lrc(
    desired: &mut [Desired],
    manifest: &Manifest,
    now_unix: u64,
    enabled: bool,
) {
    preview_synced_lrc_with_timing(desired, manifest, now_unix, enabled, LyricsTiming::Line);
}

/// Mode-aware variant of [`preview_synced_lrc`].
pub fn preview_synced_lrc_with_timing(
    desired: &mut [Desired],
    manifest: &Manifest,
    now_unix: u64,
    enabled: bool,
    timing: LyricsTiming,
) {
    for d in desired.iter_mut() {
        let entry = manifest.get(&d.clip.id);
        // Carry forward durable state before adding preview-only placeholders.
        d.embedded_lyrics_hash = entry
            .map(|e| e.embedded_lyrics_hash.clone())
            .unwrap_or_default();
        d.embedded_timed_lyrics_hash = entry
            .map(|e| e.embedded_timed_lyrics_hash.clone())
            .unwrap_or_default();
        if d.clip.lyrics.trim().is_empty()
            && entry.is_some_and(|entry| {
                entry.embedded_lyrics_hash.is_empty()
                    && entry
                        .synced_lyrics
                        .as_ref()
                        .is_none_or(|check| !check.empty)
            })
        {
            d.embedded_lyrics_hash =
                content_hash(&format!("embedded-lyrics-preview/{}", d.clip.id));
        }
        let timed_sidecar_needs = d
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Lrc)
            .is_some_and(|artifact| {
                needs_fetch(entry, &artifact.path, ArtifactKind::Lrc, now_unix, timing)
            });
        if entry.is_some()
            && wants_timed_embed(d)
            && (timed_embed_needs_fetch(d, entry, timing) || timed_sidecar_needs)
        {
            d.embedded_timed_lyrics_hash =
                content_hash(&format!("timed-lyrics-preview/{}", d.clip.id));
        }

        // `.lrc` preview.
        if let Some(idx) = d.artifacts.iter().position(|a| a.kind == ArtifactKind::Lrc) {
            let path = d.artifacts[idx].path.clone();
            if enabled && needs_fetch(entry, &path, ArtifactKind::Lrc, now_unix, timing) {
                d.artifacts[idx].hash = synced_lrc_source_hash_with_timing(&d.clip.id, timing);
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
            if enabled && needs_fetch(entry, &path, ArtifactKind::LyricsTxt, now_unix, timing) {
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
    use crate::hash::synced_lrc_source_hash;
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
            embedded_timed_lyrics_hash: String::new(),
            lyrics_reencode_safe: true,
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
    fn targets_missing_embed_without_sidecars() {
        let mut d = vec![desired("a", "")];
        d[0].artifacts.clear();
        let manifest = Manifest::new();
        assert!(synced_lyrics_targets(&d, &manifest, 0).contains("a"));
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
                    timed_version: TIMED_LYRICS_VERSION,
                    timing: Some(LyricsTiming::Line),
                }),
            ),
        );
        let targets = synced_lyrics_targets(&d, &manifest, 2_000);
        assert!(targets.contains("new"));
        assert!(!targets.contains("done"));
    }

    #[test]
    fn missing_plain_lyrics_are_rechecked_every_run() {
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
                    timed_version: TIMED_LYRICS_VERSION,
                    timing: Some(LyricsTiming::Line),
                }),
            ),
        );
        // Missing plain lyrics ignore the timed-sidecar re-check window.
        let soon = 1_000 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, soon).contains("instr"));
        let later = 1_001 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, later).contains("instr"));
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
                    timed_version: TIMED_LYRICS_VERSION,
                    timing: Some(LyricsTiming::Line),
                }),
            ),
        );
        // Within the window: no re-fetch (avoids churn on every run).
        let soon = 1_000 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, soon).is_empty());
        // Past the window: re-checked, to upgrade to timed if alignment arrived.
        let later = 1_001 + SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, later).contains("a"));
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
                    timed_version: TIMED_LYRICS_VERSION,
                    timing: Some(LyricsTiming::Line),
                }),
            ),
        );
        // Even long after the window: still not re-fetched.
        let very_late = 2 * SYNCED_LRC_RECHECK_SECS;
        assert!(synced_lyrics_targets(&d, &manifest, very_late).is_empty());
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
                    timed_version: TIMED_LYRICS_VERSION,
                    timing: Some(LyricsTiming::Line),
                }),
            ),
        );
        assert!(synced_lyrics_targets(&d, &manifest, 2_000).contains("done"));
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
                    timed_version: TIMED_LYRICS_VERSION,
                    timing: Some(LyricsTiming::Line),
                }),
            ),
        );
        assert!(synced_lyrics_targets(&d, &manifest, 2_000).contains("a"));
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
                timing: Some(LyricsTiming::Line),
                timed_embed_hash: None,
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
                timing: Some(LyricsTiming::Line),
                timed_embed_hash: None,
            }]
        );
    }

    #[test]
    fn embed_only_empty_probe_is_recorded_for_future_preview() {
        let mut d = vec![desired("instr", "")];
        d[0].artifacts.clear();
        let successes = HashMap::from([("instr".to_string(), AlignedLyrics::default())]);

        let pending = apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        assert_eq!(
            pending,
            vec![PendingCheck {
                clip_id: "instr".to_string(),
                empty: true,
                timed: false,
                written_slots: Vec::new(),
                timing: None,
                timed_embed_hash: None,
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
                    timed_version: TIMED_LYRICS_VERSION,
                    timing: Some(LyricsTiming::Line),
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
                    timed_version: TIMED_LYRICS_VERSION,
                    timing: Some(LyricsTiming::Line),
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
                    timed_version: TIMED_LYRICS_VERSION,
                    timing: Some(LyricsTiming::Line),
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
            timed_version: TIMED_LYRICS_VERSION,
            timing: Some(LyricsTiming::Line),
        }
    }

    fn slot(path: &str, hash: &str) -> Option<ArtifactState> {
        Some(ArtifactState {
            path: path.to_string(),
            hash: hash.to_string(),
        })
    }

    #[test]
    fn embed_target_backfills_without_lrc_coupling() {
        let mut e = entry(slot("a.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = String::new();
        let mut d = desired("a", "");
        d.artifacts.clear();
        assert!(embed_needs_fetch(&d, Some(&e)));
    }

    #[test]
    fn embed_target_skips_any_nonempty_fingerprint() {
        let mut e = entry(slot("a.lrc", "sidecar"), Some(timed_check()));
        e.embedded_lyrics_hash = "plain-text".to_string();
        let mut d = desired("a", "");
        d.artifacts.clear();
        assert!(!embed_needs_fetch(&d, Some(&e)));
        assert!(!needs_fetch(
            Some(&e),
            "a.lrc",
            ArtifactKind::Lrc,
            2_000,
            LyricsTiming::Line
        ));
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
            within_window,
            LyricsTiming::Line
        ));
    }

    #[test]
    fn apply_hashes_exact_plain_embed_independently_of_lrc() {
        let mut d = vec![desired("a", "")];
        let mut successes = HashMap::new();
        successes.insert("a".to_string(), one_line_alignment());
        apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        let art = &d[0].artifacts[0];
        assert_eq!(d[0].embedded_lyrics_hash, content_hash("hi there"));
        assert_ne!(d[0].embedded_lyrics_hash, art.hash);
    }

    #[test]
    fn apply_hashes_timed_embed_for_mp3_with_lrc_enabled() {
        let mut d = vec![desired("a", "")];
        d[0].format = AudioFormat::Mp3;
        let successes = HashMap::from([("a".to_string(), one_line_alignment())]);

        apply_synced_lrc(&mut d, &Manifest::new(), &successes);

        let lrc = d[0]
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Lrc)
            .unwrap();
        assert_eq!(
            d[0].embedded_timed_lyrics_hash,
            timed_embed_fingerprint(&one_line_alignment(), LyricsTiming::Line)
        );
        assert_ne!(d[0].embedded_timed_lyrics_hash, lrc.hash);
    }

    #[test]
    fn timed_embed_migration_targets_existing_mp3_once() {
        let mut d = vec![desired("a", "")];
        d[0].format = AudioFormat::Mp3;
        let mut manifest = Manifest::new();
        let mut e = entry(slot("a.lrc", "timed"), Some(timed_check()));
        e.format = AudioFormat::Mp3;
        e.embedded_lyrics_hash = "plain".to_string();
        manifest.insert("a", e);

        assert!(synced_lyrics_targets(&d, &manifest, 2_000).contains("a"));

        manifest
            .entries
            .get_mut("a")
            .unwrap()
            .embedded_timed_lyrics_hash = "timed".to_string();
        assert!(synced_lyrics_targets(&d, &manifest, 2_000).is_empty());
    }

    #[test]
    fn legacy_mixed_timing_targets_even_with_an_existing_embed_hash() {
        let mut d = vec![desired("a", "")];
        d[0].format = AudioFormat::Mp3;
        let mut legacy = timed_check();
        legacy.timed_version = 0;
        legacy.timing = None;
        let mut entry = entry(slot("a.lrc", "line-body"), Some(legacy));
        entry.format = AudioFormat::Mp3;
        entry.embedded_lyrics_hash = "plain".to_owned();
        entry.embedded_timed_lyrics_hash = "legacy-word-sylt".to_owned();
        let mut manifest = Manifest::new();
        manifest.insert("a", entry);

        assert!(
            synced_lyrics_targets_with_timing(&d, &manifest, 2_000, LyricsTiming::Line)
                .contains("a")
        );

        let stored = manifest.entries.get_mut("a").unwrap();
        stored.synced_lyrics = Some(timed_check());
        assert!(
            synced_lyrics_targets_with_timing(&d, &manifest, 2_000, LyricsTiming::Line).is_empty()
        );
    }

    #[test]
    fn changing_timing_mode_rewrites_lrc_and_timed_embed() {
        let mut d = vec![desired("a", "")];
        d[0].format = AudioFormat::Mp3;
        let mut entry = entry(slot("a.lrc", "line-body"), Some(timed_check()));
        entry.format = AudioFormat::Mp3;
        entry.embedded_lyrics_hash = "plain".to_owned();
        entry.embedded_timed_lyrics_hash = "line-sylt".to_owned();
        let mut manifest = Manifest::new();
        manifest.insert("a", entry);

        assert!(
            synced_lyrics_targets_with_timing(&d, &manifest, 2_000, LyricsTiming::Word)
                .contains("a")
        );
        let successes = HashMap::from([("a".to_owned(), one_line_alignment())]);
        let pending =
            apply_synced_lrc_with_timing(&mut d, &manifest, &successes, LyricsTiming::Word);

        let lrc = d[0]
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Lrc)
            .unwrap();
        assert!(
            lrc.content
                .as_deref()
                .unwrap()
                .contains("[00:00.50]<00:00.50>hi <00:00.90>there")
        );
        assert_ne!(d[0].embedded_timed_lyrics_hash, "line-sylt");
        assert_eq!(pending[0].timing, Some(LyricsTiming::Word));
        assert_eq!(
            pending[0].timed_embed_hash,
            Some(d[0].embedded_timed_lyrics_hash.clone())
        );
    }

    #[test]
    fn disabling_lrc_carries_existing_timed_embed() {
        let mut d = vec![desired("a", "")];
        d[0].format = AudioFormat::Mp3;
        d[0].artifacts.clear();
        let mut manifest = Manifest::new();
        let mut e = entry(None, None);
        e.format = AudioFormat::Mp3;
        e.embedded_lyrics_hash = "plain".to_string();
        e.embedded_timed_lyrics_hash = "timed".to_string();
        manifest.insert("a", e);

        apply_synced_lrc(&mut d, &manifest, &HashMap::new());

        assert_eq!(d[0].embedded_timed_lyrics_hash, "timed");
    }

    #[test]
    fn apply_preserves_existing_embed_on_empty_regression() {
        let mut d = vec![desired("instr", "")];
        let mut manifest = Manifest::new();
        let mut e = entry(slot("instr.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = "H".to_string();
        manifest.insert("instr", e);
        let mut successes = HashMap::new();
        successes.insert("instr".to_string(), AlignedLyrics::default());
        let pending = apply_synced_lrc(&mut d, &manifest, &successes);

        let lrc = d[0]
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Lrc)
            .unwrap();
        assert_eq!(lrc.hash, "H");
        assert_eq!(lrc.content, None);
        assert_eq!(d[0].embedded_lyrics_hash, "H");
        assert!(pending.is_empty());
    }

    #[test]
    fn empty_alignment_does_not_complete_legacy_timing_migration() {
        let mut d = vec![desired("legacy", "inline lyrics")];
        d[0].format = AudioFormat::Mp3;
        let mut legacy = timed_check();
        legacy.timed_version = 0;
        legacy.timing = None;
        let mut entry = entry(slot("legacy.lrc", "old-timed-lrc"), Some(legacy));
        entry.format = AudioFormat::Mp3;
        entry.embedded_timed_lyrics_hash = "old-word-sylt".to_owned();
        let mut manifest = Manifest::new();
        manifest.insert("legacy", entry);
        let successes = HashMap::from([("legacy".to_owned(), AlignedLyrics::default())]);

        let pending =
            apply_synced_lrc_with_timing(&mut d, &manifest, &successes, LyricsTiming::Line);

        let lrc = d[0]
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Lrc)
            .unwrap();
        assert_eq!(lrc.hash, "old-timed-lrc");
        assert_eq!(lrc.content, None);
        assert_eq!(d[0].embedded_timed_lyrics_hash, "old-word-sylt");
        assert!(pending.is_empty());
        assert!(
            synced_lyrics_targets_with_timing(&d, &manifest, 2_000, LyricsTiming::Line)
                .contains("legacy")
        );
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
    fn failed_reformat_lookup_marks_existing_lyrics_unsafe_to_reencode() {
        let mut d = vec![desired("a", "")];
        d[0].format = AudioFormat::Mp3;
        d[0].artifacts.clear();
        let mut manifest = Manifest::new();
        let mut e = entry(None, None);
        e.embedded_lyrics_hash = "plain-hash".to_string();
        manifest.insert("a", e);

        apply_synced_lrc(&mut d, &manifest, &HashMap::new());

        assert!(!d[0].lyrics_reencode_safe);
        assert_eq!(d[0].embedded_lyrics_hash, "plain-hash");
    }

    #[test]
    fn failed_timed_reformat_lookup_is_unsafe_even_with_inline_plain_lyrics() {
        let mut d = vec![desired("a", "inline words")];
        d[0].format = AudioFormat::Wav;
        let mut manifest = Manifest::new();
        let mut e = entry(slot("a.lrc", "timed"), Some(timed_check()));
        e.format = AudioFormat::Mp3;
        e.embedded_timed_lyrics_hash = "timed".to_string();
        manifest.insert("a", e);

        apply_synced_lrc(&mut d, &manifest, &HashMap::new());

        assert!(!d[0].lyrics_reencode_safe);
    }

    #[test]
    fn successful_reformat_lookup_is_safe_and_refreshes_the_hash() {
        let mut d = vec![desired("a", "")];
        d[0].format = AudioFormat::Mp3;
        d[0].artifacts.clear();
        let mut manifest = Manifest::new();
        let mut e = entry(None, None);
        e.embedded_lyrics_hash = "old-hash".to_string();
        manifest.insert("a", e);
        let successes = HashMap::from([("a".to_string(), one_line_alignment())]);

        apply_synced_lrc(&mut d, &manifest, &successes);

        assert!(d[0].lyrics_reencode_safe);
        assert_eq!(d[0].embedded_lyrics_hash, content_hash("hi there"));
    }

    #[test]
    fn preview_marks_unknown_embed_but_settles_known_empty() {
        let mut d = vec![
            desired("stale", ""),
            desired("done", ""),
            desired("instrumental", ""),
        ];
        let mut manifest = Manifest::new();
        let mut stale = entry(slot("stale.lrc", "H"), Some(timed_check()));
        stale.embedded_lyrics_hash = String::new();
        manifest.insert("stale", stale);
        let mut done = entry(slot("done.lrc", "D"), Some(timed_check()));
        done.embedded_lyrics_hash = "D".to_string();
        manifest.insert("done", done);
        manifest.insert(
            "instrumental",
            entry(
                None,
                Some(SyncedLyricsCheck {
                    empty: true,
                    timed: false,
                    ..timed_check()
                }),
            ),
        );

        preview_synced_lrc(&mut d, &manifest, 2_000, true);
        assert!(!d[0].embedded_lyrics_hash.is_empty());
        assert_eq!(
            d[1].embedded_lyrics_hash, "D",
            "resolved clip carries forward"
        );
        assert_eq!(
            d[2].embedded_lyrics_hash, "",
            "a known empty result does not keep check permanently dirty"
        );
    }

    #[test]
    fn preview_marks_timed_embed_when_lrc_is_enabled_later() {
        let mut d = vec![desired("a", "inline words")];
        d[0].format = AudioFormat::Mp3;
        let mut manifest = Manifest::new();
        let mut e = entry(None, None);
        e.format = AudioFormat::Mp3;
        manifest.insert("a", e);

        preview_synced_lrc(&mut d, &manifest, 2_000, true);

        assert!(!d[0].embedded_timed_lyrics_hash.is_empty());
    }

    #[test]
    fn reformat_makes_migrated_clip_a_target() {
        // An already-migrated FLAC with a non-empty embed fingerprint is neither
        // a back-fill nor a rename target, but a pending MP3 reformat must
        // recreate its plain lyrics.
        let mut manifest = Manifest::new();
        let mut e = entry(slot("a.lrc", "H"), Some(timed_check()));
        e.embedded_lyrics_hash = "H".to_string(); // entry.format is FLAC (helper)
        manifest.insert("a", e);

        let mut reformat = vec![desired("a", "")];
        reformat[0].format = AudioFormat::Mp3;
        assert!(
            synced_lyrics_targets(&reformat, &manifest, 2_000).contains("a"),
            "a format change re-embeds a migrated clip"
        );

        // No format change: the same stable clip is not a target.
        let stable = vec![desired("a", "")]; // FLAC == entry.format
        assert!(
            synced_lyrics_targets(&stable, &manifest, 2_000).is_empty(),
            "no reformat, no back-fill -> no fetch"
        );

        // A clip with no persisted `.lrc` and no embedded fallback is still a
        // target because the plain audio metadata is missing.
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
            synced_lyrics_targets(&reformat_no_lrc, &no_lrc, 2_000).contains("a"),
            "missing plain lyrics are independent of `.lrc`"
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
                timing: None,
                timed_embed_hash: None,
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
                timing: None,
                timed_embed_hash: None,
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
            synced_lyrics_targets(&d, &Manifest::new(), 1_000).contains("a"),
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
            synced_lyrics_targets(&d, &Manifest::new(), 1_000).contains("a"),
            "fetched once"
        );

        // After the fetch resolved and the marker + `.lyrics.txt` slot landed:
        let mut manifest = Manifest::new();
        let mut e = entry(None, Some(timed_check()));
        e.lyrics_txt = Some(ArtifactState {
            path: "a.lyrics.txt".to_string(),
            hash: "body-hash".to_string(),
        });
        e.embedded_lyrics_hash = content_hash("hi there");
        manifest.insert("a", e);
        assert!(
            synced_lyrics_targets(&d, &manifest, 2_000).is_empty(),
            "a resolved lyrics-only clip converges (no forever re-fetch)"
        );

        // But a rename still moves it: the drifted path re-fetches.
        let mut renamed = d.clone();
        renamed[0].artifacts[0].path = "new/a.lyrics.txt".to_string();
        assert!(
            synced_lyrics_targets(&renamed, &manifest, 2_000).contains("a"),
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
        e.embedded_lyrics_hash = "H".to_string(); // the audio embed is complete
        // ...but there is no `.lyrics.txt` slot yet.
        manifest.insert("a", e);

        let d = vec![desired_both("a", "")];

        // 1) The clip IS a target despite the converged `.lrc`.
        assert!(
            synced_lyrics_targets(&d, &manifest, 2_000).contains("a"),
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
            synced_lyrics_targets(&d, &converged, 3_000).is_empty(),
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
            synced_lyrics_targets(std::slice::from_ref(&d0), &Manifest::new(), 1_000).contains("a"),
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
