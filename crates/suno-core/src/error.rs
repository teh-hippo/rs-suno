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
    /// The Suno API rate-limited the request.
    #[error("rate limited")]
    RateLimited,
    /// The config file could not be parsed or failed validation.
    #[error("config error: {0}")]
    Config(String),
}

/// A `Result` whose error is the engine [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
