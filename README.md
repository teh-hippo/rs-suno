# rs-suno

A download-only command line tool that mirrors your [Suno.ai](https://suno.com)
library to local files, modelled on [rclone](https://rclone.org).

The binary is `suno`; the crate is `rs-suno`.

## Prerequisite

`rs-suno` needs [`ffmpeg`](https://ffmpeg.org) on your `PATH`, built with FLAC
and animated-WebP support. Most distribution packages include both.

## Install

```bash
cargo install rs-suno
```

Pre-built binaries for common platforms are also attached to each
[GitHub release](https://github.com/teh-hippo/rs-suno/releases).

## Quick start

```bash
# 1. Create a config with your Suno __client token and a library root.
suno config init

# 2a. Mirror your library, including deletions (like rclone sync).
suno sync

# 2b. Or only ever add and update, never delete (like rclone copy).
suno copy
```

Preview any run without touching disk using `suno check` or `--dry-run`.

## Features

- Mirrors your whole library, liked songs, and playlists as tagged audio.
- Two verbs: `sync` mirrors with deletion, `copy` is additive.
- MP3, FLAC, or WAV output (FLAC by default).
- Rich tags: title, artist, album, date, style, model, creator, and remix
  lineage, plus embedded and folder cover art and unsynced lyrics.
- Lineage albums that group a song with its remixes and edits.
- Optional animated WebP covers (`--animated-covers`).
- M3U8 playlists, including a synthetic "Liked Songs" list.
- Careful deletion safety: it never deletes on an empty, failed, partial, or
  truncated listing, and aborts a suspicious mass deletion.
- Incremental and resumable, safe to run from cron or a systemd timer.
- Multiple named accounts, each with its own token and destination.

## Documentation

Read the full user guide at
**<https://teh-hippo.github.io/rs-suno/>**.

It covers installation, authentication, configuration, every command, deletion
safety, artwork, playlists, scheduling, and troubleshooting.

Contributors should start with [AGENTS.md](AGENTS.md).

## Licence

MIT. See [LICENSE](LICENSE).
