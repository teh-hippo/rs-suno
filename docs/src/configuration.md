# Configuration

Most people keep their token and destination in a config file so a run is just
`suno sync` or `suno copy`. Flags and environment variables can still override
the file for one-off runs and automation.

> **Precedence:** for normal settings, the first value found wins:
> command-line flag, environment variable, source table, account table,
> defaults table, then the built-in default.

## Config file location

By default the config lives at:

- Linux and macOS: `$XDG_CONFIG_HOME/suno/config.toml`, or
  `~/.config/suno/config.toml`.
- Windows: `%APPDATA%\suno\config.toml`.

Point at a different file with `--config <PATH>` or the `SUNO_CONFIG`
environment variable. `suno version` prints the resolved path.

## Create a config

The quickest way is the interactive setup:

```bash
suno config init
```

It prompts for an account label (default `default`), your `__client` token, and
an optional library root, then writes the file. It will not overwrite an
existing config unless you pass `--yes`.

Add another account later:

```bash
suno config add-account work
```

Print the current config with every token redacted:

```bash
suno config show
```

## File format

The config is TOML with an optional `[defaults]` table and one
`[accounts.<label>]` table per account:

```toml
[defaults]
format = "flac"
concurrency = 4
retries = 3
min_newest = 1
animated_covers = false
video_cover_retention = "neither" # neither|webp|mp4|both
animated_cover_quality = 70
animated_cover_max_fps = 24
animated_cover_max_width = 720
animated_cover_compression_level = 0 # 0..6
details_sidecar = false
lyrics_sidecar = false
lrc_sidecar = false
video_mp4 = false
download_stems = false
stem_format = "wav"
naming_template = "{creator}/{album}/{creator}-{title} [{id8}]"
character_set = "unicode"

[accounts.me]
token = "__client=<your-token>"
root = "/home/alice/music/suno"
account_id = "user_abc123"
lyrics_sidecar = true
lrc_sidecar = true

[accounts.me.sources.liked]
format = "mp3"

[accounts.work]
token_command = "bws secret get <secret-id>"
root = "/home/alice/music/suno-work"
format = "mp3"

[accounts.work.areas]
library = "mirror"
liked = "copy"
playlists = "copy"

[accounts.work.areas.playlist]
"pl_abc123" = "mirror"

[accounts.work.albums]
"1a2b3c4d-0000-0000-0000-000000000000" = "Greatest Hits"
```

On Unix, `rs-suno` writes this config file with private permissions (`0600`), and
creates its parent config directory with private permissions (`0700`) when
needed. These modes are not applied on non-Unix platforms.

### Account settings

| Key | Type | Default | Description |
|---|---|---|---|
| `token` | string | | The `__client` session token for the account. |
| `token_command` | string | | A shell command to run for the account token. `rs-suno` trims stdout and uses it as the token. It is resolved after `--token` and `SUNO_*_TOKEN`, but before the stored `token`. |
| `root` | path | | Default destination directory. Used when a command omits `DEST`, and required by `--all`. |
| `account_id` | string | | Optional Suno user id this account must authenticate as. When set, a run refuses (exit 7) before contacting Suno if the token belongs to a different id, a belt-and-braces check alongside the on-disk owner pin. See [deletion safety](sync-copy-and-deletion-safety.md). |
| `format` | `mp3` \| `flac` \| `alac` \| `wav` | `flac` | Audio format for downloads. `flac` and `alac` (Apple Lossless, `.m4a`) are lossless transcodes of the WAV render; `alac` is tagged with iTunes atoms, keeping the Suno fields as freeform `com.apple.iTunes` atoms. Changing an existing library's format re-encodes the affected files on the next run, and an older `rs-suno` cannot read a library once a newer format is written. |
| `concurrency` | integer | `4` | Simultaneous downloads. |
| `retries` | integer | `3` | Download retry attempts per clip before it is logged as failed. |
| `min_newest` | integer | `1` | Minimum newest clips kept when a recency filter would otherwise select nothing. |
| `animated_covers` | bool | `false` | Also write animated WebP covers from clip video previews. |
| `video_cover_retention` | `neither` \| `webp` \| `mp4` \| `both` | `neither` | Retention mode for the album video-cover, derived from each album's animated preview. `webp` keeps the transcoded animated `cover.webp` (and the per-song `.webp`), `mp4` keeps the raw `cover.mp4` (the `video_cover_url` byte-for-byte, no transcode), `both` keeps both, `neither` keeps neither. Overrides `animated_covers` when set. The standalone music video (`video_url`) is a separate asset with its own `video_mp4` toggle. |
| `animated_cover_quality` | integer | `70` | Animated WebP quality (`0..100`, higher is better and larger). |
| `animated_cover_max_fps` | integer | `24` | Frame-rate cap for animated WebP output. |
| `animated_cover_max_width` | integer | native | Optional width cap in pixels for animated WebP output (no upscaling). |
| `animated_cover_compression_level` | integer | `0` | Animated WebP compression effort (`0..6`, higher is smaller and slower). |
| `details_sidecar` | bool | `false` | Also write a plain-text `<song>.details.txt` beside each audio file, dumping the same metadata that is embedded in the tags plus the song id, duration, and canonical `suno.com` URL. |
| `lyrics_sidecar` | bool | `false` | Also write a plain-text `<song>.lyrics.txt` beside each audio file, holding the song's lyrics verbatim. A song with no lyrics gets no file. |
| `lrc_sidecar` | bool | `false` | Also write a `<song>.lrc` beside each audio file. When Suno has word/line alignment for the song, the `.lrc` is synced line-level (a `[mm:ss.xx]` timestamp per line, the universally supported form) and, for MP3, an ID3 `SYLT` frame with per-word timing is embedded too; otherwise it falls back to the untimed lyrics. A song Suno cannot align (an instrumental) gets no file. Enabling this fetches each song's alignment once. |
| `video_mp4` | bool | `false` | Also download the standalone `<song>.mp4` music video beside each audio file, when Suno provides one. A song with no video gets no file. Turning this off leaves existing videos in place; a video is only removed alongside its own audio. |
| `download_stems` | bool | `false` | Also mirror each song's already-generated stems into a `<song>.stems/` sub-folder beside it. Download-only: it lists and downloads existing stems and **never** triggers separation or spends credits. A song with no stems gets no folder. Each stem is stored RAW (see `stem_format`), never transcoded to FLAC. Turning this off leaves existing stems in place; individual stems are only removed when Suno's authoritative listing no longer contains them, or alongside their own song. |
| `stem_format` | string | `wav` | Container for downloaded stems: `wav` (lossless, fetched through the same free WAV render the FLAC pipeline uses) or `mp3` (the public CDN file). Stems are stored RAW in whichever container and are never re-encoded to FLAC, even when the song's own `format` is FLAC. |
| `naming_template` | string | `{creator}/{album}/{creator}-{title} [{id8}]` | Relative path template. Supported placeholders are `{creator}`, `{handle}`, `{album}`, `{title}`, `{id}`, `{id8}`, and `{root_id8}`. Empty path segments are dropped. |
| `character_set` | `unicode` \| `ascii` | `unicode` | Character set for filename sanitisation. Unicode preserves valid path characters; ASCII folds names to portable ASCII. |
| `sources` | table | | Optional per-source overrides under `[accounts.<label>.sources.<name>]`. A source table may set any account key in this table except `token`, `root`, `account_id`, `sources`, `areas`, and `albums`. |
| `areas` | table | | Optional per-area mirror/copy selection. See [Per-area sync/copy modes](#per-area-synccopy-modes). |
| `albums` | table | | Optional album-name overrides keyed by lineage root id. See [Album name overrides](#album-name-overrides). |

Any per-run account key may also be set under `[defaults]` to apply to every
account. Account-only tables and identity fields (`token`, `root`, `account_id`,
`sources`, `areas`, and `albums`) cannot be set in `[defaults]`.

`token_command` and the other per-run settings also work in
`[accounts.<label>.sources.<name>]`, so one source can override an account or
default value when needed.

Security note: `token_command` runs a user-configured shell command. Keep it
under your control and never rely on untrusted input in the command string.

### Per-area sync/copy modes

By default a verb sets the mode for the whole run: `sync` mirrors, `copy` adds.
An optional `[accounts.<label>.areas]` table gives an account a durable per-area
mode instead, so a scheduled `suno sync` can mirror some areas and only add to
others:

```toml
[accounts.me.areas]
library = "mirror"   # "mirror", "copy", or "off"
liked = "copy"       # "mirror" or "copy"
playlists = "copy"   # default mode for every playlist

[accounts.me.areas.playlist]
# Per-playlist overrides, keyed by playlist id from Suno.
pl_abc123 = "mirror"
```

- **`library`** takes `mirror`, `copy`, or `off`. `off` is the only way to let a
  mirror delete files that exist only in your library and nowhere else, so it
  drops the implicit copy protector described in
  [deletion safety](sync-copy-and-deletion-safety.md). It cannot be set by a
  flag, only here.
- **`liked`** and **`playlists`** take `mirror` or `copy`. `playlists` sets the
  default for every playlist; `[areas.playlist]` overrides individual playlists
  by id.
- An area you do not list is simply not selected by a config-driven run.

A scope flag (`--liked` or `--playlist`) always overrides `[areas]` for that run,
and an unknown key (for example `libary` instead of `library`) is a parse error
rather than a silent no-op.

### Album name overrides

Album names are derived from lineage: a clip folders under its root ancestor's
title, or its own title when it is a root (see
[lineage and albums](lineage-and-albums.md)). When a derived name is undesirable
(for example the earliest version of a song carried a strange working title
before you settled on a proper one), an optional
`[accounts.<label>.albums]` table renames an album by its stable lineage root
id:

```toml
[accounts.me.albums]
# <root_id> = "Preferred Name"
"1a2b3c4d-...-rootid" = "Greatest Hits"
```

- The key is the album's **lineage root id**, not the derived title. The root id
  is stable, whereas the derived title is exactly what you are replacing. Find it
  from the `[{root_id8}]` suffix in a folder name, the `SUNO_LINEAGE` tag's
  `Root <id>` line, or the lineage store.
- The override is **account-wide**, like lineage itself, so it is set on the
  account and never per-source.
- A blank or whitespace-only value is ignored, so a stray key can never blank an
  album.
- The preferred name flows consistently into the folder path, the `ALBUM` tag,
  the change hash, and album-art paths, and it also settles name collisions: two
  distinct roots renamed onto the same album are still kept apart by the
  `[{root_id8}]` suffix.

On the next `sync`, an album rename **moves** the existing folder and all its
contents (member tracks, `folder.jpg`, `cover.webp` / `cover.mp4`) to the new
path and prunes the emptied old directory. It re-tags each track in place from
the local file; it does not re-download the audio. Deletion safety holds
throughout: the rename is a move, never a delete-then-redownload, and nothing is
deleted on an empty, failed, or partial listing.

### Multiple accounts

Each account has its own token and its own `root`. Account roots must not nest
inside one another: a config where one account's root is a parent of another's
is rejected, so two libraries can never share or overwrite files. Run one
account with `--account <label>`, or every account in isolation with `--all`
(each writes to its own `root`).

If exactly one account is configured, it is used automatically and you can omit
`--account`.

## Precedence

For every normal setting, the first value found wins, in this order:

1. Command-line flag (for example `--format wav`).
2. Environment variable (per-account `SUNO_<LABEL>_*` before global `SUNO_*`).
3. Source table (`[accounts.<label>.sources.<name>]`).
4. Account table (`[accounts.<label>]`).
5. Defaults table (`[defaults]`).
6. The built-in default.

Token resolution has one extra step between environment variables and the stored
account token:

1. `--token`
2. `SUNO_<LABEL>_TOKEN` or `SUNO_TOKEN`
3. `SUNO_<LABEL>_TOKEN_COMMAND`, `SUNO_TOKEN_COMMAND`, or `token_command`
   resolved from source, account, then defaults
4. `[accounts.<label>].token`

## Environment variables

| Variable | Equivalent | Notes |
|---|---|---|
| `SUNO_TOKEN` | `--token` | Also `SUNO_<LABEL>_TOKEN` for one account. |
| `SUNO_TOKEN_COMMAND` | `token_command` | Also `SUNO_<LABEL>_TOKEN_COMMAND` for one account. |
| `SUNO_ACCOUNT` | `--account` | |
| `SUNO_CONFIG` | `--config` | |
| `SUNO_DRY_RUN` | `--dry-run` | |
| `SUNO_YES` | `--yes` | |
| `SUNO_FORMAT` | `--format` | `mp3`, `flac`, `alac`, or `wav`. |
| `SUNO_CONCURRENCY` | `--concurrency` | Integer, default `4`. |
| `SUNO_RETRIES` | `--retries` | |
| `SUNO_MIN_NEWEST` | `--min-newest` | |
| `SUNO_ANIMATED_COVERS` | `--animated-covers` | `true` or `false`. |
| `SUNO_VIDEO_COVER_RETENTION` | `--video-cover-retention` | `neither`, `webp`, `mp4`, `both`. |
| `SUNO_ANIMATED_COVER_QUALITY` | `--animated-cover-quality` | `0..100`. |
| `SUNO_ANIMATED_COVER_MAX_FPS` | `--animated-cover-max-fps` | Positive integer. |
| `SUNO_ANIMATED_COVER_MAX_WIDTH` | `--animated-cover-max-width` | Integer width cap in pixels. |
| `SUNO_ANIMATED_COVER_COMPRESSION_LEVEL` | `--animated-cover-compression-level` | `0..6`. |
| `SUNO_DETAILS_SIDECAR` | `--details-sidecar` | `true` or `false`. |
| `SUNO_LYRICS_SIDECAR` | `--lyrics-sidecar` | `true` or `false`. |
| `SUNO_LRC_SIDECAR` | `--lrc-sidecar` | `true` or `false`. |
| `SUNO_VIDEO_MP4` | `--video-mp4` | `true` or `false`. |
| `SUNO_DOWNLOAD_STEMS` | `--download-stems` | `true` or `false`. |
| `SUNO_STEM_FORMAT` | `--stem-format` | `wav` or `mp3`. |
| `SUNO_NAMING_TEMPLATE` | `--naming-template` | Supported placeholders: `{creator}`, `{handle}`, `{album}`, `{title}`, `{id}`, `{id8}`, `{root_id8}`. |
| `SUNO_CHARACTER_SET` | `--character-set` | `unicode` or `ascii`. |

Per-account variants use the account label upper-cased with hyphens turned into
underscores, so account `my-lib` reads `SUNO_MY_LIB_TOKEN`,
`SUNO_MY_LIB_FORMAT`, and so on. A per-account variable overrides the matching
global one.

## Running without a config

You do not need a config file for the read-only and one-off commands. With
`--token` (or `SUNO_TOKEN`) set and no config present, `rs-suno` runs against a
single implicit account, which is handy for `ls`, `lsjson`, and `fetch`.
