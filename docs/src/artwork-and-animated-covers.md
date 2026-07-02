# Artwork and animated covers

Every download is tagged and carries cover art, so your library looks right in
any music player. Artwork also lands as sidecar files so folder-based browsers
and media servers pick it up.

## Metadata tags

`rs-suno` writes rich metadata into each file. MP3 uses ID3v2.4 frames and FLAC
uses Vorbis comments.

**Core tags**

- Title
- Artist and album artist (your creator name, falling back to `Suno`)
- Album (the lineage album title; see
  [Lineage and albums](lineage-and-albums.md))
- Date (the clip's creation date)

**Suno tags**

- Style (the clip's style tags) and a style summary
- The generation prompt (as a `SUNO_PROMPT` tag)
- Model (name and version, for example `chirp-v4`)
- Creator handle
- Parent clip, root clip, and a compact lineage summary

**Lyrics**

Unsynced lyrics (plain text, without timestamps) are embedded when the clip has
them. You can also write them beside the audio as an optional `.lyrics.txt` or an
untimed `.lrc` sidecar.

## Cover art

Each download carries and produces static JPEG cover art:

- **Embedded front cover** inside the audio file, so players that read embedded
  art show it.
- **A per-song cover** written beside each audio file, sharing the track's name
  with a `.jpg` extension.
- **An album cover** named `folder.jpg` in each album folder, which folder-based
  players and media servers use as the album thumbnail. It is chosen
  deterministically from the most-played art-bearing clip in the album.

## Animated covers

Suno clips have a short looping video preview. `rs-suno` can turn that into an
**animated WebP** cover. This is opt-in, because it costs an extra transcode per
clip.

Enable it per run with `--animated-covers`, or set `animated_covers = true` in
your [config](configuration.md).

With animated covers on, and for clips that have a video preview, `rs-suno` also
writes:

- a per-song animated cover beside each audio file, sharing the track's name
  with a `.webp` extension, and
- an album animated cover named `cover.webp` in each album folder, chosen from
  the earliest clip in the album that has a video preview.

The static `.jpg` covers are always written as well, so players without WebP
support still show art.

### ffmpeg requirement

Animated covers are transcoded from the clip's video preview with `ffmpeg`, so
you need an ffmpeg build with animated-WebP (`libwebp_anim`) support. See
[Installation and ffmpeg](installation-and-ffmpeg.md#ffmpeg). Without it, use
the default static covers.

## A note on WAV

The WAV format carries only limited metadata. When you download in WAV, lyrics
and embedded album art are omitted, and `rs-suno` warns you. Choose FLAC (the
default) or MP3 if you want the full set of tags and embedded art.

## What lands on disk

For an album with animated covers enabled, the layout looks like:

```text
alice/
  Neon Horizon/
    folder.jpg
    cover.webp
    alice-Neon Horizon [a1b2c3d4].flac
    alice-Neon Horizon [a1b2c3d4].jpg
    alice-Neon Horizon [a1b2c3d4].webp
    alice-Neon Horizon (Remix) [8d9e0f1a].flac
    alice-Neon Horizon (Remix) [8d9e0f1a].jpg
    alice-Neon Horizon (Remix) [8d9e0f1a].webp
```

Without `--animated-covers`, the `.webp` files and `cover.webp` are simply not
written.
