//! Configuration model and precedence resolution.
//!
//! Parses a TOML string and merges in environment variables and CLI flag
//! overrides supplied by the caller. Performs no disk or environment IO.
//!
//! [`shape`] holds the parsed input types and their validation, [`resolve`]
//! layers the precedence tiers into [`EffectiveSettings`], and [`effective`]
//! is the resolved output. [`label_to_env`] is the shared env-prefix rule both
//! the validator and the resolver use.

mod effective;
mod resolve;
mod shape;

#[cfg(feature = "schema")]
mod schema;

#[cfg(test)]
mod fixtures;

pub use effective::{EffectiveSettings, FlagOverrides};
#[cfg(feature = "schema")]
pub use schema::config_schema_json;
pub use shape::{AccountConfig, AreaMode, AreasConfig, Config, Defaults, Settings, SourceConfig};

/// Convert an account label to its environment variable prefix, mirroring the
/// per-account keys the resolver reads: `my-lib` becomes `MY_LIB` for lookups
/// like `SUNO_MY_LIB_TOKEN`.
pub fn label_to_env(label: &str) -> String {
    label.to_ascii_uppercase().replace('-', "_")
}
