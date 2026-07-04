//! The Suno API client: lists the library behind the [`Http`](crate::Http) port.

use std::collections::BTreeSet;
use std::sync::Mutex;

use futures_util::stream::{self, StreamExt};
use serde_json::Value;

use crate::auth::ClerkAuth;
use crate::backoff::{backoff_delay, retry_after};
use crate::clock::Clock;
use crate::consts::{
    API_MAX_RETRIES, BILLING_INFO_PATH, CLIP_PARENT_PATH, FEED_INITIAL_RATE, FEED_PAGE_SIZE,
    FEED_V3_PATH, MAX_PAGES, PLAYLIST_ME_PATH, PLAYLIST_PATH, SUNO_API_BASE_URL,
};
use crate::error::{Error, Result};
use crate::http::{Http, HttpRequest, Method};
use crate::is_downloadable;
use crate::limiter::{AdaptiveLimiter, retry_after_delay};
use crate::lyrics::AlignedLyrics;
use crate::model::Clip;

/// One of the account's own playlists, as listed by `/api/playlist/me`.
///
/// Carries only what playlist reconciliation needs: the stable id (the state
/// key), the display name (drives the `.m3u8` file name and `#PLAYLIST` line),
/// and the member count for reporting. The ordered members are fetched
/// separately with [`SunoClient::get_playlist_clips`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playlist {
    /// The playlist's stable Suno id.
    pub id: String,
    /// The playlist's display name.
    pub name: String,
    /// The number of clips Suno reports in the playlist.
    pub num_clips: u64,
}

/// The authenticated account's current remaining credit balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingInfo {
    /// Credits remaining in the current billing state.
    pub total_credits_left: u64,
}

/// One separated stem of a clip, as listed by the free, read-only stems
/// endpoint.
///
/// A stem is itself a full clip object: the listing returns the same shape as
/// the library feed, so each stem carries its own clip `id`, a `title` whose
/// trailing parenthetical is the stem label (e.g. `"My Song (Vocals)"`), a
/// `status`, and a public `audio_url` on `cdn1.suno.ai` that downloads free and
/// unauthenticated. Listing and downloading stems never spends credits or
/// triggers separation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stem {
    /// The stem's own server clip id. Used both as the stable per-stem key and
    /// to render the stem's lossless WAV through the free `convert_wav` flow.
    pub id: String,
    /// The stem label, taken from the trailing parenthetical of the stem clip's
    /// title (e.g. `Vocals`, `Backing Vocals`, `Drums`). May be blank when the
    /// title has no parenthetical, so it is never used alone as a key or name.
    pub label: String,
    /// The public CDN MP3 URL the stem downloads from (a plain GET; free).
    pub url: String,
}

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
    pub async fn list_clips(
        &self,
        http: &impl Http,
        liked: bool,
        limit: Option<usize>,
    ) -> Result<(Vec<Clip>, bool)> {
        let mut clips = Vec::new();
        let mut cursor: Option<String> = None;
        let mut complete = false;
        for _ in 0..MAX_PAGES {
            let body = feed_v3_body(liked, cursor.as_deref());
            let response = self
                .api_send_retrying(http, Method::Post, FEED_V3_PATH, body)
                .await?;
            let (page_clips, has_more, next_cursor) = parse_feed_v3(&response)?;
            clips.extend(page_clips);
            match has_more {
                Some(false) => {
                    complete = true;
                    break;
                }
                Some(true) => match next_cursor {
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
        Ok((clips, complete))
    }

    /// Fetch one clip by ID.
    ///
    /// Tries the dedicated `/api/clip/{id}` endpoint first, then falls back to
    /// scanning the library feed, since that endpoint's exact shape is not yet
    /// confirmed against the live API.
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
    pub async fn wav_url(&self, http: &impl Http, id: &str) -> Result<Option<String>> {
        let path = format!("/api/gen/{id}/wav_file/");
        let body = self.api_get(http, &path).await?;
        let data: Value = serde_json::from_slice(&body)
            .map_err(|err| Error::Api(format!("invalid wav_file JSON: {err}")))?;
        Ok(data
            .get("wav_file_url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_string))
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

    /// Fetch specific clips by id, one `GET /api/clip/{id}` per id.
    ///
    /// Used by lineage resolution to gap-fill ancestors that are absent from a
    /// normal listing, including trashed ones. The v3 feed has no batch by-id
    /// filter, so each id is fetched individually; `/api/clip/{id}` returns any
    /// clip, trashed or artefact, with the full field set. Unlike
    /// [`list_clips`](Self::list_clips), no downloadability filter is applied: an
    /// ancestor may itself be an infill or context-window artefact that the
    /// lineage walk must still traverse. Clips returned here are ancestors for
    /// resolution only and must never be treated as download candidates. Ids are
    /// deduplicated in order, and an id that cannot be retrieved (a `404`) is
    /// skipped so the caller can fall back to the parent endpoint. Requests are
    /// issued with bounded concurrency, preserving the de-duplicated input order.
    pub async fn get_clips_by_ids(
        &self,
        http: &impl Http,
        ids: &[&str],
        concurrency: usize,
    ) -> Result<Vec<Clip>> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let ordered: Vec<&str> = ids
            .iter()
            .copied()
            .filter(|id| !id.is_empty() && seen.insert(id))
            .collect();
        let limit = concurrency.max(1);
        let fetched = stream::iter(ordered.iter().copied().enumerate())
            .map(|(idx, id)| async move {
                let path = format!("/api/clip/{id}");
                match self.api_get_retrying(http, &path).await {
                    Ok(body) => Ok((idx, parse_clip(&body))),
                    Err(Error::NotFound(_)) => Ok((idx, None)),
                    Err(err) => Err(err),
                }
            })
            .buffered(limit)
            .collect::<Vec<_>>()
            .await;
        let mut clips = Vec::new();
        for item in fetched {
            let (_idx, clip) = item?;
            if let Some(clip) = clip {
                clips.push(clip);
            }
        }
        Ok(clips)
    }

    /// Fetch a clip's immediate parent via the dedicated parent endpoint.
    ///
    /// Returns the parent clip, or `None` when the clip is a root (no parent) or
    /// the endpoint yields no clip. Lineage resolution uses this as a fallback
    /// when a missing ancestor cannot be retrieved by id. Only a `404` (the clip
    /// has no parent) maps to `None`; any other failure, including a transient
    /// `5xx`, propagates as an error rather than being mistaken for a root.
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
    /// Trashed and share-list playlists are excluded by query, so the result is
    /// the account's authoritative own set. Paging stops on the first empty page
    /// and is hard-capped at [`MAX_PAGES`] so a server that ignores the page
    /// parameter cannot loop forever. Only entries with a non-empty id are kept.
    ///
    /// A hard failure propagates as an error; the caller treats that as "the
    /// playlist listing did not fully enumerate" and refuses every playlist
    /// deletion this run, so a dropped fetch can never remove a `.m3u8`.
    pub async fn get_playlists(&self, http: &impl Http) -> Result<Vec<Playlist>> {
        let mut playlists = Vec::new();
        for page in 1..=MAX_PAGES {
            let path =
                format!("{PLAYLIST_ME_PATH}?page={page}&show_trashed=false&show_sharelist=false");
            let body = self.api_get_retrying(http, &path).await?;
            let page_playlists = parse_playlists(&body)?;
            if page_playlists.is_empty() {
                break;
            }
            playlists.extend(page_playlists);
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
    /// this single page (`returned == num_total_results`). A short page (a
    /// paginated or partially-listed playlist) returns `false`, so a Mirror
    /// playlist area under `library = "off"` is never treated as authoritative
    /// unless its whole member set was seen (D5).
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
        let (clips, _complete) = self.list_clips(http, false, None).await?;
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
    /// space out requests, widening that pace as the rate is halved again.
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
        let pace = self.limiter.lock().unwrap().pace();
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

/// Unwrap a `{ "clip": {...} }` wrapper to the inner clip object, or return
/// `value` unchanged when it carries no object `clip` key (it is already bare).
fn unwrap_clip(value: &Value) -> &Value {
    value
        .get("clip")
        .filter(|clip| clip.is_object())
        .unwrap_or(value)
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

/// Parse the stems page count from `GET /api/clip/{id}/stems/pages`
/// (`{"pages": N}`).
///
/// A missing, non-numeric, or negative `pages` reads as `0` (no stems), so a
/// malformed body is treated as indeterminate rather than guessing a count.
fn parse_stem_page_count(body: &[u8]) -> u32 {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|data| data.get("pages").and_then(Value::as_u64))
        .and_then(|pages| u32::try_from(pages).ok())
        .unwrap_or(0)
}

/// Parse one page of the stems listing (`{"stems": [<clip>, ...]}`) into
/// [`Stem`]s.
///
/// Each stem is a full clip object, so it is mapped with [`Clip::from_json`]:
/// the id is the stem clip id, the label is the trailing parenthetical of its
/// title, and the download URL is its public CDN MP3. Only stems carrying both a
/// non-empty id and URL are kept — a stem with no id cannot be WAV-rendered, and
/// one with no URL cannot be mirrored. Malformed JSON yields no stems (never a
/// panic), so a bad body is treated as an empty, non-authoritative page.
fn parse_stems_page(body: &[u8]) -> Vec<Stem> {
    let Ok(data) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let items = if let Some(array) = data.as_array() {
        array.as_slice()
    } else {
        data.get("stems")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    };
    items
        .iter()
        .map(parse_stem)
        .filter(|stem| !stem.id.is_empty() && !stem.url.is_empty())
        .collect()
}

/// Map one raw stem clip element to a [`Stem`]: its clip id, the trailing
/// parenthetical of its title as the label, and its public CDN MP3 URL.
fn parse_stem(raw: &Value) -> Stem {
    let clip = Clip::from_json(raw);
    Stem {
        id: clip.id.clone(),
        label: stem_label_from_title(&clip.title),
        url: clip.mp3_url(),
    }
}

/// The stem label carried in a stem clip's title: the text inside its trailing
/// parenthetical (`"My Song (Backing Vocals)"` -> `Backing Vocals`). Returns an
/// empty string when the title has no closing parenthetical, so the caller falls
/// back to the stem id for naming.
fn stem_label_from_title(title: &str) -> String {
    let trimmed = title.trim_end();
    let Some(before_close) = trimmed.strip_suffix(')') else {
        return String::new();
    };
    match before_close.rfind('(') {
        Some(open) => before_close[open + 1..].trim().to_string(),
        None => String::new(),
    }
}

/// Drop stems that repeat across pages, keeping the first occurrence of each
/// download URL so a paged listing counts a stem once.
fn dedupe_stems(stems: &mut Vec<Stem>) {
    let mut seen = BTreeSet::new();
    stems.retain(|stem| seen.insert(stem.url.clone()));
}

/// Parse a single-clip response body, accepting either a bare clip object or a
/// `{"clip": {...}}` wrapper. Returns `None` when no clip id is present.
fn parse_clip(body: &[u8]) -> Option<Clip> {
    let data: Value = serde_json::from_slice(body).ok()?;
    let raw = unwrap_clip(&data);
    let has_id = raw
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty());
    has_id.then(|| Clip::from_json(raw))
}

/// Parse `/api/billing/info/` into the remaining credits we report in `doctor`.
fn parse_billing_info(body: &[u8]) -> Result<BillingInfo> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid billing JSON: {err}")))?;
    let total_credits_left = data
        .get("total_credits_left")
        .and_then(json_u64)
        .ok_or_else(|| Error::Api("invalid billing JSON: missing total_credits_left".into()))?;
    Ok(BillingInfo { total_credits_left })
}

/// Read a numeric field that Suno may encode either as a JSON number or a
/// decimal string.
fn json_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// Build the JSON body for a `POST /api/feed/v3` page.
///
/// `filters.trashed` is the string `"False"` so the feed excludes trashed clips
/// exactly as the old v2 listing did; a `liked` walk adds `filters.liked =
/// "True"` (v3 ignores an `is_liked` key). The `cursor` is omitted on the first
/// page and set to the previous page's `next_cursor` thereafter.
fn feed_v3_body(liked: bool, cursor: Option<&str>) -> Vec<u8> {
    let mut filters = serde_json::Map::new();
    filters.insert("trashed".to_string(), Value::String("False".to_string()));
    if liked {
        filters.insert("liked".to_string(), Value::String("True".to_string()));
    }
    let mut body = serde_json::Map::new();
    body.insert("limit".to_string(), Value::from(FEED_PAGE_SIZE));
    body.insert("filters".to_string(), Value::Object(filters));
    if let Some(cursor) = cursor {
        body.insert("cursor".to_string(), Value::String(cursor.to_string()));
    }
    serde_json::to_vec(&Value::Object(body)).unwrap_or_default()
}

/// Parse a v3 feed page into the kept clips, the raw `has_more`, and the
/// `next_cursor`.
///
/// `has_more` is [`None`] when the key is missing or not a bool, so the caller
/// can refuse to treat an unrecognised page as a fully drained feed. An empty
/// `next_cursor` string maps to [`None`] so it is never re-sent as a cursor.
fn parse_feed_v3(body: &[u8]) -> Result<(Vec<Clip>, Option<bool>, Option<String>)> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid feed JSON: {err}")))?;
    let Some(object) = data.as_object() else {
        return Ok((Vec::new(), None, None));
    };
    let clips = object
        .get("clips")
        .and_then(Value::as_array)
        .map(|raw| {
            raw.iter()
                .map(Clip::from_json)
                .filter(is_downloadable)
                .collect()
        })
        .unwrap_or_default();
    let has_more = object.get("has_more").and_then(Value::as_bool);
    let next_cursor = object
        .get("next_cursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_string);
    Ok((clips, has_more, next_cursor))
}

/// Parse a `/api/playlist/me` page into playlists, dropping entries with no id.
fn parse_playlists(body: &[u8]) -> Result<Vec<Playlist>> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid playlist JSON: {err}")))?;
    Ok(data
        .get("playlists")
        .and_then(Value::as_array)
        .map(|raw| raw.iter().filter_map(parse_playlist_item).collect())
        .unwrap_or_default())
}

/// Map one raw `/api/playlist/me` entry, or `None` when it carries no id.
///
/// `num_total_results` is the playlist's member count; a missing name defaults
/// to `Untitled` (matching the clip mapping) so the file name is never empty.
fn parse_playlist_item(raw: &Value) -> Option<Playlist> {
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_string();
    let name = match raw.get("name") {
        Some(Value::String(name)) if !name.is_empty() => name.clone(),
        _ => "Untitled".to_string(),
    };
    let num_clips = raw
        .get("num_total_results")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Playlist {
        id,
        name,
        num_clips,
    })
}

/// Parse a `/api/playlist/{id}/` body into its ordered member clips plus a
/// completeness flag.
///
/// Each `playlist_clips[]` entry wraps the clip under `clip`; the wrapper is
/// unwrapped (falling back to the entry itself), order is preserved exactly, and
/// only clips with a non-empty id survive. No downloadability filter is applied:
/// a playlist may hold any clip, and members absent from the local library are
/// reconciled as comment lines by the caller, not dropped here. The scoped-sync
/// path applies [`is_downloadable`](crate::is_downloadable) itself when it fetches
/// members as download candidates.
///
/// The completeness flag is `true` when the number of raw `playlist_clips[]`
/// entries equals the response's `num_total_results`, i.e. the whole member set
/// arrived on this single page. It gates a Mirror playlist area's deletion
/// authority (D5): a short or paginated page cannot be authoritative for
/// deletion, so it returns `false`.
fn parse_playlist_clips(body: &[u8]) -> Result<(Vec<Clip>, bool)> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid playlist JSON: {err}")))?;
    let raw = data.get("playlist_clips").and_then(Value::as_array);
    let raw_len = raw.map(|a| a.len()).unwrap_or(0);
    let clips: Vec<Clip> = raw
        .map(|raw| {
            raw.iter()
                .map(|entry| Clip::from_json(unwrap_clip(entry)))
                .filter(|clip| !clip.id.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // Completeness compares the raw entry count (before the empty-id filter)
    // against the reported total: a full single page has them equal. A missing
    // or malformed total is never treated as complete, so a page whose size
    // cannot be verified fails safe toward "not authoritative" and a Mirror area
    // can never delete from it.
    let complete = data
        .get("num_total_results")
        .and_then(Value::as_u64)
        .is_some_and(|total| raw_len as u64 == total);
    Ok((clips, complete))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockHttp, RecordingClock, Reply, Rule, ScriptedHttp};
    use std::time::Duration;

    fn feed_body() -> String {
        serde_json::json!({
            "has_more": false,
            "clips": [
                {
                    "id": "a", "title": "Song A", "status": "complete",
                    "audio_url": "https://cdn1.suno.ai/a.mp3",
                    "metadata": {"tags": "rock", "duration": 120.5, "type": "gen"}
                },
                {"id": "b", "title": "Infill", "status": "complete", "metadata": {"task": "infill"}},
                {"id": "c", "title": "Streaming", "status": "streaming", "metadata": {}},
                {
                    "id": "d", "title": "Context", "status": "complete",
                    "metadata": {"type": "rendered_context_window"}
                }
            ]
        })
        .to_string()
    }

    #[test]
    fn parse_feed_v3_filters_and_reads_pagination() {
        let (clips, has_more, next_cursor) = parse_feed_v3(feed_body().as_bytes()).unwrap();
        assert_eq!(has_more, Some(false));
        assert_eq!(next_cursor, None);
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].id, "a");
        assert_eq!(clips[0].tags, "rock");
        assert!((clips[0].duration - 120.5).abs() < f64::EPSILON);
    }

    #[test]
    fn feed_v3_body_carries_filters_and_optional_cursor() {
        let first: Value = serde_json::from_slice(&feed_v3_body(false, None)).unwrap();
        assert_eq!(first["filters"]["trashed"], "False");
        assert!(first.get("cursor").is_none());
        assert!(first["filters"].get("liked").is_none());

        let liked: Value = serde_json::from_slice(&feed_v3_body(true, Some("cur42"))).unwrap();
        assert_eq!(liked["filters"]["liked"], "True");
        assert_eq!(liked["cursor"], "cur42");
    }

    #[test]
    fn audiopipe_url_is_rewritten_to_cdn() {
        let raw =
            serde_json::json!({"id": "x", "audio_url": "https://audiopipe.suno.ai/?item_id=x"});
        assert_eq!(
            Clip::from_json(&raw).audio_url,
            "https://cdn1.suno.ai/x.mp3"
        );
    }

    #[test]
    fn list_clips_authenticates_then_reads_the_feed() {
        let client_body = serde_json::json!({
            "response": {
                "last_active_session_id": "s",
                "sessions": [{"id": "s", "user": {"id": "u", "username": "h"}}]
            }
        })
        .to_string();
        let http = MockHttp::new(vec![
            Rule::new(
                "/v1/client/sessions/",
                200,
                r#"{"jwt": "a.b.c"}"#.to_string(),
            ),
            Rule::new("/v1/client", 200, client_body),
            Rule::new("/api/feed/v3", 200, feed_body()),
        ]);

        let auth = ClerkAuth::new("eyJtoken");
        pollster::block_on(auth.authenticate(&http)).unwrap();
        let client = SunoClient::new(auth, RecordingClock::new());
        let (clips, complete) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].id, "a");
        assert!(complete);
    }

    #[test]
    fn api_request_uses_clock_now_unix_for_jwt_expiry() {
        use crate::consts::JWT_REFRESH_BUFFER;
        use base64::Engine;
        let exp = 1_000_000i64;
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        let jwt_str = format!("hdr.{}.sig", payload);
        let token_body = format!(r#"{{"jwt": "{jwt_str}"}}"#);
        let client_body = serde_json::json!({
            "response": {
                "last_active_session_id": "s",
                "sessions": [{"id": "s", "user": {"id": "u", "username": "h"}}]
            }
        })
        .to_string();

        let make_http = || {
            ScriptedHttp::new()
                .route("/v1/client/sessions/", Reply::json(&token_body))
                .route("/v1/client", Reply::json(&client_body))
                .route("/api/feed/v3", Reply::json(&feed_body()))
        };

        // At the refresh boundary: ensure_jwt triggers a second refresh_jwt call.
        let http = make_http();
        let auth = ClerkAuth::new("eyJtoken");
        pollster::block_on(auth.authenticate(&http)).unwrap();
        let client = SunoClient::new(auth, RecordingClock::at(exp - JWT_REFRESH_BUFFER));
        let (clips, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert_eq!(clips.len(), 1);
        // authenticate + api_request refresh = 2 token calls.
        assert_eq!(http.count("/v1/client/sessions/"), 2);

        // Just before the boundary: no additional refresh.
        let http2 = make_http();
        let auth2 = ClerkAuth::new("eyJtoken");
        pollster::block_on(auth2.authenticate(&http2)).unwrap();
        let client2 = SunoClient::new(auth2, RecordingClock::at(exp - JWT_REFRESH_BUFFER - 1));
        let (clips2, _) = pollster::block_on(client2.list_clips(&http2, false, None)).unwrap();
        assert_eq!(clips2.len(), 1);
        // Only authenticate's token call; no extra refresh.
        assert_eq!(http2.count("/v1/client/sessions/"), 1);
    }

    #[test]
    fn list_clips_reports_incomplete_when_paging_is_capped() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            "/api/feed/v3",
            200,
            serde_json::json!({
                "has_more": true,
                "next_cursor": "cur1",
                "clips": [{
                    "id": "a", "title": "Song A", "status": "complete",
                    "audio_url": "https://cdn1.suno.ai/a.mp3",
                    "metadata": {"type": "gen"}
                }]
            })
            .to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let (_clips, complete) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert!(!complete);
    }

    fn auth_rules() -> Vec<Rule> {
        let client_body = serde_json::json!({
            "response": {
                "last_active_session_id": "s",
                "sessions": [{"id": "s", "user": {"id": "u", "username": "h"}}]
            }
        })
        .to_string();
        vec![
            Rule::new(
                "/v1/client/sessions/",
                200,
                r#"{"jwt": "a.b.c"}"#.to_string(),
            ),
            Rule::new("/v1/client", 200, client_body),
        ]
    }

    fn authed_client(http: &MockHttp) -> SunoClient<RecordingClock> {
        let auth = ClerkAuth::new("eyJtoken");
        pollster::block_on(auth.authenticate(http)).unwrap();
        SunoClient::new(auth, RecordingClock::new())
    }

    #[test]
    fn get_billing_info_reads_remaining_credits() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            BILLING_INFO_PATH,
            200,
            r#"{"total_credits_left":500,"monthly_limit":1000,"monthly_usage":500}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let billing = pollster::block_on(client.get_billing_info(&http)).unwrap();
        assert_eq!(billing.total_credits_left, 500);
    }

    #[test]
    fn get_billing_info_rejects_missing_balance() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            BILLING_INFO_PATH,
            200,
            r#"{"monthly_usage":12}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let err = pollster::block_on(client.get_billing_info(&http)).unwrap_err();
        assert!(err.to_string().contains("total_credits_left"));
    }

    #[test]
    fn aligned_lyrics_reads_words_and_lines() {
        let mut rules = auth_rules();
        let body = serde_json::json!({
            "aligned_words": [
                {"word": "hi", "success": true, "start_s": 0.5, "end_s": 0.9, "p_align": 0.99}
            ],
            "aligned_lyrics": [
                {"text": "hi", "start_s": 0.5, "end_s": 0.9, "section": "Verse 1",
                 "words": [{"text": "hi", "start_s": 0.5, "end_s": 0.9}]}
            ],
            "hoot_cer": 0.2, "is_streamed": false
        })
        .to_string();
        rules.push(Rule::new("/aligned_lyrics/v2/", 200, body));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let aligned = pollster::block_on(client.aligned_lyrics(&http, "clip-1")).unwrap();
        assert_eq!(aligned.words.len(), 1);
        assert_eq!(aligned.lines.len(), 1);
        assert_eq!(aligned.lines[0].section, "Verse 1");
        assert!(!aligned.is_empty());
    }

    #[test]
    fn aligned_lyrics_empty_arrays_map_to_empty() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            "/aligned_lyrics/v2/",
            200,
            r#"{"aligned_words":[],"aligned_lyrics":[],"hoot_cer":1.0}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let aligned = pollster::block_on(client.aligned_lyrics(&http, "instr")).unwrap();
        assert!(aligned.is_empty());
    }

    #[test]
    fn aligned_lyrics_maps_404_to_empty() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            "/aligned_lyrics/v2/",
            404,
            "not found".to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let aligned = pollster::block_on(client.aligned_lyrics(&http, "missing")).unwrap();
        assert!(aligned.is_empty());
    }

    fn scripted_client(http: &ScriptedHttp, clock: RecordingClock) -> SunoClient<RecordingClock> {
        let auth = ClerkAuth::new("eyJtoken");
        pollster::block_on(auth.authenticate(http)).unwrap();
        SunoClient::new(auth, clock)
    }

    fn one_clip_page(id: &str, next_cursor: Option<&str>) -> String {
        let mut page = serde_json::json!({
            "has_more": next_cursor.is_some(),
            "clips": [{
                "id": id, "title": "Song", "status": "complete",
                "audio_url": format!("https://cdn1.suno.ai/{id}.mp3"),
                "metadata": {"type": "gen"}
            }]
        });
        if let Some(cursor) = next_cursor {
            page["next_cursor"] = serde_json::json!(cursor);
        }
        page.to_string()
    }

    #[test]
    fn list_clips_retries_a_rate_limited_page() {
        let http = ScriptedHttp::new().with_auth().route_seq(
            "/api/feed/v3",
            vec![Reply::status(429), Reply::json(&feed_body())],
        );
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let (clips, complete) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert_eq!(clips.len(), 1);
        assert!(complete);
        // The throttled page was retried once, waiting the default post-429 wait.
        assert_eq!(http.count("/api/feed/v3"), 2);
        assert_eq!(clock.sleeps(), vec![Duration::from_secs(5)]);
    }

    #[test]
    fn list_clips_honours_retry_after_on_a_throttled_page() {
        let http = ScriptedHttp::new().with_auth().route_seq(
            "/api/feed/v3",
            vec![
                Reply::status(429).with_retry_after(7),
                Reply::json(&feed_body()),
            ],
        );
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let (clips, _complete) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert_eq!(clips.len(), 1);
        // The server's Retry-After is honoured directly as the post-429 wait.
        assert_eq!(clock.sleeps(), vec![Duration::from_secs(7)]);
    }

    #[test]
    fn list_clips_re_posts_the_same_cursor_after_a_throttled_page() {
        // A 429 mid-walk must re-POST the *same* cursor, not skip a page.
        let http = ScriptedHttp::new().with_auth().route_seq(
            "/api/feed/v3",
            vec![
                Reply::json(&one_clip_page("a", Some("cur1"))),
                Reply::status(429),
                Reply::json(&one_clip_page("b", None)),
            ],
        );
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let (clips, complete) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert!(complete);
        assert_eq!(clips.len(), 2);
        let bodies = http.bodies();
        let feed_bodies: Vec<&String> = bodies.iter().filter(|b| b.contains("filters")).collect();
        assert_eq!(feed_bodies.len(), 3, "page 1, the 429 retry, then page 2");
        // The retry (body 2) carries the SAME cursor as the throttled call (body 2 == the
        // second feed POST), i.e. the cursor from page 1's next_cursor.
        let retried: Value = serde_json::from_str(feed_bodies[1]).unwrap();
        let after_retry: Value = serde_json::from_str(feed_bodies[2]).unwrap();
        assert_eq!(retried["cursor"], "cur1");
        assert_eq!(after_retry["cursor"], "cur1");
    }

    #[test]
    fn list_clips_threads_the_cursor_across_pages() {
        let http = ScriptedHttp::new().with_auth().route_seq(
            "/api/feed/v3",
            vec![
                Reply::json(&one_clip_page("a", Some("cur1"))),
                Reply::json(&one_clip_page("b", None)),
            ],
        );
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let (clips, complete) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert!(complete);
        assert_eq!(clips.len(), 2);
        let bodies = http.bodies();
        let feed_bodies: Vec<&String> = bodies.iter().filter(|b| b.contains("filters")).collect();
        assert_eq!(feed_bodies.len(), 2);
        let page1: Value = serde_json::from_str(feed_bodies[0]).unwrap();
        let page2: Value = serde_json::from_str(feed_bodies[1]).unwrap();
        // Page 1 omits the cursor; page 2 carries exactly page 1's next_cursor.
        assert!(page1.get("cursor").is_none());
        assert_eq!(page2["cursor"], "cur1");
    }

    #[test]
    fn list_clips_stops_incomplete_when_has_more_but_no_cursor() {
        // has_more == true with no usable next_cursor: a truncated feed. The walk
        // must stop, report incomplete, and never re-POST a null cursor.
        let page = serde_json::json!({
            "has_more": true,
            "clips": [{
                "id": "a", "title": "Song", "status": "complete",
                "audio_url": "https://cdn1.suno.ai/a.mp3", "metadata": {"type": "gen"}
            }]
        })
        .to_string();
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/feed/v3", Reply::json(&page));
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let (clips, complete) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert!(!complete);
        assert_eq!(clips.len(), 1);
        assert_eq!(http.count("/api/feed/v3"), 1, "no re-POST of a null cursor");
    }

    #[test]
    fn list_clips_is_incomplete_when_has_more_is_missing() {
        // A page with no has_more key must not be read as a fully drained feed.
        let page = serde_json::json!({
            "clips": [{
                "id": "a", "title": "Song", "status": "complete",
                "audio_url": "https://cdn1.suno.ai/a.mp3", "metadata": {"type": "gen"}
            }]
        })
        .to_string();
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/feed/v3", Reply::json(&page));
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let (clips, complete) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert!(!complete);
        assert_eq!(clips.len(), 1);
        assert_eq!(http.count("/api/feed/v3"), 1);
    }

    #[test]
    fn list_clips_propagates_an_error_mid_walk_and_never_completes() {
        let http = ScriptedHttp::new().with_auth().route_seq(
            "/api/feed/v3",
            vec![
                Reply::json(&one_clip_page("a", Some("cur1"))),
                Reply::status(500),
            ],
        );
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let result = pollster::block_on(client.list_clips(&http, false, None));
        assert!(matches!(result, Err(Error::Api(_))));
    }

    #[test]
    fn list_clips_is_complete_on_an_empty_drained_feed() {
        // An empty but fully drained feed is authoritative (complete = true);
        // deletion is separately gated by there being a mirror source.
        let page = serde_json::json!({"has_more": false, "clips": []}).to_string();
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/feed/v3", Reply::json(&page));
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let (clips, complete) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert!(complete);
        assert!(clips.is_empty());
    }

    #[test]
    fn list_clips_liked_scope_sends_the_liked_filter() {
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/feed/v3", Reply::json(&feed_body()));
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let _ = pollster::block_on(client.list_clips(&http, true, None)).unwrap();
        let bodies = http.bodies();
        let feed_body = bodies.iter().find(|b| b.contains("filters")).unwrap();
        let value: Value = serde_json::from_str(feed_body).unwrap();
        assert_eq!(value["filters"]["liked"], "True");
        assert_eq!(value["filters"]["trashed"], "False");
    }

    #[test]
    fn list_clips_does_not_pace_an_unthrottled_walk() {
        let http = ScriptedHttp::new().with_auth().route_seq(
            "/api/feed/v3",
            vec![
                Reply::json(&one_clip_page("a", Some("cur1"))),
                Reply::json(&one_clip_page("e", None)),
            ],
        );
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let (clips, complete) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert!(complete);
        assert_eq!(clips.len(), 2);
        assert_eq!(http.count("/api/feed/v3"), 2);
        // Pacing is reactive: with no 429 the whole walk waits nowhere.
        assert!(clock.sleeps().is_empty());
    }

    #[test]
    fn list_clips_slows_its_pace_after_a_throttled_page() {
        let http = ScriptedHttp::new().with_auth().route_seq(
            "/api/feed/v3",
            vec![
                Reply::status(429),
                Reply::json(&one_clip_page("a", Some("cur1"))),
                Reply::json(&one_clip_page("e", None)),
            ],
        );
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let (clips, complete) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
        assert!(complete);
        assert_eq!(clips.len(), 2);
        // The 429 halved the rate, so the default post-429 wait is followed by a
        // doubled inter-page pace (500ms to 1s) for the next page.
        assert_eq!(
            clock.sleeps(),
            vec![Duration::from_secs(5), Duration::from_secs(1)]
        );
    }

    #[test]
    fn list_clips_gives_up_after_max_retries() {
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/feed/v3", Reply::status(429));
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());

        let result = pollster::block_on(client.list_clips(&http, false, None));
        assert!(matches!(result, Err(Error::RateLimited { .. })));
        let budget = crate::consts::API_MAX_RETRIES as usize;
        assert_eq!(clock.sleeps().len(), budget);
        assert_eq!(http.count("/api/feed/v3"), budget + 1);
    }

    #[test]
    fn parse_clip_accepts_bare_and_wrapped_shapes() {
        let bare = serde_json::json!({"id": "z", "title": "Zed"}).to_string();
        assert_eq!(parse_clip(bare.as_bytes()).unwrap().id, "z");

        let wrapped = serde_json::json!({"clip": {"id": "w", "title": "Wai"}}).to_string();
        assert_eq!(parse_clip(wrapped.as_bytes()).unwrap().id, "w");

        let missing = serde_json::json!({"detail": "not found"}).to_string();
        assert!(parse_clip(missing.as_bytes()).is_none());
    }

    #[test]
    fn get_clip_uses_the_dedicated_endpoint() {
        let clip_body = serde_json::json!({
            "id": "z", "title": "Zed", "status": "complete",
            "audio_url": "https://cdn1.suno.ai/z.mp3",
            "metadata": {"tags": "jazz", "duration": 99.0, "type": "gen"}
        })
        .to_string();
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/clip/", 200, clip_body));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let clip = pollster::block_on(client.get_clip(&http, "z")).unwrap();
        assert_eq!(clip.id, "z");
        assert_eq!(clip.title, "Zed");
        assert_eq!(clip.tags, "jazz");
    }

    #[test]
    fn get_clip_falls_back_to_the_feed_when_endpoint_missing() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            "/api/clip/",
            404,
            r#"{"detail": "not found"}"#.to_string(),
        ));
        rules.push(Rule::new("/api/feed/v3", 200, feed_body()));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let clip = pollster::block_on(client.get_clip(&http, "a")).unwrap();
        assert_eq!(clip.id, "a");
        assert_eq!(clip.tags, "rock");
    }

    #[test]
    fn request_wav_accepts_a_2xx_status() {
        let mut rules = auth_rules();
        rules.push(Rule::new("/convert_wav/", 201, "{}".to_string()));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        assert!(pollster::block_on(client.request_wav(&http, "z")).is_ok());
    }

    #[test]
    fn wav_url_reads_the_ready_url() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            "/wav_file/",
            200,
            r#"{"wav_file_url": "https://cdn1.suno.ai/z.wav"}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let url = pollster::block_on(client.wav_url(&http, "z")).unwrap();
        assert_eq!(url.as_deref(), Some("https://cdn1.suno.ai/z.wav"));
    }

    #[test]
    fn wav_url_is_none_until_the_render_is_ready() {
        let mut rules = auth_rules();
        rules.push(Rule::new("/wav_file/", 200, "{}".to_string()));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let url = pollster::block_on(client.wav_url(&http, "z")).unwrap();
        assert_eq!(url, None);
    }

    #[test]
    fn get_clips_by_ids_fetches_each_id_and_keeps_artefacts() {
        // The per-id gap-fill path must not apply the listing's downloadability
        // filter: an infill ancestor and an upload root both survive, fetched one
        // `/api/clip/{id}` at a time.
        let p1 = serde_json::json!({
            "id": "p1", "title": "Infill Ancestor", "status": "complete",
            "metadata": {"type": "gen", "task": "infill"}
        })
        .to_string();
        let p2 = serde_json::json!({
            "id": "p2", "title": "Uploaded Root", "status": "complete",
            "metadata": {"type": "upload"}
        })
        .to_string();
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/clip/p1", 200, p1));
        rules.push(Rule::new("/api/clip/p2", 200, p2));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let clips = pollster::block_on(client.get_clips_by_ids(&http, &["p1", "p2"], 4)).unwrap();
        assert_eq!(
            clips.len(),
            2,
            "infill and upload ancestors must not be filtered"
        );
        assert_eq!(clips[0].id, "p1");
        assert_eq!(clips[1].id, "p2");
    }

    #[test]
    fn get_clips_by_ids_returns_a_trashed_clip() {
        // A trashed ancestor must still be retrievable by id (the v2 `?ids=`
        // capability that per-id `/api/clip/{id}` replaces).
        let trashed = serde_json::json!({
            "id": "t1", "title": "Trashed Ancestor", "status": "complete",
            "is_trashed": true, "metadata": {"type": "gen"}
        })
        .to_string();
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/clip/t1", 200, trashed));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let clips = pollster::block_on(client.get_clips_by_ids(&http, &["t1"], 4)).unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].id, "t1");
        assert!(clips[0].is_trashed);
    }

    #[test]
    fn get_clips_by_ids_skips_a_not_found_id_and_dedupes() {
        let only = serde_json::json!({
            "id": "only", "title": "Bare", "status": "complete", "metadata": {"type": "gen"}
        })
        .to_string();
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/clip/gone", Reply::status(404))
            .route("/api/clip/only", Reply::json(&only));
        let client = scripted_client(&http, RecordingClock::new());

        let clips =
            pollster::block_on(client.get_clips_by_ids(&http, &["only", "gone", "only"], 4))
                .unwrap();
        assert_eq!(clips.len(), 1, "the 404 id is skipped");
        assert_eq!(clips[0].id, "only");
        // "only" is fetched once despite appearing twice; "gone" is attempted once.
        assert_eq!(http.count("/api/clip/only"), 1);
        assert_eq!(http.count("/api/clip/gone"), 1);
    }

    #[test]
    fn get_clips_by_ids_matches_serial_results_and_keeps_order_when_concurrent() {
        let a = serde_json::json!({
            "id": "a", "title": "A", "status": "complete", "metadata": {"type": "gen"}
        })
        .to_string();
        let b = serde_json::json!({
            "id": "b", "title": "B", "status": "complete", "metadata": {"type": "gen"}
        })
        .to_string();
        let c = serde_json::json!({
            "id": "c", "title": "C", "status": "complete", "metadata": {"type": "gen"}
        })
        .to_string();
        let http = ScriptedHttp::new()
            .with_auth()
            .route("/api/clip/a", Reply::json(&a))
            .route("/api/clip/b", Reply::json(&b))
            .route("/api/clip/c", Reply::json(&c));
        let client = scripted_client(&http, RecordingClock::new());
        let ids = ["b", "a", "c", "a"];

        let serial = pollster::block_on(client.get_clips_by_ids(&http, &ids, 1)).unwrap();
        let concurrent = pollster::block_on(client.get_clips_by_ids(&http, &ids, 4)).unwrap();

        let serial_ids: Vec<&str> = serial.iter().map(|clip| clip.id.as_str()).collect();
        let concurrent_ids: Vec<&str> = concurrent.iter().map(|clip| clip.id.as_str()).collect();
        assert_eq!(serial_ids, vec!["b", "a", "c"]);
        assert_eq!(concurrent_ids, serial_ids);
    }

    #[test]
    fn get_clip_parent_reads_the_parent_clip() {
        let parent = serde_json::json!({
            "id": "par", "title": "Ancestor", "status": "complete",
            "metadata": {"type": "gen"}
        })
        .to_string();
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/clips/parent?clip_id=child", 200, parent));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let clip = pollster::block_on(client.get_clip_parent(&http, "child")).unwrap();
        assert_eq!(clip.unwrap().id, "par");
    }

    #[test]
    fn get_clip_parent_is_none_for_a_root() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            "/api/clips/parent",
            404,
            r#"{"detail": "no parent"}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let clip = pollster::block_on(client.get_clip_parent(&http, "root")).unwrap();
        assert!(clip.is_none());
    }

    #[test]
    fn get_clip_parent_propagates_server_errors_instead_of_reporting_no_parent() {
        // A transient 5xx must never be mistaken for "this clip is a root":
        // folding it into Ok(None) would fabricate a wrong external root and let
        // a blip rewrite lineage (HARDENING H3). Only a real 404 means no parent.
        for status in [500u16, 503] {
            let mut rules = auth_rules();
            rules.push(Rule::new(
                "/api/clips/parent",
                status,
                r#"{"detail": "server error"}"#.to_string(),
            ));
            let http = MockHttp::new(rules);
            let client = authed_client(&http);

            let result = pollster::block_on(client.get_clip_parent(&http, "child"));
            assert!(
                matches!(result, Err(Error::Api(_))),
                "status {status} must propagate as an error, not Ok(None)"
            );
        }
    }

    #[test]
    fn get_playlists_maps_entries_and_skips_missing_ids() {
        let page1 = serde_json::json!({
            "playlists": [
                {"id": "pl1", "name": "Road Trip", "num_total_results": 12},
                {"id": "", "name": "No Id", "num_total_results": 3},
                {"name": "Also No Id"}
            ]
        })
        .to_string();
        let mut rules = auth_rules();
        // Page 1 returns entries; page 2 is empty, ending pagination.
        rules.push(Rule::new("/api/playlist/me?page=1", 200, page1));
        rules.push(Rule::new(
            "/api/playlist/me?page=2",
            200,
            r#"{"playlists": []}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let playlists = pollster::block_on(client.get_playlists(&http)).unwrap();
        assert_eq!(playlists.len(), 1, "entries without an id are dropped");
        assert_eq!(
            playlists[0],
            Playlist {
                id: "pl1".to_owned(),
                name: "Road Trip".to_owned(),
                num_clips: 12,
            }
        );
    }

    #[test]
    fn get_playlists_defaults_a_missing_name_to_untitled() {
        let page1 = serde_json::json!({
            "playlists": [{"id": "pl9", "num_total_results": 1}]
        })
        .to_string();
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/playlist/me?page=1", 200, page1));
        rules.push(Rule::new(
            "/api/playlist/me?page=2",
            200,
            r#"{"playlists": []}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let playlists = pollster::block_on(client.get_playlists(&http)).unwrap();
        assert_eq!(playlists[0].name, "Untitled");
    }

    #[test]
    fn get_playlist_clips_preserves_order_and_unwraps_clip() {
        // Members arrive wrapped under `clip`, in playlist order, already
        // non-trashed. Order is preserved and no downloadability filter is applied.
        let body = serde_json::json!({
            "num_total_results": 2,
            "playlist_clips": [
                {"clip": {
                    "id": "second", "title": "Second", "status": "complete",
                    "metadata": {"duration": 60.0, "type": "gen"}
                }},
                {"clip": {
                    "id": "first", "title": "First", "status": "complete",
                    "metadata": {"duration": 30.0, "task": "infill", "type": "gen"}
                }}
            ]
        })
        .to_string();
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/playlist/pl1/", 200, body));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let (clips, complete) =
            pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
        assert_eq!(clips.len(), 2, "an infill member is not filtered out");
        assert_eq!(clips[0].id, "second");
        assert_eq!(clips[1].id, "first");
        assert!(
            complete,
            "returned == num_total_results is fully enumerated"
        );
    }

    #[test]
    fn get_playlist_clips_short_page_is_not_complete() {
        // A page with fewer entries than num_total_results is not authoritative.
        let body = serde_json::json!({
            "num_total_results": 5,
            "playlist_clips": [
                {"clip": {
                    "id": "only", "title": "Only", "status": "complete",
                    "metadata": {"duration": 60.0, "type": "gen"}
                }}
            ]
        })
        .to_string();
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/playlist/pl1/", 200, body));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let (clips, complete) =
            pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
        assert_eq!(clips.len(), 1);
        assert!(!complete, "a short page is not fully enumerated");
    }

    #[test]
    fn get_playlist_clips_is_empty_for_a_playlist_with_no_members() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            "/api/playlist/empty/",
            200,
            r#"{"num_total_results": 0, "playlist_clips": []}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let (clips, complete) =
            pollster::block_on(client.get_playlist_clips(&http, "empty")).unwrap();
        assert!(clips.is_empty());
        assert!(
            complete,
            "an empty playlist reporting zero total is complete"
        );
    }

    #[test]
    fn get_playlist_clips_missing_total_is_not_complete() {
        // A body without num_total_results cannot be verified as whole, so it is
        // never authoritative -- an empty or malformed page must not let a Mirror
        // area delete from it (D5).
        let mut rules = auth_rules();
        rules.push(Rule::new(
            "/api/playlist/pl1/",
            200,
            r#"{"playlist_clips": []}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let (clips, complete) =
            pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
        assert!(clips.is_empty());
        assert!(!complete, "a missing total is never fully enumerated");
    }

    /// A stems page body: each stem is a full clip object whose title carries
    /// the label in a trailing parenthetical, as the live endpoint returns.
    fn stem_page(stems: &[(&str, &str, &str)]) -> String {
        let entries: Vec<Value> = stems
            .iter()
            .map(|(id, label, url)| {
                serde_json::json!({
                    "id": id,
                    "title": format!("My Song ({label})"),
                    "status": "complete",
                    "audio_url": url,
                })
            })
            .collect();
        serde_json::json!({ "stems": entries }).to_string()
    }

    /// The page-count body for `GET /api/clip/{id}/stems/pages`.
    fn stem_pages(pages: u32) -> String {
        serde_json::json!({ "pages": pages }).to_string()
    }

    #[test]
    fn list_stems_drains_all_declared_pages_and_is_authoritative() {
        // Two 0-indexed pages, both drained: the stems concatenate in order and
        // the listing is authoritative (it declared its pages and held stems).
        let http = ScriptedHttp::new()
            .with_auth()
            .route("stems/pages", Reply::json(&stem_pages(2)))
            .route(
                "stems?page=0",
                Reply::json(&stem_page(&[
                    ("s1", "Vocals", "https://cdn1.suno.ai/s1.mp3"),
                    ("s2", "Drums", "https://cdn1.suno.ai/s2.mp3"),
                ])),
            )
            .route(
                "stems?page=1",
                Reply::json(&stem_page(&[("s3", "Bass", "https://cdn1.suno.ai/s3.mp3")])),
            );
        let client = scripted_client(&http, RecordingClock::new());

        let (stems, complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();
        assert_eq!(stems.len(), 3);
        assert_eq!(stems[0].id, "s1");
        assert_eq!(stems[0].label, "Vocals");
        assert_eq!(stems[0].url, "https://cdn1.suno.ai/s1.mp3");
        assert_eq!(stems[2].label, "Bass");
        assert!(
            complete,
            "a fully drained listing that returned stems is authoritative"
        );
    }

    #[test]
    fn list_stems_zero_pages_is_indeterminate_never_empty() {
        // A clip with no stems answers `{"pages": 0}`. That must NOT be read as an
        // authoritative empty set, or it could delete local stems.
        let http = ScriptedHttp::new()
            .with_auth()
            .route("stems/pages", Reply::json(&stem_pages(0)));
        let client = scripted_client(&http, RecordingClock::new());

        let (stems, complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();
        assert!(stems.is_empty());
        assert!(
            !complete,
            "an empty listing is indeterminate, so existing stems are kept"
        );
    }

    #[test]
    fn list_stems_missing_page_count_is_indeterminate() {
        // A `400`/`404` on the page-count endpoint (Suno's "no stems" answer) is
        // indeterminate, never an authoritative empty set.
        for status in [400u16, 404] {
            let http = ScriptedHttp::new()
                .with_auth()
                .route("stems/pages", Reply::status(status));
            let client = scripted_client(&http, RecordingClock::new());
            let (stems, complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();
            assert!(stems.is_empty(), "status {status}");
            assert!(!complete, "status {status} is indeterminate, not empty");
        }
    }

    #[test]
    fn stem_page_count_400_is_no_stems() {
        // A genuine `400` on the page-count endpoint means "no stems": it must
        // produce ([], false) — indeterminate, not an authoritative empty set.
        let http = ScriptedHttp::new()
            .with_auth()
            .route("stems/pages", Reply::status(400));
        let client = scripted_client(&http, RecordingClock::new());

        let (stems, complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();
        assert!(stems.is_empty());
        assert!(
            !complete,
            "400 is indeterminate, not an authoritative empty set"
        );
    }

    #[test]
    fn stem_page_count_5xx_with_invalid_page_body_is_not_no_stems() {
        // A `5xx` whose body happens to contain "Invalid page" must NOT be
        // classified as "no stems": body-text matching would misclassify it.
        // Only a genuine `400` status triggers the no-stems path.
        let http = ScriptedHttp::new()
            .with_auth()
            .route("stems/pages", Reply::with_body(500, "Invalid page"));
        let client = scripted_client(&http, RecordingClock::new());

        let result = pollster::block_on(client.list_stems(&http, "clip1"));
        assert!(
            result.is_err(),
            "a 5xx is a transient error, never 'no stems'"
        );
    }

    #[test]
    fn list_stems_page_error_mid_enumeration_propagates() {
        // A transient 5xx on a page mid-drain is indeterminate, not an end: it
        // surfaces as an error rather than a (partial) authoritative set, so the
        // caller keeps existing stems.
        let http = ScriptedHttp::new()
            .with_auth()
            .route("stems/pages", Reply::json(&stem_pages(2)))
            .route(
                "stems?page=0",
                Reply::json(&stem_page(&[(
                    "s1",
                    "Vocals",
                    "https://cdn1.suno.ai/s1.mp3",
                )])),
            )
            .route("stems?page=1", Reply::status(500));
        let client = scripted_client(&http, RecordingClock::new());

        let result = pollster::block_on(client.list_stems(&http, "clip1"));
        assert!(result.is_err(), "a 5xx page is not a clean drain");
    }

    #[test]
    fn list_stems_over_max_pages_is_truncated_never_authoritative() {
        // A clip that declares more pages than the `MAX_PAGES` cap can only be
        // drained partially, so even though the fetched pages hold stems the
        // listing is TRUNCATED and must not be authoritative: its un-fetched
        // stems on pages beyond the cap would otherwise be delete-reconciled.
        let http = ScriptedHttp::new()
            .with_auth()
            .route("stems/pages", Reply::json(&stem_pages(MAX_PAGES + 1)))
            .route(
                "stems?page=",
                Reply::json(&stem_page(&[(
                    "s1",
                    "Vocals",
                    "https://cdn1.suno.ai/s1.mp3",
                )])),
            );
        let client = scripted_client(&http, RecordingClock::new());

        let (stems, complete) = pollster::block_on(client.list_stems(&http, "clip1")).unwrap();
        assert!(!stems.is_empty(), "the fetched pages still yield stems");
        assert!(
            !complete,
            "a listing declaring more than MAX_PAGES is truncated, never authoritative"
        );
    }

    #[test]
    fn parse_stems_page_maps_full_clips_and_skips_idless() {
        // A stem is a full clip: id, label from the title parenthetical, and the
        // public CDN MP3 url.
        let page = stem_page(&[("x", "Backing Vocals", "https://cdn1.suno.ai/x.mp3")]);
        let stems = parse_stems_page(page.as_bytes());
        assert_eq!(stems.len(), 1);
        assert_eq!(stems[0].id, "x");
        assert_eq!(stems[0].label, "Backing Vocals");
        assert_eq!(stems[0].url, "https://cdn1.suno.ai/x.mp3");
        // An entry with no id cannot be keyed or WAV-rendered and is dropped.
        let no_id = br#"{"stems": [{"title": "Ghost (Vocals)", "audio_url": "https://cdn1.suno.ai/g.mp3"}]}"#;
        assert!(parse_stems_page(no_id).is_empty());
        // A stem with an id but no audio_url still resolves a deterministic CDN
        // url from its id, so it remains downloadable.
        let no_url = br#"{"stems": [{"id": "y", "title": "Song (Bass)"}]}"#;
        let recovered = parse_stems_page(no_url);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].url, "https://cdn1.suno.ai/y.mp3");
        // Malformed JSON never panics; it yields no stems.
        assert!(parse_stems_page(b"not json").is_empty());
    }

    #[test]
    fn parse_stem_page_count_reads_pages_field() {
        assert_eq!(parse_stem_page_count(br#"{"pages": 12}"#), 12);
        assert_eq!(parse_stem_page_count(br#"{"pages": 0}"#), 0);
        // Missing, negative, or non-numeric pages read as 0 (indeterminate).
        assert_eq!(parse_stem_page_count(br#"{}"#), 0);
        assert_eq!(parse_stem_page_count(br#"{"pages": -1}"#), 0);
        assert_eq!(parse_stem_page_count(b"not json"), 0);
    }

    #[test]
    fn stem_label_from_title_extracts_trailing_parenthetical() {
        assert_eq!(stem_label_from_title("My Song (Vocals)"), "Vocals");
        assert_eq!(
            stem_label_from_title("A (b) Song (Backing Vocals)"),
            "Backing Vocals"
        );
        assert_eq!(stem_label_from_title("My Song (Drums) "), "Drums");
        // No parenthetical: empty, so the caller falls back to the stem id.
        assert_eq!(stem_label_from_title("My Song"), "");
        assert_eq!(stem_label_from_title(""), "");
    }

    #[test]
    fn post_allow_list_permits_only_feed_and_wav_render() {
        assert!(post_path_allowed(FEED_V3_PATH));
        assert!(post_path_allowed("/api/gen/abc123/convert_wav/"));
        // No generation endpoint is on the list.
        assert!(!post_path_allowed("/api/gen/abc123/stem_task"));
        assert!(!post_path_allowed("/api/gen/abc123/separate"));
        // Path traversal or extra segments can't smuggle a match.
        assert!(!post_path_allowed("/api/gen/a/../evil/convert_wav/"));
        assert!(!post_path_allowed("/api/gen/a/b/convert_wav/"));
        // The stems endpoints are GET-only and never on the POST allow-list.
        assert!(!post_path_allowed("/api/clip/x/stems/pages"));
        assert!(!post_path_allowed("/api/clip/x/stems?page=0"));
    }

    #[test]
    fn api_request_refuses_a_post_off_the_allow_list() {
        // The single POST chokepoint rejects an off-list POST before the wire, so
        // a credit-spending endpoint can never be reached by accident.
        let http = MockHttp::new(auth_rules());
        let client = authed_client(&http);
        let err = pollster::block_on(client.api_request(
            &http,
            Method::Post,
            "/api/gen/x/stem_task",
            b"{}".to_vec(),
        ))
        .unwrap_err();
        assert!(matches!(err, Error::Refused(_)));
    }
}
