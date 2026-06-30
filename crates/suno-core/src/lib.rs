//! Core engine for rs-suno: library selection, sync reconciliation, and tagging.
//!
//! Runtime-agnostic and free of direct IO. Network access happens through the
//! [`Http`] port, which a CLI adapter implements, so the engine stays testable
//! in isolation.

mod auth;
mod client;
mod consts;
mod error;
mod http;
mod model;

#[cfg(test)]
mod testutil;

pub use auth::ClerkAuth;
pub use client::SunoClient;
pub use error::{Error, Result};
pub use http::{Http, HttpRequest, HttpResponse, Method, TransportError};
pub use model::Clip;
