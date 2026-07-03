//! Error types for the Suno engine.

use std::time::Duration;

/// An error raised by the engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The token was rejected, or no session or JWT could be obtained.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// A transport failure talking to Clerk or the Suno API.
    #[error("could not connect: {0}")]
    Connection(String),
    /// The Suno API returned an unexpected status or body.
    #[error("api error: {0}")]
    Api(String),
    /// The Suno API returned `404 Not Found` for the requested resource.
    ///
    /// Distinct from [`Api`](Self::Api) so a caller can treat a genuine absence
    /// (a clip with no parent) as `None` without also swallowing a transient
    /// `5xx`, which must surface as a real error.
    #[error("not found: {0}")]
    NotFound(String),
    /// The Suno API rate-limited the request, with the server's `Retry-After`
    /// hint in whole seconds when it sent one (it usually does not).
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    /// Reading or writing audio metadata tags failed.
    #[error("tagging failed: {0}")]
    Tag(String),
    /// The config file could not be parsed or failed validation.
    #[error("config error: {0}")]
    Config(String),
    /// A request was refused by an engine-side safety guard before it reached
    /// the network. Used by the crate-wide POST allow-list, which rejects any
    /// POST to a path outside the small known-safe set so a mutating request
    /// (above all a credit-spending one) can never be sent by accident.
    #[error("refused: {0}")]
    Refused(String),
}

/// A `Result` whose error is the engine [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
