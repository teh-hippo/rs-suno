//! Error types for the Suno engine.

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
    /// The Suno API rate-limited the request.
    #[error("rate limited")]
    RateLimited,
    /// Reading or writing audio metadata tags failed.
    #[error("tagging failed: {0}")]
    Tag(String),
    /// The config file could not be parsed or failed validation.
    #[error("config error: {0}")]
    Config(String),
}

/// A `Result` whose error is the engine [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
