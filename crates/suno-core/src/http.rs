//! The HTTP port: the engine's only window to the network.
//!
//! The engine builds [`HttpRequest`]s and reads [`HttpResponse`]s but never
//! performs IO itself. A CLI adapter implements [`Http`] with a real client,
//! which keeps the engine testable with a simple in-memory double.

use std::future::Future;

/// The HTTP method for a request. Clerk and Suno only need these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// A request the engine wants an adapter to perform.
///
/// `body` is empty for GET and for bodyless POSTs (the Clerk token mint); an
/// adapter sends it only when non-empty.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// A bare GET for a public (unauthenticated) URL: no headers, no token.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A POST carrying `body` (an empty `body` is a bodyless POST).
    pub fn post(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: Method::Post,
            url: url.into(),
            headers: Vec::new(),
            body,
        }
    }
}

/// The response an adapter returns to the engine.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Read a header value by case-insensitive name, if present.
    ///
    /// The download executor uses this for `Content-Length` (provider-reported
    /// size) and `Retry-After` (rate-limit backoff), so the lookup must ignore
    /// header-name casing the way HTTP does.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A failure to complete a request at the transport level.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(pub String);

/// The HTTP port an adapter implements for the engine.
///
/// `Sync` so engine code can hold a shared `&impl Http` across an `.await` and
/// keep the resulting future `Send` (`&T: Send` requires `T: Sync`).
pub trait Http: Sync {
    /// Perform `request` and return the response, or a [`TransportError`].
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send;
}
