# Playlists (M3U8)

`rs-suno` writes your Suno playlists as `.m3u8` files so any player can open them
against your mirrored library.

## What gets written

- **One playlist per Suno playlist.** Each of your playlists is written as an
  extended M3U8 file, with its members in the order Suno holds them.
- **The Suno playlist image.** When Suno exposes `image_url`, a same-basename
  `.jpg` is written beside the `.m3u8`.
- **A synthetic "Liked Songs" playlist.** Your liked clips are written as a
  `Liked Songs.m3u8`, in order, even though Suno has no explicit playlist for
  them.

Playlist files are written at the root of the destination directory. Each file
is named after the playlist, made safe for the filesystem, with an `.m3u8`
extension.

For example, `Neon Nights.m3u8` and `Neon Nights.jpg` form one playlist and
cover pair. Navidrome 0.63.2 discovers this sidecar natively, so no server API
credentials or `#EXTALBUMARTURL` directive are required. The synthetic
`Liked Songs.m3u8` has no image unless Suno later exposes one for that feed.

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
- If any audio download or lossless upgrade fails, playlist files and covers are
  left untouched so they never point at an uncommitted target path.

Playlists are regular mirror artefacts: their `.m3u8` files are rewritten when
their name, order, or any member's path, title, or duration changes. A playlist
image is refreshed when its Suno URL changes or the local `.jpg` is missing.
A failed image fetch leaves the previous cover untouched. Removing or renaming
playlist files and covers uses the same fully enumerated deletion gates.

A scoped run (`--liked` or `--playlist`) maintains only the selected areas'
`.m3u8` files. A liked-only run refreshes `Liked Songs.m3u8`; a playlist-scoped
run refreshes the playlists it enumerated. Other existing playlist files are
left untouched. Run a full `sync` or `copy` to refresh every playlist.

## Media servers

The `.m3u8` files on disk always describe the current library, because they are
rewritten whenever a member's path changes. What can drift is the *copy* a media
server imported into its own database.

Servers such as Navidrome, Plex and Jellyfin store playlist membership as a
reference to their own per-file database row. When a file both moves and has an
identity-bearing tag rewritten in the same run, a server can fail to recognise
the move, create a second row, and mark the original missing. Any playlist
pointing at the old row then loses that track, often with no error: Navidrome,
for example, keeps serving the original song count while silently returning
fewer entries.

The default naming template makes this more likely than you might expect,
because `{track2}` puts the album track number in the file name. Track numbers
come from creation order within a [lineage album](lineage-and-albums.md), so a
newly generated sibling can renumber the whole album, renaming every file and
rewriting every `TRACKNUMBER` tag at once.

If you point a media server at the library, either of these avoids the problem.

**Keep paths stable.** Drop the volatile prefix so a file name depends only on
the clip's own immutable id:

```toml
[defaults]
naming_template = "{creator}/{album}/{creator}-{title} [{id8}]"
```

Track numbers are still written to `TRACKNUMBER` and `TRACKTOTAL`, so albums
order correctly in a player. Only the paths stop moving. Changing this on an
existing library causes one mass rename, so let the server rescan afterwards.

**Or anchor the server to the clip id.** Every file carries its Suno clip id in
the `SUNO_ID` tag, which never changes for a given clip. Pointing the server's
file identity at that tag makes renames and retags harmless. In Navidrome this
is the `PID.Track` setting, and the custom tag may need registering in the
server's tag mappings first.

Whichever you choose, keep the library root path stable and leave the server's
playlist auto-import enabled, so the sidecars can repair an imported playlist on
the next scan. If the root moves, a server may keep looking for the sidecars at
the old location and quietly stop refreshing them.
