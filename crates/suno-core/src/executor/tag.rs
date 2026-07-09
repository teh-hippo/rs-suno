use super::*;

impl<H, F, G, C> Ctx<'_, H, F, G, C>
where
    H: Http,
    F: Filesystem,
    G: Ffmpeg,
    C: Clock,
{
    /// Re-tag the existing file in place to match current metadata and art.
    pub(crate) async fn retag(
        &self,
        manifest: &mut Manifest,
        clip: &Clip,
        lineage: &LineageContext,
        path: &str,
    ) -> Result<Effect, Fail> {
        let Some(format) = manifest.get(&clip.id).map(|entry| entry.format) else {
            return Err(permanent_fail(
                &clip.id,
                "retag target missing from manifest",
            ));
        };

        if format == AudioFormat::Wav {
            let (meta, synced) = self.track_meta(clip, lineage);
            let cover = self.resolve_cover(clip, format).await?;
            let existing = self.fs.read(path).map_err(|err| {
                permanent_fail(&clip.id, format!("could not read for retag: {err}"))
            })?;
            let tagged = tag_wav(
                &existing,
                &meta,
                cover.as_ref().map(EmbedCover::as_cover),
                synced,
            )
            .map_err(|err| permanent_fail(&clip.id, err.to_string()))?;
            let size = self.write_verify(&clip.id, path, &tagged)?;
            self.refresh_hashes(manifest, &clip.id, Some(size));
            return Ok(Effect::Retagged);
        }

        let (meta, synced) = self.track_meta(clip, lineage);
        let cover = self.resolve_cover(clip, format).await?;
        let cover = cover.as_ref().map(EmbedCover::as_cover);
        let existing = self
            .fs
            .read(path)
            .map_err(|err| permanent_fail(&clip.id, format!("could not read for retag: {err}")))?;
        let tagged = match format {
            AudioFormat::Mp3 => tag_mp3(&existing, &meta, cover, synced),
            AudioFormat::Flac => tag_flac(&existing, &meta, cover),
            AudioFormat::Alac => tag_alac(&existing, &meta, cover),
            AudioFormat::Wav => unreachable!("WAV handled above"),
        }
        .map_err(|err| permanent_fail(&clip.id, err.to_string()))?;
        let size = self.write_verify(&clip.id, path, &tagged)?;
        self.refresh_hashes(manifest, &clip.id, Some(size));
        Ok(Effect::Retagged)
    }

    /// The track metadata for a clip, paired with its synced lyrics (if any).
    ///
    /// The feed omits per-clip lyrics, so when this run fetched aligned lyrics
    /// for the clip the plain text is folded into `lyrics` here, which the MP3
    /// `USLT` and FLAC `LYRICS` tags then carry. The returned [`AlignedLyrics`]
    /// is passed on to [`tag_mp3`] for the word-level `SYLT` frame.
    pub(crate) fn track_meta<'m>(
        &'m self,
        clip: &Clip,
        lineage: &LineageContext,
    ) -> (TrackMetadata, Option<&'m AlignedLyrics>) {
        let synced = self.synced_for(&clip.id);
        let mut meta = TrackMetadata::from_clip(clip, lineage);
        if let Some(aligned) = synced {
            meta.lyrics = aligned.plain_text();
        }
        (meta, synced)
    }

    /// This run's non-empty aligned lyrics for a clip, if any were fetched.
    pub(crate) fn synced_for(&self, clip_id: &str) -> Option<&AlignedLyrics> {
        self.synced
            .get(clip_id)
            .filter(|aligned| !aligned.is_empty())
    }

    /// Refresh an existing entry's hashes, protection, and (optionally) size.
    pub(crate) fn refresh_hashes(&self, manifest: &mut Manifest, clip_id: &str, size: Option<u64>) {
        let desired = self.by_id.get(clip_id).copied();
        if let Some(entry) = manifest.entries.get_mut(clip_id) {
            if let Some(d) = desired {
                entry.meta_hash = d.meta_hash.clone();
                entry.art_hash = d.art_hash.clone();
                entry.embedded_lyrics_hash = d.embedded_lyrics_hash.clone();
                entry.preserve = preserve_for(d);
            }
            if let Some(size) = size {
                entry.size = size;
            }
        }
    }

    /// Refresh only an entry's preserve marker from the current desired state.
    ///
    /// A clip can gain or lose copy/private protection with no file change, which
    /// reconcile emits as a [`Skip`](Action::Skip). Refreshing here keeps the
    /// persisted marker a faithful image of live protection, so the cross-run
    /// delete guard (SYNC-8) never reads it stale.
    pub(crate) fn refresh_preserve(&self, manifest: &mut Manifest, clip_id: &str) {
        if let Some(d) = self.by_id.get(clip_id).copied()
            && let Some(entry) = manifest.entries.get_mut(clip_id)
        {
            entry.preserve = preserve_for(d);
        }
    }
}
