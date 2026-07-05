# Installation and ffmpeg

## Install the CLI

### With Cargo

If you have a Rust toolchain, install the published crate from crates.io:

```bash
cargo install rs-suno
```

This builds and installs the `suno` binary into `~/.cargo/bin`. Make sure that
directory is on your `PATH`.

### Pre-built binaries

Pre-built binaries are attached to each
[GitHub release](https://github.com/teh-hippo/rs-suno/releases). The following
platforms are published:

| Platform | Asset | Notes |
|---|---|---|
| Linux x86_64 | `suno-vX-x86_64-linux.tar.gz` | statically linked (musl) |
| Linux aarch64 | `suno-vX-aarch64-linux.tar.gz` | statically linked (musl) |
| Windows x86_64 | `suno-vX-x86_64-windows.zip` | native MSVC |
| Windows aarch64 | `suno-vX-aarch64-windows.zip` | native MSVC |

Download the archive for your platform, extract the `suno` binary, and place it
somewhere on your `PATH`.

Each archive ships a matching `.sha256` checksum and a build-provenance
attestation. The binaries are unsigned, so Windows may show a SmartScreen
"unknown publisher" prompt on first run; verify the download against its
`.sha256` and attestation if you want to confirm its integrity.

### Verify the install

```bash
suno version
```

This prints the build version and target, the resolved config path, and the
detected `ffmpeg`:

```text
suno 0.1.0 (x86_64-unknown-linux-gnu)
config: /home/alice/.config/suno/config.toml
ffmpeg: 6.1.1 (detected at /usr/bin/ffmpeg)
```

If the last line reads `ffmpeg: not found on PATH`, install ffmpeg as below.

## ffmpeg

`rs-suno` shells out to `ffmpeg` for two jobs:

- transcoding the server-rendered lossless audio to **FLAC**, and
- transcoding a clip's video preview to an **animated WebP** cover when you pass
  `--animated-covers`.

You therefore need an `ffmpeg` build with FLAC and animated-WebP (`libwebp_anim`)
support. Most distribution packages include both. `suno` runs the first `ffmpeg`
it finds on your `PATH`.

### Install ffmpeg

| Platform | Command |
|---|---|
| Debian, Ubuntu | `sudo apt install ffmpeg` |
| Fedora | `sudo dnf install ffmpeg` |
| Arch | `sudo pacman -S ffmpeg` |
| macOS (Homebrew) | `brew install ffmpeg` |
| Windows (winget) | `winget install Gyan.FFmpeg` |

### Check ffmpeg has what you need

Confirm ffmpeg is on your `PATH` and can encode the formats:

```bash
ffmpeg -hide_banner -encoders | grep -E 'flac|libwebp'
```

You should see both the `flac` audio encoder and the `libwebp`/`libwebp_anim`
video encoders. FLAC is required for the default audio format; the WebP encoder
is only needed if you use `--animated-covers`. MP3 and WAV downloads do not need
ffmpeg for the audio itself, but FLAC does, so keeping a full ffmpeg build is the
simplest path.

## Next steps

With the binary and ffmpeg in place, set up your token in
[Authentication](authentication.md), then create a config with
[`suno config init`](configuration.md).
