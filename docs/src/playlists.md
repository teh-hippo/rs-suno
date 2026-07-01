# Playlists (M3U8)

`rs-suno` writes your Suno playlists as `.m3u8` files so any player can open them
against your mirrored library.

## What gets written

- **One playlist per Suno playlist.** Each of your playlists is written as an
  extended M3U8 file, with its members in the order Suno holds them.
- **A synthetic "Liked Songs" playlist.** Your liked clips are written as a
  `Liked Songs.m3u8`, in order, even though Suno has no explicit playlist for
  them.

Playlist files are written at the root of the destination directory. Each file
is named after the playlist, made safe for the filesystem, with an `.m3u8`
extension.

## Format

The files are extended M3U8: a header, the playlist name, and one `#EXTINF`
entry per track giving its duration and title, followed by the track's path
relative to the playlist. Relative paths mean the playlist keeps working if you
move the whole library.

```text
#EXTM3U
#PLAYLIST:Neon Nights
#EXTINF:217,Neon Horizon
alice/Neon Horizon/alice-Neon Horizon [a1b2c3d4].flac
#EXTINF:182,Electric Storm
alice/Weather/alice-Electric Storm [3f2a1b4c].flac
```

## Members not in your library

A playlist can reference clips you have not downloaded (for example someone
else's track, or a clip excluded by a filter). Rather than write a broken path,
`rs-suno` records the member as a comment noting it is not in the library, using
the member's own title. The rest of the playlist stays valid and in order.

## Ordering and safety

- **Order is preserved** exactly as Suno reports it.
- A playlist is only written when its members were listed completely. If a
  playlist's listing fails, that playlist is skipped for the run rather than
  written half-empty. The synthetic "Liked Songs" playlist is likewise only
  written when the liked feed was fully enumerated.

Playlists are regular mirror artefacts: they are rewritten when their name,
order, or any member's path, title, or duration changes, and kept in step by
every `sync` or `copy`.
