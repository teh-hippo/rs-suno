use super::lifecycle::Relocate;
use super::*;

impl<H, F, G, C> Ctx<'_, H, F, G, C>
where
    H: Http,
    F: Filesystem,
    G: Ffmpeg,
    C: Clock,
{
    /// Commit one prepared stem result serially, in plan order.
    ///
    /// Writes the pre-fetched bytes (including any WAV render), removes any stale
    /// copy left at the previously tracked path, and records the stem slot.
    /// All filesystem and manifest effects are identical to what the former serial
    /// [`write_stem`] did; moving the slow fetch into [`prepare`] is the only change.
    ///
    /// Skipped when the owning clip's audio is absent from the manifest.
    pub(crate) fn commit_stem(
        &self,
        manifest: &mut Manifest,
        prepared: PreparedStem,
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        let PreparedStem {
            clip_id,
            key,
            path,
            hash,
            bytes,
        } = prepared;
        if manifest.get(&clip_id).is_none() {
            return Ok(Effect::Skipped);
        }
        let old_path = manifest
            .get(&clip_id)
            .and_then(|e| e.stems.get(&key))
            .map(|s| s.path.clone());
        self.write_verify(&clip_id, &path, &bytes)?;
        self.remove_superseded(
            &clip_id,
            old_path.as_deref(),
            &path,
            tracked_paths,
            committed,
            "stem",
        )?;
        if let Some(entry) = manifest.entries.get_mut(&clip_id) {
            set_manifest_stem(
                entry,
                &key,
                Some(ArtifactState {
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                }),
            );
        }
        Ok(Effect::ArtifactWritten)
    }

    /// Relocate a stem with a local rename, falling back to a fetch-and-write
    /// when the move is unsafe or the old file has vanished (#141).
    ///
    /// Reconcile downgrades a pure stem path drift to a `MoveStem`, so a retitle
    /// renames the raw stem rather than re-rendering a WAV through `convert_wav`
    /// or re-fetching an MP3. The in-place rename is taken only when `from` is
    /// this slot's alone to give up (no other tracked slot references it — two
    /// same-base clips can share a stem path after a partially-failed swap — and
    /// no committed write this run already holds it); otherwise the
    /// fetch-and-write fallback re-fetches the correct bytes at `to`, so a
    /// co-referenced shared stem is never renamed away with mismatched content.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn move_stem(
        &self,
        client_lock: &ClientLock<'_, C>,
        manifest: &mut Manifest,
        clip_id: &str,
        key: &str,
        stem_id: &str,
        from: &str,
        to: &str,
        source_url: &str,
        format: StemFormat,
        hash: &str,
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        if manifest.get(clip_id).is_none() {
            return Ok(Effect::Skipped);
        }
        // Try the in-place rename shared with `move_artifact`; on a fall-through
        // the fetch-and-write fallback re-fetches the correct bytes at `to`, so a
        // co-referenced shared stem is never renamed away with mismatched content.
        match self.try_relocate(from, to, tracked_paths, committed) {
            Relocate::Renamed => {
                if let Some(entry) = manifest.entries.get_mut(clip_id) {
                    set_manifest_stem(
                        entry,
                        key,
                        Some(ArtifactState {
                            path: to.to_owned(),
                            hash: hash.to_owned(),
                        }),
                    );
                }
                return Ok(Effect::Renamed);
            }
            Relocate::DiskFull => {
                return Err(disk_fail(clip_id, "disk full: no space left to move stem"));
            }
            Relocate::Refetch => {}
        }
        let bytes = self
            .fetch_stem_bytes(client_lock, clip_id, stem_id, source_url, format)
            .await?;
        self.commit_stem(
            manifest,
            PreparedStem {
                clip_id: clip_id.to_owned(),
                key: key.to_owned(),
                path: to.to_owned(),
                hash: hash.to_owned(),
                bytes,
            },
            tracked_paths,
            committed,
        )
    }

    /// Resolve a stem's RAW bytes in its native container, never transcoding.
    ///
    /// A `Wav` stem renders the stem clip's lossless WAV through the very same
    /// free `convert_wav` + poll flow the main FLAC/WAV audio uses
    /// ([`resolve_wav_url`](Self::resolve_wav_url)), keyed on the stem's own
    /// `stem_id`, then downloads that WAV. An `Mp3` stem (or a degenerate `Wav`
    /// stem with no id to render) downloads its public CDN url directly. Stems
    /// are the deliberate exception to the source format: the bytes are returned
    /// exactly as delivered and are never re-encoded to FLAC.
    pub(crate) async fn fetch_stem_bytes(
        &self,
        client_lock: &ClientLock<'_, C>,
        clip_id: &str,
        stem_id: &str,
        source_url: &str,
        format: StemFormat,
    ) -> Result<Vec<u8>, Fail> {
        let url = match format {
            StemFormat::Wav if !stem_id.is_empty() => {
                match self.resolve_wav_url(client_lock, stem_id).await? {
                    Some(url) => url,
                    None => return Err(transient_fail(clip_id, "stem WAV render was not ready")),
                }
            }
            // Mp3, or a Wav stem with no id to render, downloads the CDN mp3.
            _ => source_url.to_owned(),
        };
        self.fetch_bytes(&url)
            .await
            .map_err(|err| err.attribute(clip_id))
    }

    /// Remove one stem file and clear its slot in the owning clip's stem map.
    ///
    /// `remove` is idempotent, so an already-absent stem is not a failure. When
    /// the owning entry is already gone (its audio was deleted earlier this run,
    /// co-deleting the stem), there is no slot to clear and that is fine; the
    /// emptied `.stems` folder is pruned by the end-of-run directory sweep.
    pub(crate) fn delete_stem(
        &self,
        manifest: &mut Manifest,
        clip_id: &str,
        key: &str,
        path: &str,
    ) -> Result<Effect, Fail> {
        self.fs.remove(path).map_err(|err| {
            disk_or_permanent(
                clip_id,
                err.is_out_of_space(),
                "disk full: no space left to remove stem",
                format!("stem delete failed: {err}"),
            )
        })?;
        if let Some(entry) = manifest.entries.get_mut(clip_id) {
            set_manifest_stem(entry, key, None);
        }
        Ok(Effect::ArtifactDeleted)
    }
}
