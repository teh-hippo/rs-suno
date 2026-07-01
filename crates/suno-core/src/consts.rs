//! Endpoints and tunables for the Suno and Clerk APIs.
//!
//! These mirror the values the Suno web client uses. The API is undocumented,
//! so they may need updating if Suno changes its endpoints.

pub(crate) const SUNO_API_BASE_URL: &str = "https://studio-api-prod.suno.com";
pub(crate) const CLERK_BASE_URL: &str = "https://clerk.suno.com";
pub(crate) const CLERK_JS_VERSION: &str = "4.72.1";
pub(crate) const CLERK_TOKEN_JS_VERSION: &str = "4.72.0-snapshot.vc141245";
pub(crate) const CDN_BASE_URL: &str = "https://cdn1.suno.ai";

/// Refresh a JWT this many seconds before it expires.
pub(crate) const JWT_REFRESH_BUFFER: i64 = 60;
/// Hard cap on feed pages so a runaway `has_more` cannot loop forever.
pub(crate) const MAX_PAGES: u32 = 100;

/// The library feed endpoint. Paged for listing, or filtered with `?ids=` to
/// gap-fill specific ancestors (including trashed ones) during lineage
/// resolution.
pub(crate) const FEED_V2_PATH: &str = "/api/feed/v2/";
/// The dedicated parent-lookup endpoint: one hop up a clip's lineage.
pub(crate) const CLIP_PARENT_PATH: &str = "/api/clips/parent";
/// The caller's own playlists, paged. Trashed and share-list playlists are
/// excluded by query so the listing is the account's authoritative own set.
pub(crate) const PLAYLIST_ME_PATH: &str = "/api/playlist/me";
/// One playlist's detail, including its ordered `playlist_clips`. The id and a
/// trailing slash are appended: `/api/playlist/{id}/`.
pub(crate) const PLAYLIST_PATH: &str = "/api/playlist/";
/// Fetch at most this many clip ids per `?ids=` request so a batch cannot build
/// an over-long URL.
pub(crate) const IDS_PER_REQUEST: usize = 40;
