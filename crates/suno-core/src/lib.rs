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
mod graph;
mod hash;
mod http;
mod lineage;
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
pub use extras::{M3u8Entry, render_m3u8};
pub use ffmpeg::{Ffmpeg, FfmpegError, WebpEncodeSettings};
pub use fs::{FileStat, Filesystem, FsError};
pub use graph::{AlbumArt, CacheEntry, LineageStore, Node, StoredEdge};
pub use hash::{art_hash, art_url_hash, meta_hash};
pub use http::{Http, HttpRequest, HttpResponse, Method, TransportError};
pub use lineage::{
    Edge, EdgeRole, EdgeType, LineageContext, Resolution, ResolveOpts, ResolveStatus, RootInfo,
    edge_type, immediate_parent, lineage_edges, resolve_roots,
};
pub use manifest::{ArtifactState, Manifest, ManifestEntry};
pub use model::{Clip, HistoryEntry};
pub use naming::{
    CharacterSet, DEFAULT_TEMPLATE, NamingConfig, NamingRequest, RenderedName, render_clip_name,
    render_clip_names,
};
pub use reconcile::{
    Action, AlbumDesired, ArtifactKind, Desired, DesiredArtifact, LocalFile, Plan, SourceMode,
    SourceStatus, album_desired, deletion_allowed, plan_album_artifacts, reconcile,
};
pub use tag::{TrackMetadata, tag_flac, tag_mp3};
