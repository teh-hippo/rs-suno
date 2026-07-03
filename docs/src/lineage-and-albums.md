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

## File and folder layout

Files are named deterministically from the clip and its lineage:

```text
{creator}/{album}/{creator}-{title} [{id8}]
```

- `{creator}` is your display name (falling back to your handle).
- `{album}` is the lineage album title described above.
- `{title}` is the clip's title.
- `{id8}` is the first eight characters of the clip id.

For example, a FLAC download might land at:

```text
alice/Neon Horizon/alice-Neon Horizon (Remix) [8d9e0f1a].flac
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
