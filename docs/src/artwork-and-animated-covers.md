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
- Track number and total, numbering the album by creation order (see
  [track numbers](lineage-and-albums.md#track-numbers)): `TRACKNUMBER`/
  `TRACKTOTAL` in FLAC, `TRCK` in MP3 and WAV, `trkn` in ALAC
- Date (the clip's creation date)

**Suno tags**

- Style (the clip's style tags) and a style summary
- The generation prompt (as a `SUNO_PROMPT` tag)
- Model (name and version, for example `chirp-v4`)
- Creator handle
- Parent clip, root clip, and a compact lineage summary
- The clip's own id and canonical page URL (`SUNO_ID` and `SUNO_URL`,
  `https://suno.com/song/<id>`), so a track can be traced back to Suno

**Lyrics**

Plain lyrics (without timestamps) are embedded when the clip has them: as a
`USLT` frame in MP3 and a `LYRICS` comment in FLAC. You can also write them
beside the audio as an optional `.lyrics.txt` sidecar. When the feed provides no
inline lyrics for a clip, the sidecar is filled from Suno's aligned lyrics (the
same timed source as the `.lrc` below), so it still captures the words.

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

Each download carries cover art:

- **Embedded front cover** inside the audio file, so players that read embedded
  art show it. By default this is a static JPEG; with animated covers enabled it
  becomes an animated WebP for clips that have a video preview (see below).
- **A per-song cover** written beside each audio file, sharing the track's name
  with a `.jpg` extension. This is always the static JPEG, so folder-based
  browsers and players without WebP support still show art.
- **An album cover** named `folder.jpg` in each album folder, which folder-based
  players and media servers use as the album thumbnail. It is chosen
  deterministically from the most-played art-bearing clip in the album.

## Animated covers

Suno clips have a short looping video preview. `rs-suno` can turn that into an
**animated WebP** and embed it as the audio file's front cover, so a media server
that animates embedded art (Navidrome, for example) plays it. This is opt-in,
because it costs an extra transcode per clip.

Enable it per run with `--animated-covers`, `--video-cover-retention webp`, or
set `video_cover_retention = "webp"` (or `animated_covers = true`) in your
[config](configuration.md). Clips with no video preview keep the static JPEG, as
do ALAC (`.m4a`) files, whose container cannot hold a WebP picture.

WebP is the right format for the embed: media servers that show animated covers
animate WebP, APNG, and GIF but do **not** accept video (MP4/WebM), and WebP
gives the best quality for the size.

### The FLAC size cap

A FLAC picture is stored in a metadata block whose length is a 24-bit field, so a
single embedded picture cannot exceed about 16 MiB. A lossless animated cover is
far larger than that (a 5 s preview is around 145 MB, and even a lossless 384 px
encode is around 34 MB), so the FLAC embed must be a **bounded lossy** encode.
The default is **quality 90 scaled to at most 640 px** (about 11 MiB for a typical
5 s cover, comfortably under the cap), tunable with `animated_cover_quality`,
`animated_cover_max_fps`, `animated_cover_max_width`, and
`animated_cover_compression_level`. If an encode still would not fit (an unusual
cover, or quality and width set very high), `rs-suno` embeds the static JPEG for
that track instead, so the file is always valid. MP3 and WAV picture limits are
far higher, but the same bounded default is used to keep covers modest, because a
media server serves the embedded image at full size everywhere, including grids.

### Client support

Animation renders only where the client both reads embedded art **and** decodes
animated WebP: the Navidrome web UI in a modern browser does, while many native
and mobile Subsonic clients show the first frame only. A player that cannot read
a WebP embedded cover at all falls back to the `.jpg` per-song cover or
`folder.jpg`, so a library never loses its static art. The animated cover is
stored once per track (embedded), so enabling it grows every animated track by
roughly the encoded cover size.

### Lossless covers

For a bit-exact cover, set `--animated-cover-lossless` (or
`animated_cover_lossless = true`). Lossless animated video is intrinsically
enormous (a five-second preview is around **145 MB**), far larger than the
embedded-cover size cap, so a lossless cover always overflows it and the track
falls back to the static JPEG. Lossless is therefore not useful for embedded
covers; leave it off. Effort is capped at 4 for every mode, because effort 6
produces the same size for many times the encode time.

### Keeping the raw source

The embedded WebP is a re-encode. To keep Suno's original animation untouched,
choose `--video-cover-retention mp4` (or `video_cover_retention = "mp4"`): it
writes the album's `video_cover_url` verbatim as `cover.mp4`, with no transcode
and no ffmpeg. Use `both` to embed the animated WebP and also keep the raw
`cover.mp4`; the two come from the same album variant, and the source is fetched
only once.

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
    alice-Neon Horizon [a1b2c3d4].flac
    alice-Neon Horizon [a1b2c3d4].jpg
    alice-Neon Horizon [a1b2c3d4].lrc
    alice-Neon Horizon (Remix) [8d9e0f1a].flac
    alice-Neon Horizon (Remix) [8d9e0f1a].jpg
    alice-Neon Horizon (Remix) [8d9e0f1a].lrc
```

The animated cover is embedded inside each `.flac` (for clips with a video
preview), not written as a separate file. With `--video-cover-retention mp4` or
`both`, a raw `cover.mp4` also lands in the album folder. Without
`--animated-covers`, the embedded cover is the static JPEG; without
`lrc_sidecar`, the `.lrc` files are not written.
