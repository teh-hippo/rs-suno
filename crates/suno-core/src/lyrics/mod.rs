//! Lyric rendering, split so the timed-lyrics primitives stay a pure leaf.
//!
//! [`aligned`] is the zero-crate-import domain of Suno's aligned lyrics
//! ([`AlignedLyrics`] and the `.lrc` body / `SYLT` / plain-text renderers over
//! it). [`render`] adds the clip-level sidecar renderers (`.lyrics.txt` and the
//! timed/untimed `.lrc`), which need the clip, lineage, and tag metadata; it is
//! the sole place that coupling lives, keeping the primitives leaf clean.

mod aligned;
mod render;

pub use aligned::{AlignedLine, AlignedLineWord, AlignedLyrics, AlignedWord};
pub use render::{render_clip_lrc, render_clip_lyrics, render_synced_lrc};
