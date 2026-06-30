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
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// The response an adapter returns to the engine.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// A failure to complete a request at the transport level.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(pub String);

/// The HTTP port an adapter implements for the engine.
pub trait Http {
    /// Perform `request` and return the response, or a [`TransportError`].
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send;
}
