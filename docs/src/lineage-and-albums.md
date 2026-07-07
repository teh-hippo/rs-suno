# Lineage and albums

Suno lets you build on a clip: remix it, extend it, or edit it. Each new clip
records the one it came from, which forms a lineage that runs back to an original
root clip. `rs-suno` follows that lineage to group related clips into albums and
to lay out files predictably.

## Root resolution

For every clip, `rs-suno` walks the lineage back to its **root ancestor**, the
original clip a family of remixes and edits grew from. It fills gaps by looking
up parents directly, and it keeps a durable archive of what it has resolved (see
[the lineage store](#the-lineage-store) below), so ancestry stays stable across
runs even after Suno purges an intermediate clip.

## Albums from lineage

Clips that share a root are grouped into one **lineage album**. The album title
is chosen simply:

- If the root ancestor is a real, distinct clip, the album takes the **root
  clip's title**.
- Otherwise (a clip that is its own root), the album takes the **clip's own
  title**.

So a song and all its remixes and extensions land in one album named after the
original, while a standalone clip sits in an album of its own name.

### Overriding an album name

When a derived title is undesirable, you can rename an album by its lineage root
id in the `[accounts.<label>.albums]` config table (see
[album name overrides](configuration.md#album-name-overrides)). The preferred
name replaces the derived one everywhere the album appears: the folder path, the
`ALBUM` tag, the change hash, and album art. On the next `sync` the existing
folder is moved to the new name and the emptied old directory pruned, with no
re-download and deletion safety intact.

## Track numbers

Within an album, tracks are numbered by when each version was made: the earliest
`created_at` is track 1, the next track 2, and so on. The number is written to
the audio tags (`TRACKNUMBER` and `TRACKTOTAL` for FLAC, the equivalent
`TRCK`/`trkn` for MP3, WAV, and ALAC), so a player lists the album in creation
order, and it also prefixes the file name (`07 - …`) via the default template's
`{track2}` placeholder.

`TRACKTOTAL` is the number of that album's downloaded tracks. A single-track
album is numbered `1` of `1` by default; set `number_singletons = false` to
leave lone songs unnumbered (and unprefixed).

### Setting a lead track

Sometimes the version you think of as "song 1" was not made first, for example
when you edited the main version after generating a batch of remixes. Flag it as
the album's lead and it is promoted to track 1, with the rest shifting down while
keeping their relative order:

```toml
[accounts.me]
lead_tracks = [
  "b320f4cf",   # the 8-char code from a file name, or the full clip id
]
```

Each entry is a clip id or a unique prefix of one (such as the `[b320f4cf]` code
in a file name); the album is found from that clip's lineage, so you never name
the album here. There is one lead per album, and an entry that matches no
downloaded clip, or more than one, is reported and ignored.

## File and folder layout

Files are named deterministically from the clip and its lineage:

```text
{creator}/{album}/{track2} - {creator}-{title} [{id8}]
```

- `{creator}` is your display name (falling back to your handle).
- `{album}` is the lineage album title described above.
- `{track}` is the album track number (for example `7`); `{track2}` is the same
  number zero-padded to two digits (`07`). An unnumbered track renders neither,
  and its trailing separator is dropped so no orphan ` - ` is left behind.
- `{title}` is the clip's title.
- `{id8}` is the first eight characters of the clip id (`{id}` is the full id,
  `{root_id8}` the first eight of the lineage root, `{handle}` your handle).

For example, a FLAC download might land at:

```text
alice/Neon Horizon/03 - alice-Neon Horizon (Remix) [8d9e0f1a].flac
```

Names are made safe for the filesystem. Unicode is preserved where it is valid
in a path; awkward characters are replaced, and over-long components are
shortened.

## Collision safety

Two protections make the layout collision-free:

- **Same-title clips never clash.** The `[{id8}]` suffix in every file name keeps
  two clips with the same title in separate files.
- **Distinct roots never share a folder.** If two different roots happen to have
  the same album title, the folders are separated by a short root-id suffix, so
  one album can never absorb another's tracks.

## Lineage tags

The lineage is also written into each file's metadata, including the parent clip,
the root clip, and a compact summary of the chain. See
[Artwork and animated covers](artwork-and-animated-covers.md#metadata-tags) for
the full list of tags.

## The lineage store

Resolved ancestry is saved beside your library in `.suno-lineage.json`. It is an
append-durable archive: once an ancestor is known, it is kept, even if the clip
is later trashed or removed upstream. This keeps album grouping stable over time.
Because it cannot be rebuilt once Suno purges old clips, a corrupt store stops
the run rather than being silently discarded.
