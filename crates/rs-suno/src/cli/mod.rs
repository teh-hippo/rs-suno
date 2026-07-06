//! The CLI layer: argument parsing, the sync/copy/check engine, output and log
//! rendering, and the individual command handlers.
//!
//! `suno-core` stays IO-free; everything that touches the network, the clock,
//! the filesystem, or the terminal lives here and drives the pure engine through
//! its ports.

pub mod args;
pub mod commands;
pub mod desired;
pub mod expiry;
pub mod failure;
pub mod logs;
pub mod open_url;
pub mod output;
pub mod run;
pub mod task_output;
pub mod token;
pub mod wallclock;
