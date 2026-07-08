//! The single JSON-decode home: maps the Suno API's JSON shapes onto the
//! crate's domain types. Each submodule owns one payload family (feed, clip,
//! playlist, aligned lyrics, stem, billing, and the WAV-render poll) and
//! colocates its tests.
//!
//! Transport (HTTP calls, retry, and the POST allow-list) stays in
//! [`client`](crate::client); this module is pure and performs no IO. It reads
//! [`model`](crate::model) for the [`Clip`](crate::model::Clip) type but nothing
//! in `model` depends on it, so the decode-to-domain edge is one-way.

mod billing;
mod clip;
mod feed;
mod lyrics;
mod playlist;
mod stem;
mod wav;

pub(crate) use billing::parse_billing_info;
pub(crate) use clip::{parse_clip, parse_songs_batch};
pub(crate) use feed::{feed_v3_body, parse_feed_v3};
pub(crate) use lyrics::parse_aligned_lyrics;
pub(crate) use playlist::{parse_playlist_clips, parse_playlists};
pub(crate) use stem::{parse_stem_page_count, parse_stems_page};
pub(crate) use wav::parse_wav_url;
