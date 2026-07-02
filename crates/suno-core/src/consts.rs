//! Endpoints and tunables for the Suno and Clerk APIs.
//!
//! These mirror the values the Suno web client uses. The API is undocumented,
//! so they may need updating if Suno changes its endpoints.

pub(crate) const SUNO_API_BASE_URL: &str = "https://studio-api-prod.suno.com";
pub(crate) const CLERK_BASE_URL: &str = "https://auth.suno.com";
pub(crate) const CLERK_JS_VERSION: &str = "5.117.0";
pub(crate) const CDN_BASE_URL: &str = "https://cdn1.suno.ai";
/// Canonical public web URL base for a song page (`<base>/<clip id>`). Used in
/// the plain-text details sidecar so a human can open the song in a browser.
pub(crate) const SUNO_SONG_BASE_URL: &str = "https://suno.com/song";

/// Refresh a JWT this many seconds before it expires.
pub(crate) const JWT_REFRESH_BUFFER: i64 = 60;
/// Hard cap on feed pages so a runaway `has_more` cannot loop forever.
pub(crate) const MAX_PAGES: u32 = 100;
/// Clips requested per feed page. A larger page means fewer requests to walk a
/// big library, so the rate limiter is tripped less often.
pub(crate) const FEED_PAGE_SIZE: u32 = 50;
/// Initial adaptive request rate in requests per second, before the limiter
/// discovers Suno's real limit. Equal to a 500ms inter-request pace, matching
/// the previous fixed inter-page delay.
pub(crate) const FEED_INITIAL_RATE: f64 = 2.0;
/// Retry a rate-limited or transient API request this many times before failing.
pub(crate) const API_MAX_RETRIES: u32 = 3;

/// The library feed endpoint: a cursor-paginated `POST` taking
/// `{limit, cursor, filters}` and returning `{clips, has_more, next_cursor}`.
/// The `filters` carry `trashed: "False"` so the listing excludes trashed clips
/// exactly as the old v2 feed did; the `--liked` scope adds `liked: "True"`.
pub(crate) const FEED_V3_PATH: &str = "/api/feed/v3";
/// The dedicated parent-lookup endpoint: one hop up a clip's lineage.
pub(crate) const CLIP_PARENT_PATH: &str = "/api/clips/parent";
/// The caller's own playlists, paged. Trashed and share-list playlists are
/// excluded by query so the listing is the account's authoritative own set.
pub(crate) const PLAYLIST_ME_PATH: &str = "/api/playlist/me";
/// One playlist's detail, including its ordered `playlist_clips`. The id and a
/// trailing slash are appended: `/api/playlist/{id}/`.
pub(crate) const PLAYLIST_PATH: &str = "/api/playlist/";
