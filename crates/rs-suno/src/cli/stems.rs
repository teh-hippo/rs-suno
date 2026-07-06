//! Existing-stems enrichment of the desired set.
//!
//! Read-only helpers that page the stems listing to discover what stems already
//! exist locally, so reconciliation can keep them. Free of credit spend and
//! never a source of stem deletion.

use std::collections::HashMap;

use futures_util::stream::{self, StreamExt};
use suno_core::{Clip, Stem, SunoClient};

use crate::clock::TokioClock;
use crate::http::ReqwestHttp;

/// Strip the resolved format's extension from an audio path, giving the
/// extensionless base the sidecars and the `.stems` folder are built from.
/// Falls back to the whole path if the extension is somehow absent.
pub(crate) fn strip_format_ext(path: &str, format: suno_core::AudioFormat) -> &str {
    path.strip_suffix(&format!(".{}", format.ext()))
        .unwrap_or(path)
}

/// List existing stems for the selected clips, when the feature is on.
///
/// Read-only and free: it pages the stems listing (`GET`) and NEVER generates or
/// spends credits. Returns a map from clip id to its AUTHORITATIVE stem set. A
/// clip is present ONLY when its listing fully enumerated at least one stem; a
/// clip absent from the map (feature off, `has_stem` false, or an
/// indeterminate/failed/partial/`400` listing) means "keep existing local
/// stems", so this can never drive a stem deletion. `has_stem` is the
/// precondition, so a clip Suno reports as stemless is never even queried.
pub(crate) async fn list_existing_stems(
    enabled: bool,
    clips: &[&Clip],
    client: &SunoClient<TokioClock>,
    http: &ReqwestHttp,
    concurrency: u32,
) -> HashMap<String, Vec<Stem>> {
    let mut out = HashMap::new();
    if !enabled {
        return out;
    }
    let candidates: Vec<&Clip> = clips.iter().copied().filter(|clip| clip.has_stem).collect();
    let fetched = stream::iter(candidates)
        .map(|clip| async move {
            (
                clip.id.clone(),
                client.list_stems(http, &clip.id).await.ok(),
            )
        })
        .buffered(concurrency.max(1) as usize)
        .collect::<Vec<_>>()
        .await;
    for (id, result) in fetched {
        if let Some((stems, true)) = result
            && !stems.is_empty()
        {
            out.insert(id, stems);
        }
    }
    out
}
