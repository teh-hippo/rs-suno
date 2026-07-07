//! Core engine for rs-suno: library selection, sync reconciliation, and tagging.
//!
//! Runtime-agnostic and free of direct IO. Network access happens through the
//! [`Http`] port, which a CLI adapter implements, so the engine stays testable
//! in isolation.

mod album_art;
mod area;
mod auth;
mod backoff;
mod client;
mod clock;
pub mod config;
mod consts;
pub mod desired;
mod downloadable;
mod error;
mod executor;
mod extras;
mod ffmpeg;
mod fs;
mod graph;
mod hash;
mod http;
mod identity;
mod limiter;
mod lineage;
mod lyrics;
mod manifest;
mod model;
mod naming;
mod orphans;
mod pathkey;
pub mod reconcile;
mod roots;
pub mod select;
mod synced;
mod tag;
mod tag_alac;
mod tracks;
mod vocab;
mod wire;

#[cfg(test)]
mod testutil;

#[cfg(test)]
mod sync_chaos;

pub use album_art::{AlbumArt, PlaylistState};
pub use area::{
    AreaKind, AreaListing, adoption_enumerated, area_enumerated, area_mode, build_modes_by_id,
    build_scoped_playlist_desired, library_authoritative, playlists_enumerated, source_statuses,
    union_clips,
};
pub use auth::{ClerkAuth, TOKEN_EXPIRY_WARN_DAYS, TokenExpiry, classify_token_expiry};
pub use client::SunoClient;
pub use clock::Clock;
pub use config::{
    AccountConfig, AreaMode, AreasConfig, Config, Defaults, EffectiveSettings, FlagOverrides,
    Settings, SourceConfig,
};
pub use desired::{
    ArtifactToggles, LIKED_PLAYLIST_ID, PlaylistInput, build_desired, build_playlist_desired,
    clip_stems,
};
pub use downloadable::is_downloadable;
pub use error::{Error, Result};
pub use executor::{ExecOptions, ExecOutcome, Failure, Ports, RunStatus, execute};
pub use extras::{
    INDEX_SCHEMA_VERSION, M3u8Entry, render_clip_details, render_clip_lrc, render_clip_lyrics,
    render_library_index, render_m3u8, render_synced_lrc,
};
pub use ffmpeg::{Ffmpeg, FfmpegError, FfmpegErrorKind};
pub use fs::{FileStat, Filesystem, FsError, FsErrorKind};
pub use graph::{CacheEntry, LineageStore, Node, StoredEdge};
pub use hash::{
    SYNCED_LRC_VERSION, art_hash, art_url_hash, content_hash, meta_hash, synced_lrc_source_hash,
};
pub use http::{Http, HttpRequest, HttpResponse, Method, TransportError};
pub use identity::{AdoptDecision, Owner, OwnerGate, adopt_decision, owner_gate};
pub use lineage::{
    AttributionEdge, Edge, EdgeRole, EdgeType, LineageContext, Resolution, ResolveStatus, RootInfo,
    attribution_edges, edge_type, immediate_parent, lineage_edges,
};
pub use lyrics::{AlignedLine, AlignedLineWord, AlignedLyrics, AlignedWord};
pub use manifest::{ArtifactState, Manifest, ManifestEntry, SyncedLyricsCheck};
pub use model::{BillingInfo, Clip, ClipRoot, HistoryEntry, MediaUrl, Playlist, Stem};
pub use naming::{
    CharacterSet, DEFAULT_TEMPLATE, NamingConfig, NamingRequest, RenderedName, render_clip_name,
    render_clip_names, sanitise_name, stem_file_path, stems_folder,
};
pub use orphans::untracked_audio;
pub use reconcile::{
    Action, AlbumDesired, Desired, DesiredArtifact, DesiredStem, LocalFile, Plan, PlaylistDesired,
    SourceStatus, album_desired, area_authoritative, area_fully_enumerated, deletion_allowed,
    narrows_downloads, plan_album_artifacts, plan_playlist_artifacts, reconcile,
};
pub use roots::{ResolveOpts, resolve_roots};
pub use synced::{
    PendingCheck, SYNCED_LRC_RECHECK_SECS, apply_synced_lrc, preview_synced_lrc,
    synced_lyrics_targets,
};
pub use tag::{Cover, TrackMetadata, flac_picture_data_budget, tag_flac, tag_mp3};
pub use tag_alac::tag_alac;
pub use tracks::{LeadResolution, TrackAssignment, assign_track_numbers, resolve_lead_ids};
pub use vocab::{
    ArtifactKind, AudioFormat, SourceMode, StemFormat, VideoCoverRetention, WebpEncodeSettings,
};
