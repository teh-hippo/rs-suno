# rs-suno

[![CI](https://github.com/teh-hippo/rs-suno/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/teh-hippo/rs-suno/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/rs-suno.svg)](https://crates.io/crates/rs-suno)
[![docs](https://img.shields.io/badge/docs-user%20guide-blue)](https://teh-hippo.github.io/rs-suno/)
[![coverage](https://codecov.io/gh/teh-hippo/rs-suno/branch/main/graph/badge.svg)](https://codecov.io/gh/teh-hippo/rs-suno)
[![licence](https://img.shields.io/badge/licence-MIT-blue)](https://github.com/teh-hippo/rs-suno/blob/main/LICENSE)

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

Pre-built binaries are also attached to each
[GitHub release](https://github.com/teh-hippo/rs-suno/releases) for Linux
(x86_64 and aarch64, statically linked with musl) and Windows (x86_64 and
aarch64, native MSVC). The binaries are unsigned, so Windows may show a
SmartScreen "unknown publisher" prompt; you can verify a download against its
published `.sha256` checksum and build-provenance attestation.

An amd64 and arm64 container image with ffmpeg included is published to
`ghcr.io/teh-hippo/rs-suno`:

```bash
podman run --rm ghcr.io/teh-hippo/rs-suno:latest version
```

See the [container guide](https://teh-hippo.github.io/rs-suno/containers.html)
for persistent mounts, secrets, scheduling, and Proxmox VE deployment.

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
- Scope a run to your liked songs or specific playlists with `--liked` and
  `--playlist`; a scoped run never deletes.
- MP3, FLAC, or WAV output (FLAC by default).
- Rich tags: title, artist, album, date, style, model, creator, and remix
  lineage, plus embedded and folder cover art and unsynced lyrics.
- Lineage albums that group a song with its remixes and edits.
- Optional animated WebP covers (`--animated-covers`).
- Optional standalone music-video download (`--video-mp4`).
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
Run `mise install` after cloning to install the project ffmpeg and
rust-analyzer versions.

## Licence

MIT. See [LICENSE](LICENSE).
