use super::*;

impl<H, F, G, C> Ctx<'_, H, F, G, C>
where
    H: Http,
    F: Filesystem,
    G: Ffmpeg,
    C: Clock,
{
    /// Prepare one concurrent action side-effect-free, returning the bytes and
    /// routing metadata the serial committer needs. Only actions that pass
    /// [`is_prepareable`] reach here.
    pub(crate) async fn prepare(
        &self,
        client_lock: &ClientLock<'_, C>,
        action: &Action,
    ) -> Result<Prepared, Fail> {
        match action {
            Action::Download { .. } | Action::Reformat { .. } => self
                .prepare_audio(client_lock, action)
                .await
                .map(Prepared::Audio),
            Action::WriteArtifact {
                kind,
                path,
                source_url,
                hash,
                owner_id,
                content: None,
            } => {
                let bytes = self.artifact_bytes(*kind, source_url, owner_id).await?;
                Ok(Prepared::Artifact(PreparedArtifact {
                    kind: *kind,
                    path: path.clone(),
                    hash: hash.clone(),
                    owner_id: owner_id.clone(),
                    bytes,
                }))
            }
            Action::WriteStem {
                clip_id,
                key,
                stem_id,
                path,
                source_url,
                format,
                hash,
            } => {
                let bytes = self
                    .fetch_stem_bytes(client_lock, clip_id, stem_id, source_url, *format)
                    .await?;
                Ok(Prepared::Stem(PreparedStem {
                    clip_id: clip_id.clone(),
                    key: key.clone(),
                    path: path.clone(),
                    hash: hash.clone(),
                    bytes,
                }))
            }
            _ => unreachable!("prepare only handles prepareable actions"),
        }
    }

    /// Commit one prepared artifact result serially, in plan order.
    ///
    /// Writes the pre-fetched bytes, removes any stale copy left at the previously
    /// tracked path (when the audio moved), then records the slot on the manifest,
    /// album, or playlist store. All filesystem and state effects are identical to
    /// what the former serial [`write_artifact`] did; moving the slow fetch (and
    /// optional transcode) into [`prepare`] is the only change.
    ///
    /// A per-clip sidecar is skipped when its owning clip's audio is absent from
    /// the manifest: the audio failed or never existed this run, so the sidecar
    /// must not land without an owner (the preparation was speculative).
    pub(crate) fn commit_artifact(
        &self,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        playlists: &mut BTreeMap<String, PlaylistState>,
        prepared: PreparedArtifact,
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        let PreparedArtifact {
            kind,
            path,
            hash,
            owner_id,
            bytes,
        } = prepared;
        if is_per_clip_kind(kind) && manifest.get(&owner_id).is_none() {
            return Ok(Effect::Skipped);
        }
        let old_path = match kind {
            ArtifactKind::CoverJpg => manifest
                .get(&owner_id)
                .and_then(|e| e.cover_jpg.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::CoverWebp => manifest
                .get(&owner_id)
                .and_then(|e| e.cover_webp.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::DetailsTxt => manifest
                .get(&owner_id)
                .and_then(|e| e.details_txt.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::LyricsTxt => manifest
                .get(&owner_id)
                .and_then(|e| e.lyrics_txt.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::Lrc => manifest
                .get(&owner_id)
                .and_then(|e| e.lrc.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::VideoMp4 => manifest
                .get(&owner_id)
                .and_then(|e| e.video_mp4.as_ref())
                .map(|s| s.path.clone()),
            ArtifactKind::FolderJpg | ArtifactKind::FolderWebp | ArtifactKind::FolderMp4 => albums
                .get(&owner_id)
                .and_then(|a| a.artifact(kind))
                .map(|s| s.path.clone()),
            ArtifactKind::Playlist => None,
        };
        self.write_verify(&owner_id, &path, &bytes)?;
        if let Some(old) = old_path.as_deref()
            && !old.is_empty()
            && old != path
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
                    permanent_fail(
                        &owner_id,
                        format!("could not remove old sidecar {old}: {err}"),
                    )
                })?;
            }
        }
        if is_album_kind(kind) {
            set_album_artifact(
                albums,
                &owner_id,
                kind,
                Some(ArtifactState {
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                }),
            );
        } else if is_playlist_kind(kind) {
            set_playlist(
                playlists,
                &owner_id,
                Some(PlaylistState {
                    name: playlist_name_from_path(&path),
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                }),
            );
        } else if let Some(entry) = manifest.entries.get_mut(&owner_id) {
            set_manifest_artifact(
                entry,
                kind,
                Some(ArtifactState {
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                }),
            );
        }
        Ok(Effect::ArtifactWritten)
    }

    /// Relocate a fetched per-clip sidecar with a local rename, falling back to a
    /// fetch-and-write when the move is unsafe or the old file has vanished.
    ///
    /// Reconcile downgrades a pure path drift (same bytes, new path, old file
    /// present, fetched kind) to a `MoveArtifact`, so a retitle renames the file
    /// rather than re-downloading a cover or re-transcoding an animated WebP
    /// (#141). The in-place rename is taken only when `from` is this slot's alone
    /// to give up (no other tracked slot references it and no committed write has
    /// placed a file there); otherwise, or if the rename fails, fresh bytes are
    /// fetched and [`commit_artifact`](Self::commit_artifact) runs the gated
    /// old-path cleanup, so a swap or co-reference is handled exactly as before.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn move_artifact(
        &self,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        playlists: &mut BTreeMap<String, PlaylistState>,
        kind: ArtifactKind,
        from: &str,
        to: &str,
        source_url: &str,
        hash: &str,
        owner_id: &str,
        tracked_paths: &mut HashMap<String, u32>,
        committed: &BTreeSet<String>,
    ) -> Result<Effect, Fail> {
        // A per-clip sidecar needs its owning clip's audio present.
        if is_per_clip_kind(kind) && manifest.get(owner_id).is_none() {
            return Ok(Effect::Skipped);
        }
        // Relocate in place only when `from` is ours alone to give up: no other
        // tracked slot still references it (a prior failed swap can share a path)
        // and no committed write this run has already placed a file there.
        // Otherwise the fetch-and-write fallback copies fresh bytes and runs the
        // gated old-path cleanup.
        let exclusive =
            tracked_paths.get(from).is_none_or(|count| *count <= 1) && !committed.contains(from);
        if from != to && exclusive {
            match self.fs.rename(from, to) {
                Ok(()) => {
                    if let Some(count) = tracked_paths.get_mut(from) {
                        *count = count.saturating_sub(1);
                    }
                    if let Some(entry) = manifest.entries.get_mut(owner_id) {
                        set_manifest_artifact(
                            entry,
                            kind,
                            Some(ArtifactState {
                                path: to.to_owned(),
                                hash: hash.to_owned(),
                            }),
                        );
                    }
                    return Ok(Effect::Renamed);
                }
                Err(err) if err.is_out_of_space() => {
                    return Err(disk_fail(
                        owner_id,
                        "disk full: no space left to move sidecar",
                    ));
                }
                // The old file has vanished, or the rename is unsupported: fall
                // through to a fetch-and-write at `to`.
                Err(_) => {}
            }
        }
        let bytes = self.artifact_bytes(kind, source_url, owner_id).await?;
        self.commit_artifact(
            manifest,
            albums,
            playlists,
            PreparedArtifact {
                kind,
                path: to.to_owned(),
                hash: hash.to_owned(),
                owner_id: owner_id.to_owned(),
                bytes,
            },
            tracked_paths,
            committed,
        )
    }

    ///
    /// An animated cover — a per-clip [`CoverWebp`](ArtifactKind::CoverWebp) or an
    /// album [`FolderWebp`](ArtifactKind::FolderWebp) — fetches the clip's
    /// `video_cover` MP4 preview and transcodes it to an animated WebP through the
    /// ffmpeg port; every other kind is the fetched source verbatim (the static
    /// [`CoverJpg`](ArtifactKind::CoverJpg) / album [`FolderJpg`](ArtifactKind::FolderJpg)
    /// image, or the raw album [`FolderMp4`](ArtifactKind::FolderMp4) whose
    /// `video_cover_url` is kept untranscoded). A fetch or transcode failure
    /// is attributed to the owning clip and is a per-clip [`Fail`], except a
    /// disk-full transcode, which aborts the run like the audio FLAC path.
    pub(crate) async fn artifact_bytes(
        &self,
        kind: ArtifactKind,
        source_url: &str,
        owner_id: &str,
    ) -> Result<Vec<u8>, Fail> {
        // Reuse the cover the audio producer already fetched for the embedded tag
        // when it cached this exact URL (#89); otherwise fetch it now. The guard
        // is taken and dropped in its own statement so it never spans the await.
        let cached = self.cover_cache_lock().remove(source_url);
        let source = match cached {
            Some(bytes) => bytes,
            None => {
                let fetched = self
                    .fetch_bytes(source_url)
                    .await
                    .map_err(|err| err.attribute(owner_id))?;
                // Cache the raw source when a sibling folder artifact will fetch
                // the same URL (the `both` retention: cover.webp + cover.mp4), so
                // it is fetched exactly once. Bounded to shared URLs and drained
                // on the sibling's use.
                if self.shared_cover_urls.contains(source_url) {
                    self.cover_cache_lock()
                        .insert(source_url.to_owned(), fetched.clone());
                }
                fetched
            }
        };
        match kind {
            ArtifactKind::CoverWebp | ArtifactKind::FolderWebp => self
                .ffmpeg
                .mp4_to_webp(&source, self.opts.cover_webp)
                .await
                .map_err(|err| {
                    if err.is_out_of_space() {
                        disk_fail(owner_id, "disk full: no space left to transcode")
                    } else {
                        permanent_fail(owner_id, format!("cover transcode failed: {err}"))
                    }
                }),
            // The text sidecars are generated and always carry inline content, so
            // `write_artifact` never reaches this fetch path for them. Guard it so
            // a future miswiring fails loudly rather than fetching a URL.
            ArtifactKind::DetailsTxt | ArtifactKind::LyricsTxt | ArtifactKind::Lrc => Err(
                permanent_fail(owner_id, "text sidecar requires inline content"),
            ),
            ArtifactKind::CoverJpg
            | ArtifactKind::FolderJpg
            | ArtifactKind::FolderMp4
            | ArtifactKind::Playlist
            | ArtifactKind::VideoMp4 => Ok(source),
        }
    }

    /// Remove a sidecar file and clear its slot on the owning manifest entry.
    ///
    /// `remove` is idempotent, so an already-absent sidecar is not a failure.
    /// When the owning entry is already gone (its audio was deleted earlier this
    /// run, co-deleting the sidecar), there is no slot to clear and that is fine.
    ///
    /// Folder art is album-scoped: its slot is cleared on the album store keyed by
    /// the album's root id, not on a manifest clip.
    ///
    /// The audio `Delete` is applied before its sidecar `DeleteArtifact`. If the
    /// sidecar removal fails after the audio is already gone, the sidecar lingers
    /// untracked, but the design stays convergent rather than transactional: the
    /// next run re-plans the same removal and retries, and any directory it would
    /// have emptied is pruned once the file finally clears.
    pub(crate) fn delete_artifact(
        &self,
        manifest: &mut Manifest,
        albums: &mut BTreeMap<String, AlbumArt>,
        playlists: &mut BTreeMap<String, PlaylistState>,
        kind: ArtifactKind,
        path: &str,
        owner_id: &str,
    ) -> Result<Effect, Fail> {
        self.fs
            .remove(path)
            .map_err(|err| permanent_fail(owner_id, format!("artifact delete failed: {err}")))?;
        if is_album_kind(kind) {
            set_album_artifact(albums, owner_id, kind, None);
        } else if is_playlist_kind(kind) {
            set_playlist(playlists, owner_id, None);
        } else if let Some(entry) = manifest.entries.get_mut(owner_id) {
            set_manifest_artifact(entry, kind, None);
        }
        Ok(Effect::ArtifactDeleted)
    }
}
