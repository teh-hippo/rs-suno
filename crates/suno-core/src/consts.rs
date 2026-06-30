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
