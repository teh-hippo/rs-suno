use super::*;
use std::sync::atomic::Ordering;

const CACHED_ENTITLEMENT_REASON: &str = "lossless entitlement unavailable for this run";

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
                let result = if format.requires_wav_render()
                    && self.lossless_unavailable.load(Ordering::Acquire)
                {
                    Err(entitlement_fail(&clip.id, CACHED_ENTITLEMENT_REASON))
                } else {
                    self.produce_audio(client_lock, clip, lineage, *format)
                        .await
                };
                let (bytes, actual_path, actual_format, fallback) = match result {
                    Ok(bytes) => (Some(bytes), path.clone(), *format, None),
                    Err(fail) if matches!(fail.class, Class::Entitlement) => {
                        self.lossless_unavailable.store(true, Ordering::Release);
                        let actual_path = audio_path_with_format(path, *format, AudioFormat::Mp3);
                        let bytes = self
                            .produce_audio(client_lock, clip, lineage, AudioFormat::Mp3)
                            .await?;
                        let fallback = AudioFallback {
                            clip_id: clip.id.clone(),
                            requested_path: path.clone(),
                            actual_path: actual_path.clone(),
                            format: AudioFormat::Mp3,
                            reason: fail.reason,
                        };
                        (Some(bytes), actual_path, AudioFormat::Mp3, Some(fallback))
                    }
                    Err(fail) => return Err(fail),
                };
                Ok(RenderedAudio {
                    clip_id: clip.id.clone(),
                    path: actual_path,
                    format: actual_format,
                    from_path: None,
                    effect: AudioEffect::Downloaded,
                    bytes,
                    fallback,
                })
            }
            Action::Reformat {
                clip,
                path,
                from_path,
                from,
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
                let result = if to.requires_wav_render()
                    && self.lossless_unavailable.load(Ordering::Acquire)
                {
                    Err(entitlement_fail(&clip.id, CACHED_ENTITLEMENT_REASON))
                } else {
                    self.produce_audio(client_lock, clip, &lineage, *to).await
                };
                let (bytes, actual_path, actual_format, cleanup, effect, fallback) = match result {
                    Ok(bytes) => (
                        Some(bytes),
                        path.clone(),
                        *to,
                        Some(from_path.clone()),
                        AudioEffect::Reformatted,
                        None,
                    ),
                    Err(fail)
                        if matches!(fail.class, Class::Entitlement)
                            && *from == AudioFormat::Mp3 =>
                    {
                        self.lossless_unavailable.store(true, Ordering::Release);
                        let actual_path = audio_path_with_format(path, *to, AudioFormat::Mp3);
                        let bytes = self
                            .produce_audio(client_lock, clip, &lineage, AudioFormat::Mp3)
                            .await?;
                        let cleanup =
                            (!same_fs_path(&actual_path, from_path)).then(|| from_path.clone());
                        let fallback = AudioFallback {
                            clip_id: clip.id.clone(),
                            requested_path: path.clone(),
                            actual_path: actual_path.clone(),
                            format: AudioFormat::Mp3,
                            reason: fail.reason,
                        };
                        (
                            Some(bytes),
                            actual_path,
                            AudioFormat::Mp3,
                            cleanup,
                            AudioEffect::Reformatted,
                            Some(fallback),
                        )
                    }
                    Err(fail) if matches!(fail.class, Class::Entitlement) => {
                        self.lossless_unavailable.store(true, Ordering::Release);
                        let fallback = AudioFallback {
                            clip_id: clip.id.clone(),
                            requested_path: path.clone(),
                            actual_path: from_path.clone(),
                            format: *from,
                            reason: fail.reason,
                        };
                        (
                            None,
                            from_path.clone(),
                            *from,
                            None,
                            AudioEffect::Skipped,
                            Some(fallback),
                        )
                    }
                    Err(fail) => return Err(fail),
                };
                Ok(RenderedAudio {
                    clip_id: clip.id.clone(),
                    path: actual_path,
                    format: actual_format,
                    from_path: cleanup,
                    effect,
                    bytes,
                    fallback,
                })
            }
            // prepare_audio() is only ever dispatched for audio actions.
            #[allow(clippy::unreachable)]
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
            fallback,
        } = rendered;
        let wrote = if let Some(bytes) = bytes {
            let size = self.write_verify(&clip_id, &path, &bytes)?;
            let observed = self.observe_committed_audio(&clip_id, &path, format)?;
            if let Some(from) = from_path {
                // The new file is safely in place; only now drop the old rendering.
                self.fs.remove(&from).map_err(|err| {
                    disk_or_permanent(
                        &clip_id,
                        err.is_out_of_space(),
                        "disk full: no space left to remove old file",
                        format!("could not remove old file: {err}"),
                    )
                })?;
            }
            let mut entry = self.entry(&clip_id, &path, format, size);
            if let Some(desired) = self.by_id.get(clip_id.as_str()).copied() {
                self.refresh_entry_from_observation(&mut entry, desired, &observed);
            }
            manifest.insert(clip_id.clone(), entry);
            true
        } else {
            false
        };
        if let Some(fallback) = fallback {
            Ok(Effect::AudioFallback {
                effect,
                fallback,
                wrote,
            })
        } else {
            Ok(match effect {
                AudioEffect::Downloaded => Effect::Downloaded,
                AudioEffect::Reformatted => Effect::Reformatted,
                AudioEffect::Skipped => Effect::Skipped,
            })
        }
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
        let timed = self.opts.embed_synced_lyrics.then_some(synced).flatten();
        match format {
            AudioFormat::Mp3 => {
                let url = clip.mp3_url();
                let audio = self
                    .fetch_bytes(&url)
                    .await
                    .map_err(|err| err.attribute(&clip.id))?;
                let cover = self.resolve_cover(clip, format).await?;
                tag_mp3_with_timing(
                    &audio,
                    &meta,
                    cover.as_ref().map(EmbedCover::as_cover),
                    timed,
                    self.opts.lyrics_timing,
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
                        disk_or_permanent(
                            &clip.id,
                            err.is_out_of_space(),
                            "disk full: no space left to transcode",
                            format!("transcode failed: {err}"),
                        )
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
                tag_wav_with_timing(
                    &wav,
                    &meta,
                    cover.as_ref().map(EmbedCover::as_cover),
                    timed,
                    self.opts.lyrics_timing,
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
        self.retry_client(id, async || {
            let client = client_lock.lock().await;
            client.wav_url(self.http, id).await
        })
        .await
    }

    /// Ask Suno to render a WAV, retrying transient API failures with backoff.
    pub(crate) async fn request_wav_retrying(
        &self,
        client_lock: &ClientLock<'_, C>,
        id: &str,
    ) -> Result<(), Fail> {
        self.retry_client(id, async || {
            let client = client_lock.lock().await;
            client.request_wav(self.http, id).await
        })
        .await
    }
}

fn audio_path_with_format(path: &str, from: AudioFormat, to: AudioFormat) -> String {
    let suffix = format!(".{}", from.ext());
    let base = path.strip_suffix(&suffix).unwrap_or(path);
    format!("{base}.{}", to.ext())
}
