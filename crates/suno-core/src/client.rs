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
/// Holds the [`Clock`] so [`api_request`](Self::api_request) can back off on a
/// `429` or transient failure, and an [`AdaptiveLimiter`] that paces reactively:
/// it waits nowhere until a `429`, then halves the rate and ramps it back on
/// sustained success.
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

    /// The adaptive limiter's current requests-per-second rate, for tests.
    #[cfg(test)]
    pub(crate) fn limiter_rate(&self) -> f64 {
        self.limiter.lock().unwrap().rate()
    }

    /// List clips across the whole library, or only liked clips.
    ///
    /// Walks the cursor-paginated `POST /api/feed/v3` feed, hard-capped at
    /// [`MAX_PAGES`], truncating to `limit` when set.
    ///
    /// The `complete` flag is `true` only when paging ended on a server-reported
    /// `has_more == false`; any other stop (missing `has_more`, no usable cursor,
    /// `limit`, [`MAX_PAGES`], transport error) yields `false` so the caller
    /// never treats a truncated listing as authoritative for deletion.
    /// `any_filtered` is `true` when the downloadable filter dropped any clip,
    /// which likewise denies deletion authority since it may have hidden a
    /// manifest-tracked clip.
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
    /// A `404` maps to `None` (not yet rendered), and it skips the shared retry
    /// so the caller's poll loop owns that budget.
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
    /// The trailing slash on `.../aligned_lyrics/v2/` is required. An
    /// instrumental or un-alignable clip returns `200` with empty arrays, and a
    /// `404` is treated the same way, so an absent alignment is "no synced
    /// lyrics" rather than a run failure.
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
    /// Used by lineage resolution to gap-fill ancestors absent from a normal
    /// listing, including trashed ones. Ids are fetched in one batch via
    /// [`get_songs_by_ids`](Self::get_songs_by_ids); any the batch omits fall
    /// back to one `GET /api/clip/{id}` each, bounded by `concurrency`, attempted
    /// once, with a `404` skipped. A `429` while batching propagates rather than
    /// fanning out into per-id requests.
    ///
    /// No downloadability filter is applied: an ancestor may be an infill or
    /// context artefact the walk must still traverse, so these clips are for
    /// resolution only and must never be treated as download candidates. Ids are
    /// deduplicated in order and the result preserves that order, matched by id.
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
    /// The pure batch primitive: deduplicated ids are split into
    /// [`GET_SONGS_CHUNK`] chunks and matched back by id, preserving input order.
    /// Ids the batch does not return are left for the caller to fill.
    ///
    /// The endpoint is undocumented and may be unavailable: a chunk it cannot
    /// serve (any `4xx`/`5xx`, transport failure, or unexpected body) yields
    /// nothing for that chunk rather than erroring, so an outage degrades rather
    /// than breaks. A `429` rides the retry and then propagates rather than
    /// fanning out into per-id requests; an auth failure likewise propagates.
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

    /// The per-id `GET /api/clip/{id}` fallback for
    /// [`get_clips_by_ids`](Self::get_clips_by_ids), with bounded concurrency.
    /// Returns any clip (trashed or artefact) unfiltered; a `404` is skipped and
    /// input order is preserved.
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

    /// Fetch a clip's immediate parent, or `None` when the clip is a root.
    ///
    /// A root's parent comes back as `200` with a bodiless, id-less clip (not a
    /// `404`); [`parse_clip`] gates on a non-empty id so that maps to `Ok(None)`.
    /// The `404` arm is a fallback for the alternative encoding. Any other
    /// failure propagates rather than being mistaken for a root.
    pub async fn get_clip_parent(&self, http: &impl Http, id: &str) -> Result<Option<Clip>> {
        let path = format!("{CLIP_PARENT_PATH}?clip_id={id}");
        match self.api_get_retrying(http, &path).await {
            Ok(body) => Ok(parse_clip(&body)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// List the account's own playlists, paging `/api/playlist/me`.
    ///
    /// Trashed and share-list playlists are excluded by query. Paging stops on
    /// the first empty page, is hard-capped at [`MAX_PAGES`], and de-duplicates
    /// by id so a server that ignores the page parameter cannot loop or inflate
    /// the set.
    ///
    /// A hard failure propagates: the caller then refuses every playlist
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
    /// `playlist_clips[]` is already ordered with trashed members excluded, so
    /// order is preserved and no downloadability filter is applied. Only clips
    /// with a non-empty id are kept.
    ///
    /// The returned `bool` is a completeness signal for deletion authority:
    /// `true` only when `num_total_results` is present, equals the raw count, and
    /// no member was dropped for a missing id. A short or id-missing page returns
    /// `false`, so a Mirror playlist under `library = "off"` is never
    /// authoritative unless its whole member set was seen.
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
    /// Pages `GET /api/clip/{id}/stems/pages` then `.../stems?page=P` (0-indexed).
    /// This endpoint only reads: it never spends credits or triggers separation,
    /// so it is safe on the bulk mirror path. Call only when the clip's
    /// `has_stem` is true.
    ///
    /// The `complete` flag is `true` only when the page count came back and every
    /// page drained after at least one stem was seen. An empty listing
    /// (`pages == 0` or a `400`/`404` on the page-count endpoint), a transport
    /// failure, a partial drain, or a count above [`MAX_PAGES`] all yield
    /// `false`, so the caller KEEPS existing local stems rather than reading the
    /// absence as "no stems".
    pub async fn list_stems(&self, http: &impl Http, clip_id: &str) -> Result<(Vec<Stem>, bool)> {
        let declared = self.stem_page_count(http, clip_id).await?;
        // Zero pages is Suno's "no stems" answer: indeterminate, never an
        // authoritative empty.
        if declared == 0 {
            return Ok((Vec::new(), false));
        }
        let pages = declared.min(MAX_PAGES);
        let mut stems: Vec<Stem> = Vec::new();
        for page in 0..pages {
            // Pages are 0-indexed; no trailing slash before the query (unlike
            // `.../stems/pages`).
            let path = format!("/api/clip/{clip_id}/stems?page={page}");
            // A page error mid-enumeration is indeterminate: surface it so the
            // caller keeps existing stems rather than reading a partial drain as
            // authoritative.
            let body = self.api_get_retrying(http, &path).await?;
            stems.extend(parse_stems_page(&body));
        }
        dedupe_stems(&mut stems);
        let complete = !stems.is_empty() && declared <= MAX_PAGES;
        Ok((stems, complete))
    }

    /// Read the stems page count from `GET /api/clip/{id}/stems/pages`.
    ///
    /// A clip with no stems answers `400`/`404`; both map to `0` (indeterminate,
    /// never an authoritative empty). Any other error propagates so the caller
    /// keeps the stems as unknown.
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

    /// Like [`api_request`](Self::api_request) but paces through the adaptive
    /// rate limiter and backs off via the [`Clock`] on a `429` or transient
    /// failure, up to [`API_MAX_RETRIES`] times. Each attempt reconstructs the
    /// request, so a throttled feed page re-POSTs the same cursor rather than
    /// skipping ahead.
    ///
    /// Pacing lives at this single per-request layer so it composes with any
    /// paged walk. The WAV render flow instead uses the plain
    /// [`api_get`](Self::api_get) so the executor owns that retry budget.
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
        // Crate-wide POST allow-list: every mutating request funnels through
        // here, so a destructive or credit-spending endpoint can never be sent.
        // GETs are free and unrestricted.
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
/// allow-list), deliberately narrow so a mutating request only ever reaches a
/// vetted endpoint: the library feed ([`FEED_V3_PATH`]) and the per-clip WAV
/// render (`…/convert_wav/`). Any credit-spending endpoint is deliberately
/// absent; GETs are never gated.
fn post_path_allowed(path: &str) -> bool {
    if path == FEED_V3_PATH {
        return true;
    }
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

/// Whether an error is Suno's "no stems" answer (a `400`) on the page-count
/// endpoint, distinguished from a transient `5xx` so a server error is never
/// mistaken for "no stems".
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
