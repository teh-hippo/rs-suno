//! Core engine for rs-suno: library selection, sync reconciliation, and tagging.
//!
//! Runtime-agnostic and free of direct IO. Network access happens through the
//! [`Http`] port, which a CLI adapter implements, so the engine stays testable
//! in isolation.

mod auth;
mod client;
mod clock;
pub mod config;
mod consts;
mod error;
mod executor;
mod extras;
mod ffmpeg;
mod fs;
mod hash;
mod http;
mod manifest;
mod model;
mod naming;
pub mod reconcile;
pub mod select;
mod tag;

#[cfg(test)]
mod testutil;

#[cfg(test)]
mod sync_chaos;

pub use auth::ClerkAuth;
pub use client::SunoClient;
pub use clock::Clock;
pub use config::{
    AccountConfig, AudioFormat, Config, Defaults, EffectiveSettings, FlagOverrides, SourceConfig,
};
pub use error::{Error, Result};
pub use executor::{ExecOptions, ExecOutcome, Failure, Ports, RunStatus, execute};
pub use extras::{IndexEntry, M3u8Entry, render_clip_sidecar, render_library_index, render_m3u8};
pub use ffmpeg::{Ffmpeg, FfmpegError};
pub use fs::{FileStat, Filesystem, FsError};
pub use hash::{art_hash, meta_hash};
pub use http::{Http, HttpRequest, HttpResponse, Method, TransportError};
pub use manifest::{Manifest, ManifestEntry};
pub use model::Clip;
pub use naming::{
    AlbumMode, CharacterSet, DEFAULT_TEMPLATE, NamingConfig, NamingRequest, RenderedName,
    derive_album, render_clip_name, render_clip_names,
};
pub use reconcile::{Action, Desired, LocalFile, Plan, SourceMode, SourceStatus, reconcile};
pub use tag::{TrackMetadata, tag_flac, tag_mp3};
