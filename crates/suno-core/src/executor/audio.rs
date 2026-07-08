use super::*;

impl<H, F, G, C> Ctx<'_, H, F, G, C>
where
    H: Http,
    F: Filesystem,
    G: Ffmpeg,
    C: Clock,
{
    /// Render one audio action's tagged bytes, side-effect-free.
    ///
    /// This is the concurrent part: it fetches, transcodes, and tags the file
    /// (through shared ports, plus the client behind `client_lock`), then returns
    /// the bytes and where they must go. It deliberately writes nothing, removes
    /// nothing, and never touches `manifest`, so many run at once and an aborted
    /// run can drop them with no destination or manifest effect. The serial
    /// [`commit_audio`](Self::commit_audio) applies those effects in plan order.
    pub(crate) async fn prepare_audio(
        &self,
        client_lock: &ClientLock<'_, C>,
        action: &Action,
    ) -> Result<RenderedAudio, Fail> {
        match action {
            Action::Download {
                clip,
                lineage,
                path,
                format,
            } => {
                let bytes = self
                    .produce_audio(client_lock, clip, lineage, *format)
                    .await?;
                Ok(RenderedAudio {
                    clip_id: clip.id.clone(),
                    path: path.clone(),
                    format: *format,
                    from_path: None,
                    effect: Effect::Downloaded,
                    bytes,
                })
            }
            Action::Reformat {
                clip,
                path,
                from_path,
                from: _,
                to,
            } => {
                // A Reformat action carries no lineage, so recover it from the
                // desired set (the same context that drove naming and the hash),
                // falling back to a self-rooted context when the clip is not in
                // the current selection.
                let lineage = self
                    .by_id
                    .get(clip.id.as_str())
                    .map(|d| d.lineage.clone())
                    .unwrap_or_else(|| LineageContext::own_root(clip));
                let bytes = self.produce_audio(client_lock, clip, &lineage, *to).await?;
                Ok(RenderedAudio {
                    clip_id: clip.id.clone(),
                    path: path.clone(),
                    format: *to,
                    from_path: Some(from_path.clone()),
                    effect: Effect::Reformatted,
                    bytes,
                })
            }
            _ => unreachable!("prepare_audio only handles audio actions"),
        }
    }

    /// Commit one rendered audio result serially, in plan order.
    ///
    /// Writes the tagged bytes to the destination, then, for a [`Reformat`], drops
    /// the superseded file, then records the manifest entry. Ordering the write
    /// before the removal keeps a crash from losing both copies; keeping all of
    /// this off the concurrent phase preserves the sequential executor's plan-order
    /// guarantee for every destination and manifest effect.
    pub(crate) fn commit_audio(
        &self,
        manifest: &mut Manifest,
        rendered: RenderedAudio,
    ) -> Result<Effect, Fail> {
        let RenderedAudio {
            clip_id,
            path,
            format,
            from_path,
            effect,
            bytes,
        } = rendered;
        let size = self.write_verify(&clip_id, &path, &bytes)?;
        if let Some(from) = from_path {
            // The new file is safely in place; only now drop the old rendering.
            self.fs.remove(&from).map_err(|err| {
                if err.is_out_of_space() {
                    disk_fail(&clip_id, "disk full: no space left to remove old file")
                } else {
                    permanent_fail(&clip_id, format!("could not remove old file: {err}"))
                }
            })?;
        }
        manifest.insert(clip_id.clone(), self.entry(&clip_id, &path, format, size));
        Ok(effect)
    }

    /// Download (and transcode/tag) the audio for `clip` in `format`.
    pub(crate) async fn produce_audio(
        &self,
        client_lock: &ClientLock<'_, C>,
        clip: &Clip,
        lineage: &LineageContext,
        format: AudioFormat,
    ) -> Result<Vec<u8>, Fail> {
        let (meta, synced) = self.track_meta(clip, lineage);
        match format {
            AudioFormat::Mp3 => {
                let url = clip.mp3_url();
                let audio = self
                    .fetch_bytes(&url)
                    .await
                    .map_err(|err| err.attribute(&clip.id))?;
                let cover = self.resolve_cover(clip, format).await?;
                tag_mp3(
                    &audio,
                    &meta,
                    cover.as_ref().map(EmbedCover::as_cover),
                    synced,
                )
                .map_err(|err| permanent_fail(&clip.id, err.to_string()))
            }
            AudioFormat::Flac | AudioFormat::Alac => {
                let wav = self.fetch_wav(client_lock, clip).await?;
                let audio = self
                    .ffmpeg
                    .wav_to_lossless(&wav, format)
                    .await
                    .map_err(|err| {
                        if err.is_out_of_space() {
                            disk_fail(&clip.id, "disk full: no space left to transcode")
                        } else {
                            permanent_fail(&clip.id, format!("transcode failed: {err}"))
                        }
                    })?;
                let cover = self.resolve_cover(clip, format).await?;
                let cover = cover.as_ref().map(EmbedCover::as_cover);
                let tagged = match format {
                    AudioFormat::Alac => tag_alac(&audio, &meta, cover),
                    _ => tag_flac(&audio, &meta, cover),
                };
                tagged.map_err(|err| permanent_fail(&clip.id, err.to_string()))
            }
            AudioFormat::Wav => {
                let wav = self.fetch_wav(client_lock, clip).await?;
                let cover = self.resolve_cover(clip, format).await?;
                tag_wav(
                    &wav,
                    &meta,
                    cover.as_ref().map(EmbedCover::as_cover),
                    synced,
                )
                .map_err(|err| permanent_fail(&clip.id, err.to_string()))
            }
        }
    }

    /// Resolve the rendered WAV URL and download it.
    pub(crate) async fn fetch_wav(
        &self,
        client_lock: &ClientLock<'_, C>,
        clip: &Clip,
    ) -> Result<Vec<u8>, Fail> {
        let url = match self.resolve_wav_url(client_lock, &clip.id).await? {
            Some(url) => url,
            None => return Err(transient_fail(&clip.id, "WAV render was not ready")),
        };
        self.fetch_bytes(&url)
            .await
            .map_err(|err| err.attribute(&clip.id))
    }

    /// Read the WAV URL, requesting a render and polling if it is not ready.
    ///
    /// `None` means the render did not become ready within the poll budget; the
    /// caller treats that as a non-fatal transient failure, never a silent skip.
    ///
    /// Each client call briefly locks `client_lock`; the poll waits happen
    /// unlocked, so concurrent clips interleave their WAV renders rather than
    /// serialising behind one clip's whole poll budget.
    pub(crate) async fn resolve_wav_url(
        &self,
        client_lock: &ClientLock<'_, C>,
        id: &str,
    ) -> Result<Option<String>, Fail> {
        if let Some(url) = self.wav_url_retrying(client_lock, id).await? {
            return Ok(Some(url));
        }
        self.request_wav_retrying(client_lock, id).await?;
        for _ in 0..self.opts.wav_poll_attempts {
            self.clock.sleep(self.opts.wav_poll_interval).await;
            if let Some(url) = self.wav_url_retrying(client_lock, id).await? {
                return Ok(Some(url));
            }
        }
        Ok(None)
    }

    /// Read the rendered WAV URL, retrying transient API failures with backoff
    /// (SYNC-16/17), so the default FLAC path is as resilient as the CDN path.
    pub(crate) async fn wav_url_retrying(
        &self,
        client_lock: &ClientLock<'_, C>,
        id: &str,
    ) -> Result<Option<String>, Fail> {
        let mut attempt: u32 = 0;
        loop {
            let result = {
                let client = client_lock.lock().await;
                client.wav_url(self.http, id).await
            };
            match result {
                Ok(url) => return Ok(url),
                Err(err) => match self.retry_core(id, err, &mut attempt).await {
                    Some(fail) => return Err(fail),
                    None => continue,
                },
            }
        }
    }

    /// Ask Suno to render a WAV, retrying transient API failures with backoff.
    pub(crate) async fn request_wav_retrying(
        &self,
        client_lock: &ClientLock<'_, C>,
        id: &str,
    ) -> Result<(), Fail> {
        let mut attempt: u32 = 0;
        loop {
            let result = {
                let client = client_lock.lock().await;
                client.request_wav(self.http, id).await
            };
            match result {
                Ok(()) => return Ok(()),
                Err(err) => match self.retry_core(id, err, &mut attempt).await {
                    Some(fail) => return Err(fail),
                    None => continue,
                },
            }
        }
    }
}
