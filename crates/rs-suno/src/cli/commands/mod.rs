//! The non-engine command handlers: listing, single-clip fetch, config and auth
//! management, version reporting, and shell completions. The sync/copy/check
//! engine lives in [`crate::cli::run`].

pub mod auth;
pub mod completions;
pub mod config;
pub mod fetch;
pub mod ls;
pub mod version;
