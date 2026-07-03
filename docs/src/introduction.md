# Introduction

`rs-suno` is a download-only command line tool that mirrors your [Suno.ai](https://suno.com)
library to local files. It is written in Rust and modelled on
[rclone](https://rclone.org): you point it at a destination directory and it
keeps that directory in step with your Suno library.

The binary is called `suno`. The crate published to [crates.io](https://crates.io)
is `rs-suno`, so you install it with `cargo install rs-suno` and then run `suno`.

## What it does

- Downloads your whole library, plus liked songs and playlists, as tagged audio
  files (MP3, FLAC, or WAV).
- Mirrors changes on every run: it downloads new clips, updates tags and
  artwork, renames or re-encodes files that changed, and, with `sync`, removes
  local files whose clips have left your library.
- Embeds rich metadata: core tags (title, artist, album, date) plus Suno details
  (style, model, creator, and remix lineage), a front cover, and lyrics —
  including optional synced (timed) lyrics as an `.lrc` sidecar and an
  MP3 `SYLT` frame.
- Groups remixes and edits into lineage albums and writes M3U8 playlists,
  including a synthetic "Liked Songs" list.
- Is safe to run unattended from cron or a systemd timer, with careful deletion
  rules so a bad listing can never wipe your library.

## Two verbs, like rclone

`rs-suno` follows the rclone model of two clear verbs:

- **`sync`** mirrors a source to a destination, including deleting local files
  that are no longer present upstream. This is the full mirror.
- **`copy`** is additive: it downloads and updates, but never deletes.

If you only ever want to accumulate files, use `copy`. If you want the
destination to be a faithful mirror of your library, use `sync`. Deletion is
governed by strict safety rules described in
[Sync, copy and deletion safety](sync-copy-and-deletion-safety.md).

## Requirements

- A Suno account and its `__client` session token (see
  [Authentication](authentication.md)).
- `ffmpeg` on your `PATH`, built with FLAC and animated-WebP support (see
  [Installation and ffmpeg](installation-and-ffmpeg.md)).

## Where to go next

- New here? Start with [Installation and ffmpeg](installation-and-ffmpeg.md),
  then [Authentication](authentication.md).
- Ready to run? See the [Commands reference](commands-reference.md).
- Automating it? See [Scheduling and exit codes](scheduling-and-exit-codes.md).
