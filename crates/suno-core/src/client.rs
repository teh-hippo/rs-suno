//! The Suno API client: lists the library behind the [`Http`](crate::Http) port.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::Instant;

use futures_util::stream::{self, StreamExt};
use serde_json::Value;

use crate::auth::ClerkAuth;
use crate::backoff::{backoff_delay, retry_after};
use crate::clock::Clock;
use crate::consts::{
    API_MAX_RETRIES, BILLING_INFO_PATH, CLIP_PARENT_PATH, FEED_INITIAL_RATE, FEED_PAGE_SIZE,
    FEED_V3_PATH, GET_SONGS_BY_IDS_PATH, GET_SONGS_CHUNK, MAX_PAGES, PLAYLIST_ME_PATH,
    PLAYLIST_PATH, SUNO_API_BASE_URL,
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

/// The authenticated account's billing snapshot: credits, quota, account
/// status, plan identity, and entitlements.
///
/// Every field is optional so a drifting payload never fails the parse; an
/// absent field reads as "unknown", not zero. Numbers are signed because the
/// API returns negatives (e.g. the `-1` sentinel), and `features` is a plain
/// string set rather than an enum so new entitlement flags surface without a
/// code change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BillingInfo {
    /// Credits remaining in the current billing state.
    pub total_credits_left: Option<i64>,
    /// Monthly credit allotment (the quota denominator).
    pub monthly_limit: Option<i64>,
    /// Credits consumed this period (the quota numerator).
    pub monthly_usage: Option<i64>,
    /// Add-on, non-monthly credit balance.
    pub credits: Option<i64>,
    /// Billing period unit, e.g. `"month"`.
    pub period: Option<String>,
    /// Current period end (ISO8601), when usage resets.
    pub period_end: Option<String>,
    /// Next renewal (ISO8601).
    pub renews_on: Option<String>,
    /// Whether the subscription is active.
    pub is_active: Option<bool>,
    /// Whether the subscription is paused (paused subs stop refreshing credits).
    pub is_paused: Option<bool>,
    /// Whether payment is failing (credits may stop refreshing).
    pub is_past_due: Option<bool>,
    /// Whether the subscription is gifted.
    pub is_gifted: Option<bool>,
    /// Subscription platform, e.g. `"stripe"`.
    pub subscription_platform: Option<String>,
    /// Stable machine key for the plan tier, e.g. `"pro"`.
    pub plan_key: Option<String>,
    /// Human plan label, e.g. `"Pro Plan"`.
    pub plan_name: Option<String>,
    /// Plan tier rank (free 0, pro 10, premier 30).
    pub plan_level: Option<i64>,
    /// Entitlement flags, the union of `accessible_features[].name` and
    /// `plan.usage_plan_features[].name`.
    pub features: BTreeSet<String>,
}

impl BillingInfo {
    /// Whether the account is entitled to the named feature.
    pub fn has_feature(&self, name: &str) -> bool {
        self.features.contains(name)
    }

    /// Whether the account may separate stems.
    pub fn can_get_stems(&self) -> bool {
        self.has_feature("get_stems")
    }

    /// Whether the account may convert audio to lossless.
    pub fn can_convert_audio(&self) -> bool {
        self.has_feature("convert_audio")
    }
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

/// Deduplicate ids in first-seen order, dropping empties. Shared by the by-id
/// fetch paths so the batch, the fallback, and the returned order all agree.
fn dedup_nonempty<'a>(ids: &[&'a str]) -> Vec<&'a str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    ids.iter()
        .copied()
        .filter(|id| !id.is_empty() && seen.insert(id))
        .collect()
}

/// Parse a `get_songs_by_ids` `{"clips":[…]}` body into clips with a non-empty
/// id. Returns `None` when the body is not valid JSON or lacks a `clips` array,
/// signalling the caller to fall back to per-id fetches. No downloadability
/// filter is applied: these are lineage ancestors, which may be artefacts.
fn parse_songs_batch(body: &[u8]) -> Option<Vec<Clip>> {
    let data: Value = serde_json::from_slice(body).ok()?;
    let clips = data.get("clips")?.as_array()?;
    Some(
        clips
            .iter()
            .map(Clip::from_json)
            .filter(|clip| !clip.id.is_empty())
            .collect(),
    )
}

/// Parse `/api/billing/info/` into the billing snapshot we report in `doctor`.
///
/// Only genuinely invalid JSON bytes fail; any valid JSON value (even a
/// non-object such as `null` or `[]`) degrades to [`BillingInfo::default`].
fn parse_billing_info(body: &[u8]) -> Result<BillingInfo> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid billing JSON: {err}")))?;
    Ok(from_billing_json(&data))
}

/// Map the raw billing JSON into the domain [`BillingInfo`].
///
/// Reads each field independently through `.get()`, defaulting to `None`/empty
/// on a missing key or type mismatch, and never fails on a single field.
/// `features` is the union of `accessible_features[].name` and
/// `plan.usage_plan_features[].name`.
fn from_billing_json(data: &Value) -> BillingInfo {
    let plan = data.get("plan");
    let mut features = BTreeSet::new();
    collect_feature_names(data.get("accessible_features"), &mut features);
    collect_feature_names(
        plan.and_then(|plan| plan.get("usage_plan_features")),
        &mut features,
    );
    BillingInfo {
        total_credits_left: data.get("total_credits_left").and_then(json_i64),
        monthly_limit: data.get("monthly_limit").and_then(json_i64),
        monthly_usage: data.get("monthly_usage").and_then(json_i64),
        credits: data.get("credits").and_then(json_i64),
        period: json_string(data.get("period")),
        period_end: json_string(data.get("period_end")),
        renews_on: json_string(data.get("renews_on")),
        is_active: data.get("is_active").and_then(Value::as_bool),
        is_paused: data.get("is_paused").and_then(Value::as_bool),
        is_past_due: data.get("is_past_due").and_then(Value::as_bool),
        is_gifted: data.get("is_gifted").and_then(Value::as_bool),
        subscription_platform: json_string(data.get("subscription_platform")),
        plan_key: json_string(plan.and_then(|plan| plan.get("plan_key"))),
        plan_name: json_string(plan.and_then(|plan| plan.get("name"))),
        plan_level: plan.and_then(|plan| plan.get("level")).and_then(json_i64),
        features,
    }
}

/// Add the `name` of each `{ "name": ... }` element of a feature array to
/// `out`, skipping non-arrays, non-object elements, and empty or missing names.
fn collect_feature_names(array: Option<&Value>, out: &mut BTreeSet<String>) {
    let Some(items) = array.and_then(Value::as_array) else {
        return;
    };
    for name in items
        .iter()
        .filter_map(|item| item.get("name").and_then(Value::as_str))
    {
        if !name.is_empty() {
            out.insert(name.to_owned());
        }
    }
}

/// Read an optional string field, cloning the value when present.
fn json_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

/// Read a signed integer that Suno may encode as a JSON integer, an integral
/// JSON float (`2450.0`), or a decimal string (`"2450"` or `"2450.0"`).
///
/// Non-integral values (`2450.5`), overflow, and junk yield `None`. The
/// conversion is lossless and never saturates a value into range.
fn json_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().and_then(f64_to_i64)),
        Value::String(text) => str_to_i64(text),
        _ => None,
    }
}

/// Convert a finite, integral `f64` to `i64`, rejecting fractional values and
/// anything outside the exactly representable range.
fn f64_to_i64(value: f64) -> Option<i64> {
    // Beyond 2^53 an f64 cannot losslessly represent an integer: serde has
    // already rounded (or saturated) such a value before we see it, so we
    // refuse rather than return a wrong result. Below 2^53 the cast is exact.
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        Some(value as i64)
    } else {
        None
    }
}

/// Parse a decimal string into `i64`, accepting an all-zero fractional part
/// (`"2450.0"`) but rejecting non-integral values, overflow, and junk.
fn str_to_i64(text: &str) -> Option<i64> {
    match text.split_once('.') {
        Some((integer, fraction)) => {
            let integral = fraction.is_empty() || fraction.bytes().all(|byte| byte == b'0');
            integral.then(|| integer.parse().ok()).flatten()
        }
        None => text.parse().ok(),
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
/// The completeness flag is `true` only when the response's `num_total_results`
/// is present, equals the raw `playlist_clips[]` count, and no member was
/// dropped by the empty-id filter, i.e. the whole member set arrived intact on
/// this single page. It gates a Mirror playlist area's deletion authority (D5):
/// a short or paginated page, or one carrying a member with a missing/empty
/// clip id, cannot be authoritative for deletion, so it returns `false`.
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
    // Completeness requires the reported total to be present and to match the
    // raw entry count (before the empty-id filter) AND no member to have been
    // dropped by that filter (`clips.len() == raw_len`). A missing or malformed
    // total, a short page, or a single dropped member (empty/missing clip id)
    // all fail safe toward "not authoritative", so a Mirror area can never
    // delete from a page whose whole member set was not seen intact.
    let complete = data
        .get("num_total_results")
        .and_then(Value::as_u64)
        .is_some_and(|total| raw_len as u64 == total && clips.len() == raw_len);
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

    /// One real anonymised `POST /api/feed/v3` page (issue #219): a single
    /// downloadable clip carrying `media_urls`, `user_id`, `batch_index`, cdn2
    /// artwork, and a pagination envelope with `has_more`/`next_cursor`.
    const FEED_V3_PAGE: &str = r#"{
      "clips": [
        {
          "status": "complete",
          "title": "Track 31",
          "id": "00000000-0000-4000-8000-000000000076",
          "entity_type": "song_schema",
          "video_url": "",
          "audio_url": "https://cdn1.suno.ai/00000000-0000-4000-8000-000000000076.mp3",
          "media_urls": [
            {
              "url": "https://media.cloudfront.net/1/clip/00000000-0000-4000-8000-000000000076.m4a",
              "content_type": "m4a-opus",
              "delivery": "progressive",
              "encoding": "1.0.0"
            },
            {
              "url": "https://cdn1.suno.ai/00000000-0000-4000-8000-000000000076.mp3",
              "content_type": "mp3",
              "delivery": "progressive"
            }
          ],
          "image_url": "https://cdn2.suno.ai/image_00000000-0000-4000-8000-000000000076.jpeg",
          "image_large_url": "https://cdn2.suno.ai/image_large_00000000-0000-4000-8000-000000000076.jpeg",
          "major_model_version": "v4.5",
          "model_name": "chirp-ahi",
          "metadata": {
            "tags": "",
            "type": "gen",
            "duration": 272.0,
            "task": "gen_stem",
            "has_stem": false
          },
          "is_liked": false,
          "user_id": "00000000-0000-4000-8000-000000000019",
          "display_name": "Example Artist 4",
          "handle": "example-artist-1",
          "is_trashed": false,
          "is_hidden": false,
          "created_at": "2026-07-03T13:15:10.635Z",
          "is_public": false,
          "explicit": false,
          "batch_index": 23,
          "clip_roots": {
            "clips": [
              {
                "id": "00000000-0000-4000-8000-000000000028",
                "title": "Track 7",
                "image_url": "https://cdn2.suno.ai/image_00000000-0000-4000-8000-000000000028.jpeg",
                "is_public": false,
                "user_display_name": "Example Artist 4",
                "user_handle": "example-artist-1",
                "user_avatar_image_url": "https://cdn1.suno.ai/avatar.jpg"
              }
            ],
            "clip_attribution_type": "remix"
          }
        }
      ],
      "has_more": true,
      "next_cursor": "cursor-token"
    }"#;

    #[test]
    fn parse_feed_v3_page_maps_real_body_and_pagination() {
        let (clips, has_more, next_cursor) = parse_feed_v3(FEED_V3_PAGE.as_bytes()).unwrap();
        assert_eq!(has_more, Some(true));
        assert_eq!(next_cursor.as_deref(), Some("cursor-token"));
        // The single gen_stem clip is complete and passes is_downloadable.
        assert_eq!(clips.len(), 1);
        let clip = &clips[0];
        assert_eq!(clip.id, "00000000-0000-4000-8000-000000000076");
        assert_eq!(clip.title, "Track 31");
        assert_eq!(clip.model_name, "chirp-ahi");
        assert_eq!(clip.major_model_version, "v4.5");
        assert_eq!(clip.user_id, "00000000-0000-4000-8000-000000000019");
        assert_eq!(clip.batch_index, Some(23));
        // The cdn2 artwork host is rewritten to cdn1.
        assert_eq!(
            clip.image_url,
            "https://cdn1.suno.ai/image_00000000-0000-4000-8000-000000000076.jpeg"
        );
        assert!(clip.image_large_url.starts_with("https://cdn1.suno.ai/"));
        // media_urls carries both assets; mp3_url prefers the listed mp3.
        assert_eq!(clip.media_urls.len(), 2);
        assert_eq!(clip.media_urls[0].content_type, "m4a-opus");
        assert_eq!(
            clip.mp3_url(),
            "https://cdn1.suno.ai/00000000-0000-4000-8000-000000000076.mp3"
        );
        // A feed clip carries the same nested clip_roots shape as /api/clip/{id}.
        assert_eq!(clip.clip_attribution_type, "remix");
        assert_eq!(clip.clip_roots.len(), 1);
        assert_eq!(
            clip.clip_roots[0].id,
            "00000000-0000-4000-8000-000000000028"
        );
        assert_eq!(clip.clip_roots[0].handle, "example-artist-1");
    }

    #[test]
    fn parse_feed_v3_page_survives_stripped_optional_fields() {
        // A clip with explicit/ownership/clip_roots/media_urls all stripped still
        // parses with sane defaults (the 490/458-of-500 optionality reality).
        let stripped = serde_json::json!({
            "clips": [{
                "id": "bare", "title": "Bare", "status": "complete",
                "metadata": {"type": "gen"}
            }],
            "has_more": false
        })
        .to_string();
        let (clips, has_more, next_cursor) = parse_feed_v3(stripped.as_bytes()).unwrap();
        assert_eq!(has_more, Some(false));
        assert_eq!(next_cursor, None);
        assert_eq!(clips.len(), 1);
        assert!(clips[0].media_urls.is_empty());
        assert_eq!(clips[0].user_id, "");
        assert_eq!(clips[0].batch_index, None);
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
        assert_eq!(billing.total_credits_left, Some(500));
        assert_eq!(billing.monthly_limit, Some(1000));
        assert_eq!(billing.monthly_usage, Some(500));
    }

    #[test]
    fn get_billing_info_tolerates_missing_balance() {
        let mut rules = auth_rules();
        rules.push(Rule::new(
            BILLING_INFO_PATH,
            200,
            r#"{"monthly_usage":12}"#.to_string(),
        ));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let billing = pollster::block_on(client.get_billing_info(&http)).unwrap();
        assert_eq!(billing.total_credits_left, None);
        assert_eq!(billing.monthly_usage, Some(12));
    }

    /// The anonymised full 43-field `GET /api/billing/info/` body from issue
    /// #223, used as a real-shape parse fixture.
    const BILLING_FULL: &str = r#"{
  "subscription_platform": "stripe",
  "is_active": true,
  "is_past_due": false,
  "credits": 0,
  "subscription_type": true,
  "subscription_anchor": "REDACTED",
  "subscription_id": "REDACTED",
  "renews_on": "REDACTED",
  "period": "month",
  "monthly_usage": 50,
  "monthly_limit": 2500,
  "credit_packs": [
    {
      "id": "00000000-0000-4000-8000-000000000001",
      "amount": 500,
      "price_usd": 4
    },
    {
      "id": "00000000-0000-4000-8000-000000000002",
      "amount": 1000,
      "price_usd": 8
    }
  ],
  "plan": {
    "id": "00000000-0000-4000-8000-000000000005",
    "level": 10,
    "plan_key": "pro",
    "name": "Pro Plan",
    "features": "Access to our newest model, v4\n2,500 credits (up to 500 songs), refreshes monthly\nCommercial use rights for songs made while subscribed\nCreate up to 10 songs at once\nEarly access to new features\nPriority creation queue\nAbility to purchase add-on credits",
    "monthly_price_usd": 10.0,
    "annual_price_usd": 96.0,
    "usage_plan_features": [
      {
        "name": "v4"
      },
      {
        "name": "cover"
      },
      {
        "name": "edit_mode"
      },
      {
        "name": "persona"
      },
      {
        "name": "can_buy_credit_top_ups"
      },
      {
        "name": "commercial_rights"
      },
      {
        "name": "get_stems"
      },
      {
        "name": "generate_song_image"
      },
      {
        "name": "auk"
      },
      {
        "name": "negative_tags"
      },
      {
        "name": "remaster"
      },
      {
        "name": "generate_song_video"
      },
      {
        "name": "long_uploads"
      },
      {
        "name": "convert_audio"
      },
      {
        "name": "create_control_sliders"
      },
      {
        "name": "playlist_condition"
      },
      {
        "name": "tag_upsample"
      },
      {
        "name": "custom_models"
      }
    ]
  },
  "models": [
    {
      "can_use": true,
      "max_lengths": {
        "title": 100,
        "prompt": 5000,
        "tags": 1000,
        "negative_tags": 1000,
        "gpt_description_prompt": 3000
      },
      "name": "Example Artist 5",
      "external_key": "chirp-fenix",
      "major_version": 5,
      "description": "[description redacted]",
      "is_default_free_model": false,
      "is_default_model": true,
      "badges": [
        "pro"
      ],
      "model_badges": [
        {
          "display_name": "Example Artist 1",
          "light": {
            "text_color": "000000",
            "background_color": "00000000",
            "border_color": "000000"
          },
          "dark": {
            "text_color": "FFFFFF",
            "background_color": "00000000",
            "border_color": "FFFFFF"
          }
        }
      ],
      "style": {
        "light": {
          "text_color": "FD429C"
        },
        "dark": {
          "text_color": "FD429C"
        }
      },
      "capabilities": [
        "all"
      ],
      "features": [
        "create_control_sliders",
        "tag_upsample",
        "mumble_mode",
        "vox_and_voices",
        "reuse_styles_lyrics"
      ],
      "allowed_condition_combinations": [
        [
          "extend"
        ],
        [
          "cover"
        ],
        [
          "infill"
        ],
        [
          "persona"
        ],
        [
          "persona",
          "extend"
        ],
        [
          "persona",
          "cover"
        ],
        [
          "playlist"
        ],
        [
          "underpaint"
        ],
        [
          "overpaint"
        ],
        [
          "vox"
        ],
        [
          "vox",
          "extend"
        ],
        [
          "vox",
          "cover"
        ],
        [
          "vox",
          "playlist"
        ],
        [
          "persona",
          "infill"
        ],
        [
          "cover",
          "infill"
        ]
      ],
      "id": "00000000-0000-4000-8000-000000000006"
    }
  ],
  "plan_price": 10.0,
  "plan_currency": "AUD",
  "plan_currency_price": 15.0,
  "payment_method_type": "card",
  "can_upgrade_immediately": true,
  "plans": [
    {
      "id": "00000000-0000-4000-8000-000000000015",
      "level": 0,
      "plan_key": "free",
      "name": "Free Plan",
      "features": "50 credits renew daily (10 songs)\nCreate up to 4 songs at once\nNo commercial use\nNo credit top ups\nShared generation queue",
      "monthly_price_usd": 0.0,
      "annual_price_usd": 0.0,
      "usage_plan_features": [
        {
          "name": "tag_upsample"
        }
      ],
      "prices": []
    }
  ],
  "accessible_features": [
    {
      "name": "v4"
    },
    {
      "name": "cover"
    },
    {
      "name": "edit_mode"
    },
    {
      "name": "persona"
    },
    {
      "name": "can_buy_credit_top_ups"
    },
    {
      "name": "commercial_rights"
    },
    {
      "name": "get_stems"
    },
    {
      "name": "generate_song_image"
    },
    {
      "name": "auk"
    },
    {
      "name": "negative_tags"
    },
    {
      "name": "remaster"
    },
    {
      "name": "generate_song_video"
    },
    {
      "name": "long_uploads"
    },
    {
      "name": "convert_audio"
    },
    {
      "name": "create_control_sliders"
    },
    {
      "name": "playlist_condition"
    },
    {
      "name": "tag_upsample"
    },
    {
      "name": "custom_models"
    }
  ],
  "revcat_subscriptions_offering_id": "REDACTED",
  "total_credits_left": 2450,
  "free_persona_clips_remaining": 0,
  "free_cover_clips_remaining": 0,
  "free_remasters_remaining": 0,
  "free_mobile_remasters_remaining": 0,
  "free_mobile_v4_gens_remaining": 0,
  "free_web_v4_gens_remaining": 0,
  "free_vox_gens_remaining": 0,
  "has_been_subscriber_before": true,
  "has_valid_school_email": false,
  "has_been_student_subscriber_before": false,
  "day0_boost": -1,
  "promotions": [],
  "audio_upload_limits": {
    "min": 6,
    "max": 1800
  },
  "voice_upload_limits": {
    "min": 10,
    "max": 900
  },
  "voice_record_limits": {
    "min": 10,
    "max": 240
  },
  "period_end": "REDACTED",
  "remaster_model_types": [
    {
      "name": "Example Artist 5",
      "external_key": "chirp-flounder",
      "is_default_model": true,
      "can_use": false
    },
    {
      "name": "Example Artist 2",
      "external_key": "chirp-carp",
      "is_default_model": false,
      "can_use": false
    },
    {
      "name": "v4.5+",
      "external_key": "chirp-bass",
      "is_default_model": false,
      "can_use": false
    }
  ],
  "is_pause_scheduled": false,
  "is_paused": false,
  "is_gifted": false
}"#;

    #[test]
    fn parse_billing_info_reads_full_real_body() {
        let billing = parse_billing_info(BILLING_FULL.as_bytes()).unwrap();
        assert_eq!(billing.total_credits_left, Some(2450));
        assert_eq!(billing.monthly_limit, Some(2500));
        assert_eq!(billing.monthly_usage, Some(50));
        assert_eq!(billing.credits, Some(0));
        assert_eq!(billing.period.as_deref(), Some("month"));
        assert_eq!(billing.is_active, Some(true));
        assert_eq!(billing.is_paused, Some(false));
        assert_eq!(billing.is_past_due, Some(false));
        assert_eq!(billing.is_gifted, Some(false));
        assert_eq!(billing.subscription_platform.as_deref(), Some("stripe"));
        assert_eq!(billing.plan_key.as_deref(), Some("pro"));
        assert_eq!(billing.plan_name.as_deref(), Some("Pro Plan"));
        assert_eq!(billing.plan_level, Some(10));
        assert!(billing.can_get_stems());
        assert!(billing.can_convert_audio());
        assert!(billing.has_feature("custom_models"));
    }

    #[test]
    fn json_i64_reads_string_encoded_integer() {
        let billing = parse_billing_info(br#"{"total_credits_left":"2450"}"#).unwrap();
        assert_eq!(billing.total_credits_left, Some(2450));
    }

    #[test]
    fn json_i64_reads_integral_float() {
        let billing = parse_billing_info(br#"{"total_credits_left":2450.0}"#).unwrap();
        assert_eq!(billing.total_credits_left, Some(2450));
    }

    #[test]
    fn json_i64_reads_negative_sentinel() {
        let billing = parse_billing_info(br#"{"total_credits_left":-1}"#).unwrap();
        assert_eq!(billing.total_credits_left, Some(-1));
    }

    #[test]
    fn json_i64_rejects_non_integral_float_but_object_still_parses() {
        let billing =
            parse_billing_info(br#"{"total_credits_left":2450.5,"period":"month"}"#).unwrap();
        assert_eq!(billing.total_credits_left, None);
        assert_eq!(billing.period.as_deref(), Some("month"));
    }

    #[test]
    fn str_to_i64_handles_encodings_and_junk() {
        assert_eq!(str_to_i64("2450"), Some(2450));
        assert_eq!(str_to_i64("2450.0"), Some(2450));
        assert_eq!(str_to_i64("-1"), Some(-1));
        assert_eq!(str_to_i64("2450.5"), None);
        assert_eq!(str_to_i64(".5"), None);
        assert_eq!(str_to_i64("nope"), None);
        assert_eq!(str_to_i64("99999999999999999999999"), None);
    }

    #[test]
    fn json_i64_rejects_overflow() {
        let billing =
            parse_billing_info(br#"{"total_credits_left":99999999999999999999999}"#).unwrap();
        assert_eq!(billing.total_credits_left, None);
    }

    #[test]
    fn json_i64_covers_i64_and_float_boundaries() {
        // Integers arrive through the lossless i64 path, so the full i64 range works.
        assert_eq!(json_i64(&serde_json::json!(i64::MAX)), Some(i64::MAX));
        assert_eq!(json_i64(&serde_json::json!(i64::MIN)), Some(i64::MIN));
        // A JSON integer of 2^63 exceeds i64::MAX and must not saturate.
        assert_eq!(
            json_i64(&serde_json::json!(9_223_372_036_854_775_808_u64)),
            None
        );
        // Floats are trusted only below 2^53, so both i64 extremes are rejected.
        assert_eq!(f64_to_i64(i64::MAX as f64), None);
        assert_eq!(f64_to_i64(i64::MIN as f64), None);
        assert_eq!(f64_to_i64(2450.5), None);
        assert_eq!(f64_to_i64(f64::NAN), None);
        assert_eq!(f64_to_i64(f64::INFINITY), None);
    }

    #[test]
    fn f64_to_i64_rejects_values_below_i64_min() {
        // A float below i64::MIN must not silently saturate to i64::MIN.
        let below_min: f64 = "-9223372036854775809".parse().unwrap();
        assert_eq!(f64_to_i64(below_min), None);
        // The matching string is rejected by the lossless i64 parse.
        assert_eq!(str_to_i64("-9223372036854775809"), None);
        assert_eq!(json_i64(&serde_json::json!("-9223372036854775809")), None);
    }

    #[test]
    fn f64_to_i64_trusts_only_the_safe_integer_range() {
        // 2^53 - 1 is the largest integer an f64 represents exactly.
        assert_eq!(
            f64_to_i64(9_007_199_254_740_991.0),
            Some(9_007_199_254_740_991)
        );
        // 9007199254740993 (2^53 + 1) is not representable, so serde rounds it to
        // 2^53 before we see it; the rounded value must be refused, not returned.
        let rounded: f64 = "9007199254740993".parse().unwrap();
        assert_eq!(rounded, 9_007_199_254_740_992.0);
        assert_eq!(f64_to_i64(rounded), None);
    }

    #[test]
    fn parse_billing_info_defaults_missing_fields() {
        let billing = parse_billing_info(br#"{"monthly_usage":12}"#).unwrap();
        assert_eq!(billing.total_credits_left, None);
        assert_eq!(billing.monthly_usage, Some(12));
        assert_eq!(billing.plan_key, None);
        assert!(billing.features.is_empty());
        assert!(!billing.can_get_stems());
    }

    #[test]
    fn from_billing_json_ignores_surprising_types() {
        // `subscription_type` is a bool despite its name; a numeric field carrying
        // the wrong type must fall back to None rather than panic.
        let value = serde_json::json!({
            "subscription_type": true,
            "total_credits_left": {"unexpected": "object"},
            "is_active": "yes",
        });
        let billing = from_billing_json(&value);
        assert_eq!(billing.total_credits_left, None);
        assert_eq!(billing.is_active, None);
    }

    #[test]
    fn parse_billing_info_treats_non_object_json_as_default() {
        for body in [
            b"null".as_slice(),
            b"[]".as_slice(),
            br#""hello""#.as_slice(),
        ] {
            assert_eq!(parse_billing_info(body).unwrap(), BillingInfo::default());
        }
    }

    #[test]
    fn parse_billing_info_rejects_non_json_bytes() {
        let err = parse_billing_info(b"nope").unwrap_err();
        assert!(err.to_string().contains("invalid billing JSON"));
    }

    #[test]
    fn from_billing_json_unions_feature_sources() {
        let accessible_only = serde_json::json!({
            "accessible_features": [{"name": "get_stems"}],
        });
        assert!(from_billing_json(&accessible_only).can_get_stems());

        let plan_only = serde_json::json!({
            "plan": {"usage_plan_features": [{"name": "convert_audio"}]},
        });
        assert!(from_billing_json(&plan_only).can_convert_audio());

        let both = serde_json::json!({
            "accessible_features": [{"name": "get_stems"}, {"name": ""}, {"other": "x"}],
            "plan": {"usage_plan_features": [{"name": "convert_audio"}]},
        });
        let billing = from_billing_json(&both);
        assert!(billing.can_get_stems());
        assert!(billing.can_convert_audio());
        // Empty and malformed feature entries are ignored.
        assert_eq!(billing.features.len(), 2);
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
    fn get_clips_by_ids_keeps_infill_and_upload_ancestors() {
        // The gap-fill path must not apply the listing's downloadability filter:
        // an infill ancestor and an upload root both survive, returned by the
        // batch `get_songs_by_ids` call.
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
        let batch = format!(r#"{{"clips":[{p1},{p2}]}}"#);
        let mut rules = auth_rules();
        rules.push(Rule::new("get_songs_by_ids", 200, batch));
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
        // capability that `get_songs_by_ids` now restores in one request).
        let trashed = serde_json::json!({
            "id": "t1", "title": "Trashed Ancestor", "status": "complete",
            "is_trashed": true, "metadata": {"type": "gen"}
        })
        .to_string();
        let batch = format!(r#"{{"clips":[{trashed}]}}"#);
        let mut rules = auth_rules();
        rules.push(Rule::new("get_songs_by_ids", 200, batch));
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
        // The batch returns "only" and omits "gone"; "gone" then falls back to a
        // per-id fetch that 404s and is skipped.
        let batch = format!(r#"{{"clips":[{only}]}}"#);
        let http = ScriptedHttp::new()
            .with_auth()
            .route("get_songs_by_ids", Reply::json(&batch))
            .route("/api/clip/gone", Reply::status(404));
        let client = scripted_client(&http, RecordingClock::new());

        let clips =
            pollster::block_on(client.get_clips_by_ids(&http, &["only", "gone", "only"], 4))
                .unwrap();
        assert_eq!(clips.len(), 1, "the 404 id is skipped");
        assert_eq!(clips[0].id, "only");
        // "only" is deduped and returned by the batch, so it is never per-id
        // fetched; "gone" is attempted once via the per-id fallback.
        assert_eq!(
            http.count("get_songs_by_ids"),
            1,
            "one batch call for both ids"
        );
        assert_eq!(http.count("/api/clip/only"), 0);
        assert_eq!(http.count("/api/clip/gone"), 1);
    }

    #[test]
    fn get_clips_by_ids_matches_serial_results_and_keeps_order_when_concurrent() {
        // With no batch route the batch is unavailable, so both calls fall back
        // to per-id and must return the deduped input order regardless of the
        // concurrency used.
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

    /// A minimal complete-clip body for the batch tests below.
    fn clip_body(id: &str) -> String {
        format!(r#"{{"id":"{id}","title":"T","status":"complete","metadata":{{"type":"gen"}}}}"#)
    }

    #[test]
    fn get_songs_by_ids_maps_the_batch_body_matched_by_id_in_input_order() {
        // The batch returns the clips out of order; the result must follow the
        // de-duplicated input order, matched by id, never the response position.
        let batch = format!(
            r#"{{"clips":[{},{},{}]}}"#,
            clip_body("c"),
            clip_body("a"),
            clip_body("b")
        );
        let http = ScriptedHttp::new()
            .with_auth()
            .route("get_songs_by_ids", Reply::json(&batch));
        let client = scripted_client(&http, RecordingClock::new());

        let clips =
            pollster::block_on(client.get_songs_by_ids(&http, &["a", "b", "c", "a"])).unwrap();
        let ids: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"], "input order, not response order");
        assert_eq!(http.count("get_songs_by_ids"), 1, "one chunk, one request");
    }

    #[test]
    fn get_songs_by_ids_drops_clips_that_were_not_requested() {
        // A defensive body carrying an extra id must not leak into the result.
        let batch = format!(r#"{{"clips":[{},{}]}}"#, clip_body("a"), clip_body("x"));
        let http = ScriptedHttp::new()
            .with_auth()
            .route("get_songs_by_ids", Reply::json(&batch));
        let client = scripted_client(&http, RecordingClock::new());

        let clips = pollster::block_on(client.get_songs_by_ids(&http, &["a"])).unwrap();
        let ids: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
        assert_eq!(ids, vec!["a"], "an unrequested id is dropped");
    }

    #[test]
    fn get_songs_by_ids_chunks_ids_beyond_the_chunk_size() {
        // 21 ids span two chunks (20 + 1), one batch request each, with the
        // input order preserved across the chunk boundary.
        let ids: Vec<String> = (0..21).map(|i| format!("id-{i:02}")).collect();
        let body = |slice: &[String]| {
            let clips: Vec<String> = slice.iter().map(|id| clip_body(id)).collect();
            format!(r#"{{"clips":[{}]}}"#, clips.join(","))
        };
        let http = ScriptedHttp::new().with_auth().route_seq(
            "get_songs_by_ids",
            vec![
                Reply::json(&body(&ids[..20])),
                Reply::json(&body(&ids[20..])),
            ],
        );
        let client = scripted_client(&http, RecordingClock::new());
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();

        let clips = pollster::block_on(client.get_songs_by_ids(&http, &refs)).unwrap();
        let got: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
        assert_eq!(got, refs, "all 21 ids returned in input order");
        assert_eq!(
            http.count("get_songs_by_ids"),
            2,
            "two chunks -> two requests"
        );
        let batch_calls: Vec<String> = http
            .calls()
            .into_iter()
            .filter(|url| url.contains("get_songs_by_ids"))
            .collect();
        assert_eq!(
            batch_calls[0].matches("ids=").count(),
            20,
            "first chunk of 20"
        );
        assert_eq!(
            batch_calls[1].matches("ids=").count(),
            1,
            "second chunk of 1"
        );
    }

    #[test]
    fn get_clips_by_ids_batch_first_does_not_fetch_per_id_when_batch_is_complete() {
        // When the batch returns every requested id, no per-id request is made.
        let batch = format!(r#"{{"clips":[{},{}]}}"#, clip_body("a"), clip_body("b"));
        let http = ScriptedHttp::new()
            .with_auth()
            .route("get_songs_by_ids", Reply::json(&batch))
            .route("/api/clip/a", Reply::json(&clip_body("a")))
            .route("/api/clip/b", Reply::json(&clip_body("b")));
        let client = scripted_client(&http, RecordingClock::new());

        let clips = pollster::block_on(client.get_clips_by_ids(&http, &["a", "b"], 4)).unwrap();
        let ids: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(http.count("get_songs_by_ids"), 1);
        assert_eq!(
            http.count("/api/clip/"),
            0,
            "a complete batch needs no per-id fallback"
        );
    }

    #[test]
    fn get_clips_by_ids_fills_ids_the_batch_omits_via_per_id() {
        // The batch returns only "a"; "b" is filled by a per-id fetch.
        let batch = format!(r#"{{"clips":[{}]}}"#, clip_body("a"));
        let http = ScriptedHttp::new()
            .with_auth()
            .route("get_songs_by_ids", Reply::json(&batch))
            .route("/api/clip/b", Reply::json(&clip_body("b")));
        let client = scripted_client(&http, RecordingClock::new());

        let clips = pollster::block_on(client.get_clips_by_ids(&http, &["a", "b"], 4)).unwrap();
        let ids: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "omitted id is filled, order preserved");
        assert_eq!(http.count("/api/clip/a"), 0, "a came from the batch");
        assert_eq!(http.count("/api/clip/b"), 1, "b was filled per-id");
    }

    #[test]
    fn get_clips_by_ids_falls_back_to_per_id_on_a_malformed_batch_body() {
        // A 200 body that is not `{"clips":[…]}` yields nothing for the chunk, so
        // every requested id is recovered by the per-id fallback.
        let http = ScriptedHttp::new()
            .with_auth()
            .route("get_songs_by_ids", Reply::json("not-json{"))
            .route("/api/clip/a", Reply::json(&clip_body("a")))
            .route("/api/clip/b", Reply::json(&clip_body("b")));
        let client = scripted_client(&http, RecordingClock::new());

        let clips = pollster::block_on(client.get_clips_by_ids(&http, &["a", "b"], 4)).unwrap();
        let ids: Vec<&str> = clips.iter().map(|clip| clip.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(http.count("/api/clip/a"), 1);
        assert_eq!(http.count("/api/clip/b"), 1);
    }

    #[test]
    fn get_clips_by_ids_propagates_a_batch_rate_limit_without_per_id_fan_out() {
        // A 429 that survives the retry budget propagates: it must never fan out
        // into a burst of per-id requests that would only deepen the throttling.
        let http = ScriptedHttp::new()
            .with_auth()
            .route("get_songs_by_ids", Reply::status(429))
            .route("/api/clip/a", Reply::json(&clip_body("a")))
            .route("/api/clip/b", Reply::json(&clip_body("b")));
        let client = scripted_client(&http, RecordingClock::new());

        let result = pollster::block_on(client.get_clips_by_ids(&http, &["a", "b"], 4));
        assert!(
            matches!(result, Err(Error::RateLimited { .. })),
            "an exhausted 429 propagates"
        );
        assert_eq!(
            http.count("/api/clip/"),
            0,
            "no per-id fan-out on rate-limit exhaustion"
        );
    }

    #[test]
    fn concurrent_reads_share_aggregate_pacing_after_first_rate_limit() {
        // Batch-first: one `get_songs_by_ids` request (here returning nothing)
        // then four concurrent per-id fallbacks. All five share the 1 req/s
        // aggregate pacing, so from the first to the last reserved slot they span
        // ~4s, with a small tolerance for runtime scheduling jitter.
        const EXPECTED_SPAN: Duration = Duration::from_secs(4);
        const TOLERANCE: Duration = Duration::from_millis(50);
        let ids = ["a", "b", "c", "d"];
        let a =
            serde_json::json!({"id":"a","title":"A","status":"complete","metadata":{"type":"gen"}})
                .to_string();
        let b =
            serde_json::json!({"id":"b","title":"B","status":"complete","metadata":{"type":"gen"}})
                .to_string();
        let c =
            serde_json::json!({"id":"c","title":"C","status":"complete","metadata":{"type":"gen"}})
                .to_string();
        let d =
            serde_json::json!({"id":"d","title":"D","status":"complete","metadata":{"type":"gen"}})
                .to_string();
        let http = ScriptedHttp::new()
            .with_auth()
            .route_seq(
                "/api/feed/v3",
                vec![
                    Reply::status(429),
                    Reply::json(&one_clip_page("seed", None)),
                ],
            )
            .route("get_songs_by_ids", Reply::json(r#"{"clips":[]}"#))
            .route("/api/clip/a", Reply::json(&a))
            .route("/api/clip/b", Reply::json(&b))
            .route("/api/clip/c", Reply::json(&c))
            .route("/api/clip/d", Reply::json(&d));
        let clock = RecordingClock::new();
        let client = scripted_client(&http, clock.clone());
        pollster::block_on(client.list_clips(&http, false, Some(1))).unwrap();
        let before = clock.sleeps().len();

        let clips = pollster::block_on(client.get_clips_by_ids(&http, &ids, ids.len())).unwrap();
        assert_eq!(clips.len(), ids.len());
        let sleeps = clock.sleeps();
        let paced = &sleeps[before..];
        assert_eq!(
            paced.len(),
            ids.len() + 1,
            "one batch call plus four per-id"
        );
        let min = paced.iter().copied().min().unwrap();
        let max = paced.iter().copied().max().unwrap();
        let span = max.saturating_sub(min);
        // After the first 429, rate halves from 2 -> 1 req/s. Under shared slot
        // pacing, the batch call and the four per-id fallbacks are dispatched one
        // second apart in aggregate, so the first-to-last spacing is about four
        // seconds.
        assert!(span >= EXPECTED_SPAN.saturating_sub(TOLERANCE));
        assert!(span <= EXPECTED_SPAN + TOLERANCE);
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
    fn get_clip_parent_is_none_for_a_200_no_id_root() {
        // The live "no parent" contract: HTTP 200 with a bodiless clip that has
        // no id (`{"is_public": false}`), not a 404. parse_clip gates on a
        // non-empty id, so it maps to Ok(None) rather than a bogus edge. Both
        // the bare and `{"clip": ...}`-wrapped encodings must behave the same.
        for body in [
            r#"{"is_public": false}"#,
            r#"{"clip": {"is_public": false}}"#,
        ] {
            let mut rules = auth_rules();
            rules.push(Rule::new("/api/clips/parent", 200, body.to_string()));
            let http = MockHttp::new(rules);
            let client = authed_client(&http);

            let clip = pollster::block_on(client.get_clip_parent(&http, "root")).unwrap();
            assert!(clip.is_none(), "200-no-id body {body:?} must map to None");
        }
    }

    #[test]
    fn get_clip_parent_reads_the_reduced_user_prefixed_shape() {
        // The parent endpoint returns a reduced shape with user_-prefixed
        // identity keys; after the dual-identity mapper fix the parent Clip
        // carries a non-empty display_name/handle (regression pin for #220).
        let parent = serde_json::json!({
            "id": "00000000-0000-4000-8000-000000000020",
            "title": "Track 2",
            "is_public": false,
            "user_display_name": "Example Artist 4",
            "user_handle": "example-artist-1",
            "user_avatar_image_url": "https://cdn1.suno.ai/avatar.jpg"
        })
        .to_string();
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/clips/parent?clip_id=child", 200, parent));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let clip = pollster::block_on(client.get_clip_parent(&http, "child"))
            .unwrap()
            .expect("a parent clip with an id");
        assert_eq!(clip.id, "00000000-0000-4000-8000-000000000020");
        assert_eq!(clip.display_name, "Example Artist 4");
        assert_eq!(clip.handle, "example-artist-1");
        assert_eq!(clip.avatar_image_url, "https://cdn1.suno.ai/avatar.jpg");
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

    #[test]
    fn get_playlist_clips_dropped_member_disarms_authority() {
        // A member whose clip carries no usable id is dropped by the empty-id
        // filter, so clips.len() < raw_len even when raw_len == num_total_results.
        // Both a missing `id` key and an empty-string `id` must disarm deletion
        // authority rather than silently arming a Mirror area on a short set.
        let missing_id = serde_json::json!({
            "num_total_results": 2,
            "playlist_clips": [
                {"clip": {
                    "id": "a", "title": "A", "status": "complete",
                    "metadata": {"duration": 60.0, "type": "gen"}
                }},
                {"clip": {
                    "title": "No Id", "status": "complete",
                    "metadata": {"duration": 30.0, "type": "gen"}
                }}
            ]
        })
        .to_string();
        let empty_id = serde_json::json!({
            "num_total_results": 2,
            "playlist_clips": [
                {"clip": {
                    "id": "a", "title": "A", "status": "complete",
                    "metadata": {"duration": 60.0, "type": "gen"}
                }},
                {"clip": {
                    "id": "", "title": "Empty Id", "status": "complete",
                    "metadata": {"duration": 30.0, "type": "gen"}
                }}
            ]
        })
        .to_string();
        for body in [missing_id, empty_id] {
            let mut rules = auth_rules();
            rules.push(Rule::new("/api/playlist/pl1/", 200, body));
            let http = MockHttp::new(rules);
            let client = authed_client(&http);

            let (clips, complete) =
                pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
            assert_eq!(clips.len(), 1, "the member with no id is dropped");
            assert!(
                !complete,
                "a dropped member disarms authority even when raw_len == total"
            );
        }
    }

    #[test]
    fn get_playlist_clips_over_count_is_not_complete() {
        // total=2 but three raw members (one with an empty id): clips.len()==2
        // matches the total, yet raw_len==3 does not. The two-conjunct gate must
        // reject this; a mis-simplification to `clips.len() == total` would wrongly
        // arm authority here.
        let body = serde_json::json!({
            "num_total_results": 2,
            "playlist_clips": [
                {"clip": {
                    "id": "a", "title": "A", "status": "complete",
                    "metadata": {"duration": 60.0, "type": "gen"}
                }},
                {"clip": {
                    "id": "b", "title": "B", "status": "complete",
                    "metadata": {"duration": 30.0, "type": "gen"}
                }},
                {"clip": {
                    "id": "", "title": "Empty Id", "status": "complete",
                    "metadata": {"duration": 45.0, "type": "gen"}
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
        assert_eq!(clips.len(), 2, "the empty-id member is dropped");
        assert!(
            !complete,
            "raw_len (3) diverging from the total (2) is not authoritative"
        );
    }

    #[test]
    fn get_playlist_clips_ignores_song_count() {
        // The detail reports song_count=0 while num_total_results=1 for the same
        // playlist; completeness must trust num_total_results, so a single-member
        // page reads as complete instead of being compared against song_count.
        let body = serde_json::json!({
            "num_total_results": 1,
            "song_count": 0,
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
        assert!(
            complete,
            "completeness uses num_total_results, not song_count"
        );
    }

    #[test]
    fn get_playlists_num_clips_ignores_song_count() {
        // song_count is unreliable across endpoints (15 in the listing, 0 in the
        // detail), so num_clips must come from num_total_results, never song_count.
        let page1 = serde_json::json!({
            "playlists": [
                {"id": "pl1", "name": "Road Trip", "num_total_results": 15, "song_count": 0}
            ]
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
        assert_eq!(
            playlists[0].num_clips, 15,
            "num_clips reads num_total_results, not song_count"
        );
    }

    #[test]
    fn get_playlists_dedupes_a_page_ignoring_server() {
        // A server that ignores `page` returns the same non-empty body for every
        // page, so the empty-page terminator never fires and MAX_PAGES bounds the
        // loop. Dedupe-by-id keeps the result to the true unique set instead of
        // MAX_PAGES copies.
        let same_body = serde_json::json!({
            "playlists": [
                {"id": "pl1", "name": "Road Trip", "num_total_results": 12},
                {"id": "pl2", "name": "Chill", "num_total_results": 7}
            ]
        })
        .to_string();
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/playlist/me", 200, same_body));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let playlists = pollster::block_on(client.get_playlists(&http)).unwrap();
        assert_eq!(
            playlists.len(),
            2,
            "duplicates from a page-ignoring server are collapsed"
        );
        assert_eq!(playlists[0].id, "pl1");
        assert_eq!(playlists[1].id, "pl2");
    }

    #[test]
    fn get_playlist_clips_preserves_array_order_over_created_at() {
        // relative_index ascends with array order while the wrapper created_at
        // values are non-monotonic. Members must stay in array order: the parser
        // never sorts by created_at (or any timestamp).
        let body = serde_json::json!({
            "num_total_results": 3,
            "playlist_clips": [
                {"clip": {
                    "id": "a", "title": "A", "status": "complete",
                    "metadata": {"duration": 60.0, "type": "gen"}
                }, "relative_index": 1.0, "created_at": "2026-06-08T00:00:00.000Z"},
                {"clip": {
                    "id": "b", "title": "B", "status": "complete",
                    "metadata": {"duration": 30.0, "type": "gen"}
                }, "relative_index": 2.0, "created_at": "2026-01-11T00:00:00.000Z"},
                {"clip": {
                    "id": "c", "title": "C", "status": "complete",
                    "metadata": {"duration": 45.0, "type": "gen"}
                }, "relative_index": 3.0, "created_at": "2026-05-15T00:00:00.000Z"}
            ]
        })
        .to_string();
        let mut rules = auth_rules();
        rules.push(Rule::new("/api/playlist/pl1/", 200, body));
        let http = MockHttp::new(rules);
        let client = authed_client(&http);

        let (clips, complete) =
            pollster::block_on(client.get_playlist_clips(&http, "pl1")).unwrap();
        assert_eq!(
            clips.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"],
            "array order is preserved despite non-monotonic created_at"
        );
        assert!(complete, "three intact members equal the declared total");
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
