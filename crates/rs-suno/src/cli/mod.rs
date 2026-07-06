//! The CLI layer: argument parsing, the sync/copy/check engine, output and log
//! rendering, and the individual command handlers.
//!
//! `suno-core` stays IO-free; everything that touches the network, the clock,
//! the filesystem, or the terminal lives here and drives the pure engine through
//! its ports.

pub mod account;
pub mod areas;
pub mod args;
pub mod commands;
pub mod config_load;
pub mod desired;
pub mod execute;
pub mod expiry;
pub mod failure;
pub mod last_run;
pub mod logs;
pub mod open_url;
pub mod output;
pub mod prompt;
pub mod run;
pub mod signal;
pub mod stems;
pub mod synced_lyrics;
pub mod task_output;
pub mod token;
pub mod wallclock;
