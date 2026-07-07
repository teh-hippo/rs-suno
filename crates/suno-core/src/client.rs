//! The Suno API client: lists the library behind the [`Http`](crate::Http) port.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::Instant;

use futures_util::stream::{self, StreamExt};

use crate::auth::ClerkAuth;
use crate::backoff::{backoff_delay, retry_after};
use crate::clock::Clock;
use crate::consts::{
    API_MAX_RETRIES, BILLING_INFO_PATH, CLIP_PARENT_PATH, FEED_INITIAL_RATE, FEED_V3_PATH,
    GET_SONGS_BY_IDS_PATH, GET_SONGS_CHUNK, MAX_PAGES, PLAYLIST_ME_PATH, PLAYLIST_PATH,
    SUNO_API_BASE_URL,
};
use crate::error::{Error, Result};
use crate::http::{Http, HttpRequest, Method};
use crate::limiter::{AdaptiveLimiter, retry_after_delay};
use crate::lyrics::AlignedLyrics;
use crate::model::{BillingInfo, Clip, Playlist, Stem};
use crate::wire::{
    feed_v3_body, parse_billing_info, parse_clip, parse_feed_v3, parse_playlist_clips,
    parse_playlists, parse_songs_batch, parse_stem_page_count, parse_stems_page, parse_wav_url,
};

/// A client for the Suno library API, owning the account's [`ClerkAuth`].
///
/// The [`Clock`] is held so [`api_request`](Self::api_request) can back off
/// through the port on a `429` or transient failure — the engine still sleeps
/// nowhere itself. The [`AdaptiveLimiter`] paces reactively: an unthrottled
/// listing waits nowhere, and only after a `429` does it space requests out,
/// halving the rate and ramping it back after a run of clean successes so pacing
/// tracks Suno's real limit rather than a fixed constant.
pub struct SunoClient<C> {
    auth: ClerkAuth,
    clock: C,
    limiter: Mutex<AdaptiveLimiter>,
}

impl<C: Clock> SunoClient<C> {
    /// Create a client from a fresh or already-authenticated [`ClerkAuth`].
    pub fn new(auth: ClerkAuth, clock: C) -> Self {
        Self {
            auth,
            clock,
            limiter: Mutex::new(AdaptiveLimiter::new(FEED_INITIAL_RATE)),
        }
    }

    /// Borrow the underlying authenticator.
    pub fn auth(&self) -> &ClerkAuth {
        &self.auth
    }

    /// The adaptive limiter's current requests-per-second rate, for tests that
    /// assert the limiter still records success and `429` correctly (including
    /// under concurrent WAV-render calls serialised through the executor).
    #[cfg(test)]
    pub(crate) fn limiter_rate(&self) -> f64 {
        self.limiter.lock().unwrap().rate()
    }

    /// List clips across the whole library, or only liked clips.
    ///
    /// Walks the cursor-paginated `POST /api/feed/v3` feed, following
    /// `next_cursor` until the server reports the end. Once `limit` clips have
    /// been collected it stops at the next page boundary and truncates to
    /// `limit`. Paging is hard-capped at [`MAX_PAGES`] so a runaway
    /// `has_more` can never loop forever. When `liked` is set the feed filter
    /// scopes to liked clips (`liked: "True"`).
    ///
    /// Returns the clips paired with a `complete` flag that is `true` only when
    /// paging ended because the server reported `has_more == false` (the feed
    /// fully drained). A missing `has_more`, a `has_more == true` page with no
    /// usable `next_cursor`, a `limit` stop, exhausting [`MAX_PAGES`], or any
    /// transport error all yield `false` (or propagate) so the caller can refuse
    /// to treat a truncated listing as authoritative for deletion.
    ///
    /// A third `any_filtered` flag is `true` when any listed clip was dropped by
    /// the downloadable filter on any page, so the caller can refuse deletion
    /// authority for a listing that may have hidden a manifest-tracked clip
    /// (#248), exactly as the playlist path already does.
    pub async fn list_clips(
        &self,
        http: &impl Http,
        liked: bool,
        limit: Option<usize>,
    ) -> Result<(Vec<Clip>, bool, bool)> {
        let mut clips = Vec::new();
        let mut cursor: Option<String> = None;
        let mut complete = false;
        let mut any_filtered = false;
        for _ in 0..MAX_PAGES {
            let body = feed_v3_body(liked, cursor.as_deref());
            let response = self
                .api_send_retrying(http, Method::Post, FEED_V3_PATH, body)
                .await?;
            let page = parse_feed_v3(&response)?;
            clips.extend(page.clips);
            any_filtered |= page.any_filtered;
            match page.has_more {
                Some(false) => {
                    complete = true;
                    break;
                }
                Some(true) => match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                },
                None => break,
            }
            if limit.is_some_and(|n| clips.len() >= n) {
                break;
            }
        }
        if let Some(n) = limit {
            clips.truncate(n);
        }
        Ok((clips, complete, any_filtered))
    }

    /// Fetch one clip by ID.
    ///
    /// Tries the dedicated `/api/clip/{id}` endpoint first, then falls back to
    /// scanning the library feed if that endpoint yields no matching clip.
    pub async fn get_clip(&self, http: &impl Http, id: &str) -> Result<Clip> {
        if let Some(clip) = self.try_get_clip(http, id).await? {
            return Ok(clip);
        }
        self.find_in_feed(http, id).await
    }

    /// Ask Suno to render a clip to lossless WAV (server-side, asynchronous).
    pub async fn request_wav(&self, http: &impl Http, id: &str) -> Result<()> {
        let path = format!("/api/gen/{id}/convert_wav/");
        self.api_request(http, Method::Post, &path, Vec::new())
            .await?;
        Ok(())
    }

    /// Read the rendered WAV URL for a clip, or `None` while it is not ready.
    ///
    /// A `404` maps to `None` (the render is absent, not yet requested, or the
    /// endpoint has moved), symmetric with [`aligned_lyrics`](Self::aligned_lyrics)
    /// so an unrendered clip is "no WAV yet" rather than a run-aborting error.
    /// Like [`request_wav`](Self::request_wav) it skips the shared retry: the
    /// caller's poll loop owns that budget.
    pub async fn wav_url(&self, http: &impl Http, id: &str) -> Result<Option<String>> {
        let path = format!("/api/gen/{id}/wav_file/");
        let body = match self.api_get(http, &path).await {
            Ok(body) => body,
            Err(Error::NotFound(_)) => return Ok(None),
            Err(err) => return Err(err),
        };
        parse_wav_url(&body)
    }

    /// Fetch a clip's word- and line-level aligned (synced) lyrics.
    ///
    /// `GET /api/gen/{id}/aligned_lyrics/v2/` (the trailing slash is required) on
    /// the studio-api host, authenticated with the same JWT as every other
    /// library read. The `v2` shape carries both a flat word-level list and a
    /// line-level list with section labels and nested per-word timing (see
    /// [`AlignedLyrics`]).
    ///
    /// An instrumental or un-alignable clip returns `200` with empty arrays,
    /// which maps to an empty [`AlignedLyrics`]; a `404` (no alignment for the
    /// clip) is treated the same way, so an absent endpoint is "no synced
    /// lyrics" rather than a run failure — the caller then writes no synced
    /// artefact, exactly as an empty cover URL writes no cover. Rides the
    /// adaptive rate limiter like the other reads.
    pub async fn aligned_lyrics(&self, http: &impl Http, id: &str) -> Result<AlignedLyrics> {
        let path = format!("/api/gen/{id}/aligned_lyrics/v2/");
        match self.api_get_retrying(http, &path).await {
            Ok(body) => Ok(AlignedLyrics::from_bytes(&body)),
            Err(Error::NotFound(_)) => Ok(AlignedLyrics::default()),
            Err(err) => Err(err),
        }
    }

    /// Fetch specific clips by id, batch-first with a per-id fallback.
    ///
    /// Used by lineage resolution to gap-fill ancestors that are absent from a
    /// normal listing, including trashed ones. Ids are fetched in a single
    /// batch via [`get_songs_by_ids`](Self::get_songs_by_ids)
    /// (`GET /api/clips/get_songs_by_ids`), which cuts the round-trips and `429`s
    /// of one request per id. Any ids the batch does not return (individually
    /// trashed or absent, exactly as a `/api/clip/{id}` `404` today, or in a
    /// chunk the batch endpoint could not serve) then fall back to one
    /// `GET /api/clip/{id}` each, with bounded `concurrency`, attempted exactly
    /// once, and a `404` there is skipped so the caller can fall back to the
    /// parent endpoint. A `429` while batching propagates rather than fanning
    /// out into per-id requests.
    ///
    /// Unlike [`list_clips`](Self::list_clips), no downloadability filter is
    /// applied: an ancestor may itself be an infill or context-window artefact
    /// that the lineage walk must still traverse. Clips returned here are
    /// ancestors for resolution only and must never be treated as download
    /// candidates. Ids are deduplicated in order and the result preserves that
    /// de-duplicated input order, matched by id (never by response position).
    /// The signature is unchanged so [`gap_fill`](crate::lineage) is unaffected.
    pub async fn get_clips_by_ids(
        &self,
        http: &impl Http,
        ids: &[&str],
        concurrency: usize,
    ) -> Result<Vec<Clip>> {
        let ordered = dedup_nonempty(ids);
        let mut found: HashMap<&str, Clip> = self
            .get_songs_by_ids(http, &ordered)
            .await?
            .into_iter()
            .filter_map(|clip| {
                ordered
                    .iter()
                    .find(|id| **id == clip.id)
                    .map(|id| (*id, clip))
            })
            .collect();
        let omitted: Vec<&str> = ordered
            .iter()
            .copied()
            .filter(|id| !found.contains_key(id))
            .collect();
        if !omitted.is_empty() {
            for clip in self
                .fetch_clips_individually(http, &omitted, concurrency)
                .await?
            {
                if let Some(id) = ordered.iter().copied().find(|id| *id == clip.id) {
                    found.insert(id, clip);
                }
            }
        }
        Ok(ordered.iter().filter_map(|id| found.remove(id)).collect())
    }

    /// Batch-fetch clips by id via `GET /api/clips/get_songs_by_ids?ids=…&ids=…`.
    ///
    /// This is the pure batch primitive: the deduplicated ids are split into
    /// chunks of [`GET_SONGS_CHUNK`], each requested with repeated `ids=` params,
    /// and the `{"clips":[…]}` body is parsed defensively and matched back to the
    /// requested ids by id, so the result preserves the de-duplicated input order
    /// regardless of the server's ordering and drops any clip that was not asked
    /// for. Ids the batch does not return (trashed, absent, or in a chunk the
    /// endpoint could not serve) are simply left out; filling them is the
    /// caller's job (see [`get_clips_by_ids`](Self::get_clips_by_ids)).
    ///
    /// The batch endpoint is undocumented and may be unavailable. A chunk that
    /// the endpoint cannot serve (a `404`, a `400`, a `5xx`, a transport failure,
    /// or a body that is not `{"clips":[…]}`) yields nothing for that chunk
    /// rather than erroring, so an outage or reshape degrades rather than breaks
    /// (the decoupling rule) and the caller's per-id fallback recovers those ids
    /// exactly once. A `429`, by contrast, rides the retry inside
    /// [`api_get_retrying`](Self::api_get_retrying) and, once exhausted,
    /// propagates rather than letting a burst of per-id requests deepen the
    /// throttling; an auth failure likewise propagates rather than being masked.
    pub async fn get_songs_by_ids(&self, http: &impl Http, ids: &[&str]) -> Result<Vec<Clip>> {
        let ordered = dedup_nonempty(ids);
        let mut found: HashMap<&str, Clip> = HashMap::new();
        for chunk in ordered.chunks(GET_SONGS_CHUNK) {
            let query = chunk
                .iter()
                .map(|id| format!("ids={id}"))
                .collect::<Vec<_>>()
                .join("&");
            let path = format!("{GET_SONGS_BY_IDS_PATH}?{query}");
            let clips = match self.api_get_retrying(http, &path).await {
                Ok(body) => parse_songs_batch(&body).unwrap_or_default(),
                Err(err @ (Error::RateLimited { .. } | Error::Auth(_))) => return Err(err),
                Err(_) => Vec::new(),
            };
            for clip in clips {
                if let Some(id) = chunk.iter().copied().find(|id| *id == clip.id) {
                    found.insert(id, clip);
                }
            }
        }
        Ok(ordered.iter().filter_map(|id| found.remove(id)).collect())
    }

    /// Fetch clips one `GET /api/clip/{id}` per id, with bounded concurrency.
    ///
    /// The per-id fallback used by [`get_clips_by_ids`](Self::get_clips_by_ids)
    /// for any ids the batch did not return, whether individually omitted or in a
    /// whole chunk the batch endpoint could not serve. `/api/clip/{id}` returns
    /// any clip, trashed or artefact, with the full field set and no
    /// downloadability filter. An id that `404`s is skipped; the input order is
    /// preserved.
    async fn fetch_clips_individually(
        &self,
        http: &impl Http,
        ids: &[&str],
        concurrency: usize,
    ) -> Result<Vec<Clip>> {
        let limit = concurrency.max(1);
        let fetched = stream::iter(ids.iter().copied())
            .map(|id| async move {
                let path = format!("/api/clip/{id}");
                match self.api_get_retrying(http, &path).await {
                    Ok(body) => Ok(parse_clip(&body)),
                    Err(Error::NotFound(_)) => Ok(None),
                    Err(err) => Err(err),
                }
            })
            .buffered(limit)
            .collect::<Vec<_>>()
            .await;
        let mut clips = Vec::new();
        for item in fetched {
            if let Some(clip) = item? {
                clips.push(clip);
            }
        }
        Ok(clips)
    }

    /// Fetch a clip's immediate parent via the dedicated parent endpoint.
    ///
    /// Returns the parent clip, or `None` when the clip is a root. A root's
    /// parent is reported as HTTP `200` with a bodiless clip that carries no
    /// `id` (e.g. `{"is_public": false}`), not a `404`: [`parse_clip`] requires
    /// a non-empty id, so that root shape maps to `Ok(None)` here. The `404`
    /// arm is kept as a belt-and-braces fallback for the alternative "no parent"
    /// encoding. Any other failure, including a transient `5xx`, propagates as
    /// an error rather than being mistaken for a root.
    pub async fn get_clip_parent(&self, http: &impl Http, id: &str) -> Result<Option<Clip>> {
        let path = format!("{CLIP_PARENT_PATH}?clip_id={id}");
        match self.api_get_retrying(http, &path).await {
            // A root replies 200 with no id; parse_clip gates on a non-empty id
            // and yields None, so a root never looks like a fetched parent.
            Ok(body) => Ok(parse_clip(&body)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// List the account's own playlists, paging `/api/playlist/me`.
    ///
    /// Trashed and share-list playlists are excluded by query, so the result is
    /// the account's authoritative own set. Paging stops on the first empty page
    /// and is hard-capped at [`MAX_PAGES`] so a server that ignores the page
    /// parameter cannot loop forever. Only entries with a non-empty id are kept,
    /// and accumulated entries are de-duplicated by id so a server that ignores
    /// the page parameter and repeats a body cannot inflate the set.
    ///
    /// A hard failure propagates as an error; the caller treats that as "the
    /// playlist listing did not fully enumerate" and refuses every playlist
    /// deletion this run, so a dropped fetch can never remove a `.m3u8`.
    pub async fn get_playlists(&self, http: &impl Http) -> Result<Vec<Playlist>> {
        let mut playlists = Vec::new();
        let mut seen = BTreeSet::new();
        for page in 1..=MAX_PAGES {
            let path =
                format!("{PLAYLIST_ME_PATH}?page={page}&show_trashed=false&show_sharelist=false");
            let body = self.api_get_retrying(http, &path).await?;
            let page_playlists = parse_playlists(&body)?;
            if page_playlists.is_empty() {
                break;
            }
            for playlist in page_playlists {
                if seen.insert(playlist.id.clone()) {
                    playlists.push(playlist);
                }
            }
        }
        Ok(playlists)
    }

    /// Fetch one playlist's clips in Suno order via `/api/playlist/{id}/`.
    ///
    /// The response's `playlist_clips[]` is already ordered and trashed members
    /// are excluded by Suno, so the order is preserved exactly and no
    /// downloadability filter is applied: a playlist may legitimately contain any
    /// clip. Each entry's `clip` object is mapped (falling back to the entry
    /// itself), and only clips with a non-empty id are kept.
    ///
    /// The returned `bool` is a completeness signal for deletion authority: the
    /// endpoint reports `num_total_results` (the playlist's full member count)
    /// alongside `playlist_clips[]`, so `true` means every member came back on
    /// this single page intact (`num_total_results` present, equal to the raw
    /// count, and no member dropped for a missing/empty id). A short page, or one
    /// missing a member's id, returns `false`, so a Mirror playlist area under
    /// `library = "off"` is never treated as authoritative unless its whole
    /// member set was seen (D5).
    pub async fn get_playlist_clips(
        &self,
        http: &impl Http,
        id: &str,
    ) -> Result<(Vec<Clip>, bool)> {
        let path = format!("{PLAYLIST_PATH}{id}/");
        let body = self.api_get_retrying(http, &path).await?;
        parse_playlist_clips(&body)
    }

    /// Read the authenticated account's billing information.
    pub async fn get_billing_info(&self, http: &impl Http) -> Result<BillingInfo> {
        let body = self.api_get_retrying(http, BILLING_INFO_PATH).await?;
        parse_billing_info(&body)
    }

    /// List a clip's already-separated stems (free, read-only).
    ///
    /// Uses the live stems shape: first `GET /api/clip/{id}/stems/pages` for the
    /// page count (`{"pages": N}`), then `GET /api/clip/{id}/stems?page=P` for
    /// each `P` in `0..N` (the pages are 0-indexed), whose body is
    /// `{"stems": [<clip>, ...]}` where each stem is a full clip object. Every
    /// request rides the shared limiter and retry. This endpoint only reads: it
    /// never spends credits and never triggers separation, so it is safe on the
    /// bulk mirror path. The caller must only invoke it when the clip's
    /// `has_stem` is true.
    ///
    /// Returns the collected stems paired with a `complete` flag that is `true`
    /// only when the listing was fully and authoritatively enumerated: the page
    /// count came back and every one of its pages drained, AFTER at least one
    /// stem was seen. This encodes the deletion-safety invariant: an empty
    /// listing (`pages == 0`, or a `400`/`404` on the page-count endpoint, which
    /// Suno returns for a clip with zero stems), a transport failure, or a
    /// partial drain (a page error mid-enumeration surfaces as `Err`) all yield a
    /// non-authoritative result, so the caller KEEPS any existing local stems and
    /// never reads the absence as "no stems". A clip that declares more than
    /// [`MAX_PAGES`] pages is likewise a truncated listing and never authoritative.
    /// A stem is only ever removed from an authoritative (`complete`) listing that
    /// omits it, or when its owning clip's audio is deleted.
    pub async fn list_stems(&self, http: &impl Http, clip_id: &str) -> Result<(Vec<Stem>, bool)> {
        let declared = self.stem_page_count(http, clip_id).await?;
        // Zero pages (or no page count) is Suno's "this clip has no stems"
        // answer: indeterminate for deletion, never an authoritative empty.
        if declared == 0 {
            return Ok((Vec::new(), false));
        }
        let pages = declared.min(MAX_PAGES);
        let mut stems: Vec<Stem> = Vec::new();
        for page in 0..pages {
            // Pages are 0-indexed (0..N-1); note the path has no trailing slash
            // before the query, distinguishing it from `.../stems/pages`.
            let path = format!("/api/clip/{clip_id}/stems?page={page}");
            // A page error mid-enumeration is indeterminate, not a clean end:
            // surface it so the caller keeps existing stems rather than reading a
            // partial drain as authoritative and removing stems.
            let body = self.api_get_retrying(http, &path).await?;
            stems.extend(parse_stems_page(&body));
        }
        dedupe_stems(&mut stems);
        // Authoritative only when the whole declared page set actually drained
        // and it held stems: an all-empty listing is never "no stems", and a
        // clip declaring more than the `MAX_PAGES` cap is a truncated listing,
        // never authoritative, so its un-fetched stems are kept (mirroring the
        // feed's `list_clips` cap handling).
        let complete = !stems.is_empty() && declared <= MAX_PAGES;
        Ok((stems, complete))
    }

    /// Read the stems page count for a clip from `GET /api/clip/{id}/stems/pages`
    /// (`{"pages": N}`).
    ///
    /// A clip with no stems answers `400`/`404` here; both mean "no stems" and
    /// map to `0` (indeterminate, never an authoritative empty set), while any
    /// other error (a transient `5xx`, a transport failure) propagates so the
    /// caller treats the stems as unknown and keeps them.
    async fn stem_page_count(&self, http: &impl Http, clip_id: &str) -> Result<u32> {
        let path = format!("/api/clip/{clip_id}/stems/pages");
        match self.api_get_retrying(http, &path).await {
            Ok(body) => Ok(parse_stem_page_count(&body)),
            Err(err) if is_invalid_page_error(&err) => Ok(0),
            Err(Error::NotFound(_)) => Ok(0),
            Err(err) => Err(err),
        }
    }

    /// Try the dedicated clip endpoint, returning `None` when it is missing or
    /// returns a body that does not yield the requested clip.
    async fn try_get_clip(&self, http: &impl Http, id: &str) -> Result<Option<Clip>> {
        let path = format!("/api/clip/{id}");
        match self.api_get_retrying(http, &path).await {
            Ok(body) => Ok(parse_clip(&body).filter(|clip| clip.id == id)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Locate a clip by scanning the library feed.
    async fn find_in_feed(&self, http: &impl Http, id: &str) -> Result<Clip> {
        let (clips, _complete, _) = self.list_clips(http, false, None).await?;
        clips
            .into_iter()
            .find(|clip| clip.id == id)
            .ok_or_else(|| Error::Api(format!("clip {id} not found in the library")))
    }

    /// Perform an authenticated GET, refreshing the JWT once on a 401/403.
    async fn api_get(&self, http: &impl Http, path: &str) -> Result<Vec<u8>> {
        self.api_request(http, Method::Get, path, Vec::new()).await
    }

    /// A retrying GET: [`api_send_retrying`](Self::api_send_retrying) with no body.
    async fn api_get_retrying(&self, http: &impl Http, path: &str) -> Result<Vec<u8>> {
        self.api_send_retrying(http, Method::Get, path, Vec::new())
            .await
    }

    /// Like [`api_request`](Self::api_request) but rides through Suno's rate
    /// limiter, pacing each request to the adaptive rate and backing off through
    /// the [`Clock`] on a `429` (honouring `Retry-After` when present, defaulting
    /// to 5s and capped at 60s) or a transient connection failure, up to
    /// [`API_MAX_RETRIES`] times. Each attempt reconstructs the full request
    /// (method, path, and body), so a throttled feed page re-POSTs the same
    /// cursor rather than skipping ahead.
    ///
    /// Pacing lives here, at the single per-request layer, rather than in any
    /// paged walk, so it composes with whatever listing calls it: a page or a
    /// cursor walk pace identically. The [`AdaptiveLimiter`] paces reactively:
    /// an unthrottled walk waits nowhere, and only after the first `429` does it
    /// reserve shared request slots so concurrent callers are spaced in aggregate
    /// at `1/rate`, widening that spacing as the rate is halved again.
    ///
    /// The WAV render flow deliberately keeps to the plain [`api_get`](Self::api_get):
    /// the executor owns that retry so its budget and poll interval stay in one
    /// place. Library, playlist, and lineage reads use this so a full-library
    /// walk is not aborted by a single throttled page.
    async fn api_send_retrying(
        &self,
        http: &impl Http,
        method: Method,
        path: &str,
        body: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let pace = self.limiter.lock().unwrap().pace(Instant::now());
        if !pace.is_zero() {
            self.clock.sleep(pace).await;
        }
        let mut retries = 0;
        loop {
            match self.api_request(http, method, path, body.clone()).await {
                Ok(response) => return Ok(response),
                Err(Error::RateLimited { retry_after }) if retries < API_MAX_RETRIES => {
                    self.clock.sleep(retry_after_delay(retry_after)).await;
                    retries += 1;
                }
                Err(Error::Connection(_)) if retries < API_MAX_RETRIES => {
                    self.clock.sleep(backoff_delay(retries, None)).await;
                    retries += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Perform an authenticated request, refreshing the JWT once on a 401/403.
    ///
    /// `body` is sent only by the adapter when non-empty, so a GET or a bodyless
    /// POST reaches the network unchanged.
    async fn api_request(
        &self,
        http: &impl Http,
        method: Method,
        path: &str,
        body: Vec<u8>,
    ) -> Result<Vec<u8>> {
        // Crate-wide POST allow-list. Every mutating Suno API request funnels
        // through here, so refusing any POST to a path outside the known-safe
        // set means a destructive or credit-spending endpoint can never be sent,
        // even by a future edit that forgets the invariant. GETs are free and
        // unrestricted; only POSTs are gated.
        if method == Method::Post && !post_path_allowed(path) {
            return Err(Error::Refused(format!(
                "POST to {path} is not on the allow-list"
            )));
        }
        let url = format!("{SUNO_API_BASE_URL}{path}");
        let mut auth_refreshed = false;
        loop {
            let jwt = self.auth.ensure_jwt(self.clock.now_unix(), http).await?;
            let mut request = match method {
                Method::Get => HttpRequest::get(url.clone()),
                Method::Post => HttpRequest::post(url.clone(), body.clone()),
            };
            request
                .headers
                .push(("Authorization".to_string(), format!("Bearer {jwt}")));
            let response = http
                .send(request)
                .await
                .map_err(|err| Error::Connection(err.to_string()))?;
            match response.status {
                200..=299 => {
                    self.limiter.lock().unwrap().on_success();
                    return Ok(response.body);
                }
                401 | 403 if !auth_refreshed => {
                    self.auth.invalidate_jwt();
                    auth_refreshed = true;
                }
                401 | 403 => {
                    return Err(Error::Auth(format!(
                        "Suno API auth failed with status {}",
                        response.status
                    )));
                }
                429 => {
                    self.limiter.lock().unwrap().on_rate_limit();
                    return Err(Error::RateLimited {
                        retry_after: retry_after(&response),
                    });
                }
                400 => {
                    let preview: String = String::from_utf8_lossy(&response.body)
                        .chars()
                        .take(200)
                        .collect();
                    return Err(Error::BadRequest(format!(
                        "Suno API returned 400: {preview}"
                    )));
                }
                404 => {
                    return Err(Error::NotFound(format!("Suno API returned 404: {path}")));
                }
                status => {
                    let preview: String = String::from_utf8_lossy(&response.body)
                        .chars()
                        .take(200)
                        .collect();
                    return Err(Error::Api(format!("Suno API returned {status}: {preview}")));
                }
            }
        }
    }
}

/// Whether a Suno API path may be the target of a POST (the crate-wide POST
/// allow-list). Membership is deliberately narrow so a mutating request is only
/// ever sent to a vetted endpoint:
///
/// - [`FEED_V3_PATH`] — the cursor-paginated library listing (a POST by design).
/// - `…/convert_wav/` — the per-clip server-side lossless WAV render.
///
/// A GET is never gated (reads are free and non-mutating). Any credit-spending
/// generation endpoint is deliberately absent here.
fn post_path_allowed(path: &str) -> bool {
    if path == FEED_V3_PATH {
        return true;
    }
    // The per-clip WAV render: /api/gen/{id}/convert_wav/ with a single id.
    if let Some(rest) = path.strip_prefix("/api/gen/")
        && let Some(id) = rest.strip_suffix("/convert_wav/")
    {
        return is_single_id_segment(id);
    }
    false
}

/// Whether `segment` is a single, non-empty path id segment: no slash, no query,
/// and no `..` traversal, so an allow-list match can never be smuggled past by a
/// crafted path.
fn is_single_id_segment(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.contains('/')
        && !segment.contains('?')
        && !segment.contains("..")
}

/// Whether an error is Suno's "this clip has no stems" answer on the stems
/// page-count endpoint: a `400` (it returns `400 "Invalid page number"` for a
/// clip with zero stems). Distinguished from a transient `5xx` (also
/// [`Error::Api`]) so a server error is never mistaken for "no stems".
fn is_invalid_page_error(err: &Error) -> bool {
    matches!(err, Error::BadRequest(_))
}

/// Drop stems that repeat across pages, keeping the first occurrence of each
/// download URL so a paged listing counts a stem once.
fn dedupe_stems(stems: &mut Vec<Stem>) {
    let mut seen = BTreeSet::new();
    stems.retain(|stem| seen.insert(stem.url.clone()));
}

/// Deduplicate ids in first-seen order, dropping empties. Shared by the by-id
/// fetch paths so the batch, the fallback, and the returned order all agree.
fn dedup_nonempty<'a>(ids: &[&'a str]) -> Vec<&'a str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    ids.iter()
        .copied()
        .filter(|id| !id.is_empty() && seen.insert(id))
        .collect()
}

#[cfg(test)]
mod tests;
