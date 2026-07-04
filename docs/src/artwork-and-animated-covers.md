# Artwork and animated covers

Every download is tagged and carries cover art, so your library looks right in
any music player. Artwork also lands as sidecar files so folder-based browsers
and media servers pick it up.

## Metadata tags

`rs-suno` writes rich metadata into each file. MP3 and WAV use ID3v2.4 frames
and FLAC uses Vorbis comments.

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

Plain lyrics (without timestamps) are embedded when the clip has them: as a
`USLT` frame in MP3 and a `LYRICS` comment in FLAC. You can also write them
beside the audio as an optional `.lyrics.txt` sidecar.

**Synced (timed) lyrics**

With `lrc_sidecar` enabled, `rs-suno` also writes a synced `<song>.lrc` beside
each audio file. When Suno has word/line alignment for the clip, the `.lrc` is a
standard line-level file carrying one `[mm:ss.xx]` timestamp per line:

```text
[ti:Neon Horizon]
[ar:alice]
[al:Neon Horizon]
[length:2:58]
[re:rs-suno]
[00:12.52]We ride the neon
[00:15.30]Chasing the dawn
```

Line-level is the universally supported LRC form, so every player syncs and
displays it cleanly. (The enhanced "A2" per-word format is parsed by only a few
karaoke players and shows as literal text in the rest, so it is not used; the
per-word timing is carried in the MP3 `SYLT` frame instead.)

The `.lrc` is the primary synced-lyrics artefact and is written for every
format (MP3, FLAC and WAV). For **MP3 and WAV**, an ID3 `SYLT` (synchronised
lyrics) frame is also embedded in the file with word-level timing, so players
that read `SYLT` show karaoke-style per-word lyrics. FLAC uses Vorbis comments
and carries no `SYLT`; the line-level `.lrc` covers it.

The alignment is fetched from Suno once per clip (the result is immutable), so
enabling the feature probes every clip once on the first pass (cached thereafter);
a steady-state re-sync fetches nothing more. A clip Suno cannot align, an
**instrumental**, writes no `.lrc` and embeds no `SYLT`, exactly as a clip with no
cover writes no cover. A clip with lyrics but no alignment yet receives an untimed
plain-text fallback; both instrumentals and untimed-fallback clips are re-checked
occasionally so a later-available alignment upgrades the `.lrc` and `SYLT`.
If a fetch fails (a network or server error), the song's existing `.lrc` and
tags are left untouched and it is retried on the next run, so a good timed file
is never downgraded. Turning `lrc_sidecar` off writes no `.lrc`, embeds nothing,
and fetches no alignment, and leaves any existing `.lrc` files in place (it is
never treated as a deletion).

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

Enable it per run with `--animated-covers`, `--video-cover-retention webp`, or
set `video_cover_retention = "webp"` in your
[config](configuration.md). You can also tune encoder knobs with
`animated_cover_quality`, `animated_cover_max_fps`,
`animated_cover_max_width`, and `animated_cover_compression_level`.

With animated covers on, and for clips that have a video preview, `rs-suno` also
writes:

- a per-song animated cover beside each audio file, sharing the track's name
  with a `.webp` extension, and
- an album animated cover named `cover.webp` in each album folder, chosen from
  the earliest clip in the album that has a video preview.

The static `.jpg` covers are always written as well, so players without WebP
support still show art.

### Keeping the raw source

The WebP is a re-encode. To keep Suno's original animation untouched, choose
`--video-cover-retention mp4` (or `video_cover_retention = "mp4"`): it writes the
album's `video_cover_url` verbatim as `cover.mp4`, with no transcode and no
ffmpeg. Use `both` to keep the raw `cover.mp4` beside the transcoded
`cover.webp`; the two come from the same album variant, and the source is
fetched only once.

This is a different asset from `--video-mp4`, which downloads the standalone
music video (`video_url`) beside each song.

### ffmpeg requirement

Animated covers are transcoded from the clip's video preview with `ffmpeg`, so
you need an ffmpeg build with animated-WebP (`libwebp_anim`) support. See
[Installation and ffmpeg](installation-and-ffmpeg.md#ffmpeg). Without it, use
the default static covers. The raw `cover.mp4` needs no ffmpeg.

## WAV metadata

WAV downloads carry full ID3v2.4 tags in a RIFF `id3 ` chunk — the same tag
set as MP3: title, artist, album, date, lyrics (`USLT`), cover art (`APIC`),
and all `SUNO_*` extended-text frames. Word-level synced lyrics (`SYLT`) are
also embedded when alignment is available. Choose FLAC (the default) for a
lossless archive with Vorbis comments, MP3 for the smallest files, or WAV when
you need uncompressed PCM with full metadata.

## What lands on disk

For an album with animated covers and synced lyrics enabled, the layout looks
like:

```text
alice/
  Neon Horizon/
    folder.jpg
    cover.webp
    alice-Neon Horizon [a1b2c3d4].flac
    alice-Neon Horizon [a1b2c3d4].jpg
    alice-Neon Horizon [a1b2c3d4].webp
    alice-Neon Horizon [a1b2c3d4].lrc
    alice-Neon Horizon (Remix) [8d9e0f1a].flac
    alice-Neon Horizon (Remix) [8d9e0f1a].jpg
    alice-Neon Horizon (Remix) [8d9e0f1a].webp
    alice-Neon Horizon (Remix) [8d9e0f1a].lrc
```

Without `--animated-covers`, the `.webp` files and `cover.webp` are simply not
written; without `lrc_sidecar`, the `.lrc` files are not written.
