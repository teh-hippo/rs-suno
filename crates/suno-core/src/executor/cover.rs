use super::*;

impl<H, F, G, C> Ctx<'_, H, F, G, C>
where
    H: Http,
    F: Filesystem,
    G: Ffmpeg,
    C: Clock,
{
    /// Lock the cover cache, panicking on poison (uniform access point, no repeated magic string).
    #[allow(clippy::expect_used)]
    pub(crate) fn cover_cache_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.cover_cache.lock().expect("cover cache mutex poisoned")
    }

    /// Download cover art, trying each candidate URL in order; `None` is fine.
    pub(crate) async fn fetch_cover(&self, clip: &Clip) -> Option<Vec<u8>> {
        for url in clip.cover_candidates() {
            if let Ok(response) = self.http.send(HttpRequest::get(url)).await
                && (200..=299).contains(&response.status)
                && !response.body.is_empty()
            {
                // A `CoverJpg` sidecar will fetch this exact URL this run; keep the
                // bytes so its write reuses them instead of fetching again (#89).
                // The lock guards only the insert, never the await above.
                if self.cover_wanted.contains(url) {
                    self.cover_cache_lock()
                        .insert(url.to_owned(), response.body.clone());
                }
                return Some(response.body);
            }
        }
        None
    }

    /// Resolve the cover to embed in `clip`'s audio for `format`.
    ///
    /// When animated covers are enabled, the container can embed WebP
    /// ([`AudioFormat::embeds_animated_cover`]), and the clip has a
    /// `video_cover_url`, this fetches that MP4 preview, transcodes it to a
    /// bounded animated WebP, and — if the result fits the FLAC picture budget —
    /// embeds it as `image/webp`. It falls back to the static JPEG (exactly what
    /// a coverless clip embeds today) when the feature is off, the clip has no
    /// preview, the container is ALAC, the encode overflows the budget, or the
    /// fetch/transcode fails for any non-systemic reason. A disk-full transcode
    /// aborts the run, like the audio transcode path.
    pub(crate) async fn resolve_cover(
        &self,
        clip: &Clip,
        format: AudioFormat,
    ) -> Result<Option<EmbedCover>, Fail> {
        if self.opts.embed_animated_cover
            && format.embeds_animated_cover()
            && !clip.video_cover_url.is_empty()
        {
            match self.animated_cover_webp(clip).await {
                Ok(webp) if webp.len() <= flac_picture_data_budget("image/webp") => {
                    return Ok(Some(EmbedCover {
                        bytes: webp,
                        mime: "image/webp",
                    }));
                }
                // Oversized encode: keep the file valid by embedding the static
                // JPEG instead (the intent hash is unchanged, so this does not
                // churn; a settings change that makes it fit re-embeds).
                Ok(_) => {}
                // A full scratch disk is systemic: abort like the audio path.
                Err(fail) if matches!(fail.class, Class::Disk) => return Err(fail),
                // Any other fetch/transcode failure is best-effort, exactly like a
                // failed static-cover fetch: fall back to the JPEG.
                Err(_) => {}
            }
        }
        Ok(self.fetch_cover(clip).await.map(|bytes| EmbedCover {
            bytes,
            mime: "image/jpeg",
        }))
    }

    /// Fetch the clip's MP4 preview and transcode it to an animated WebP.
    ///
    /// A disk-full transcode is classified [`Class::Disk`] so [`resolve_cover`]
    /// can abort the run; every other failure is per-clip and triggers the JPEG
    /// fallback.
    pub(crate) async fn animated_cover_webp(&self, clip: &Clip) -> Result<Vec<u8>, Fail> {
        let mp4 = self
            .fetch_bytes(&clip.video_cover_url)
            .await
            .map_err(|err| err.attribute(&clip.id))?;
        self.ffmpeg
            .mp4_to_webp(&mp4, self.opts.cover_webp)
            .await
            .map_err(|err| {
                disk_or_permanent(
                    &clip.id,
                    err.is_out_of_space(),
                    "disk full: no space left to transcode cover",
                    format!("cover transcode failed: {err}"),
                )
            })
    }
}
