# Configuration

Most people keep their token and destination in a config file so a run is just
`suno sync` or `suno copy`. Flags and environment variables can still override
the file for one-off runs and automation.

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
retries = 3
min_newest = 1
animated_covers = false
details_sidecar = false
lyrics_sidecar = false
lrc_sidecar = false

[accounts.me]
token = "<your __client token>"
root = "/home/alice/music/suno"

[accounts.work]
token = "<another token>"
root = "/home/alice/music/suno-work"
format = "mp3"
```

On Unix, `rs-suno` writes this config file with private permissions (`0600`), and
creates its parent config directory with private permissions (`0700`) when
needed. These modes are not applied on non-Unix platforms.

### Account settings

| Key | Type | Default | Description |
|---|---|---|---|
| `token` | string | | The `__client` session token for the account. |
| `root` | path | | Default destination directory. Used when a command omits `DEST`, and required by `--all`. |
| `account_id` | string | | Optional Suno user id this account must authenticate as. When set, a run refuses (exit 7) before contacting Suno if the token belongs to a different id, a belt-and-braces check alongside the on-disk owner pin. See [deletion safety](sync-copy-and-deletion-safety.md). |
| `format` | `mp3` \| `flac` \| `wav` | `flac` | Audio format for downloads. |
| `retries` | integer | `3` | Download retry attempts per clip before it is logged as failed. |
| `min_newest` | integer | `1` | Minimum newest clips kept when a recency filter would otherwise select nothing. |
| `animated_covers` | bool | `false` | Also write animated WebP covers from clip video previews. |
| `details_sidecar` | bool | `false` | Also write a plain-text `<song>.details.txt` beside each audio file, dumping the same metadata that is embedded in the tags plus the song id, duration, and canonical `suno.com` URL. |
| `lyrics_sidecar` | bool | `false` | Also write a plain-text `<song>.lyrics.txt` beside each audio file, holding the song's lyrics verbatim. A song with no lyrics gets no file. |
| `lrc_sidecar` | bool | `false` | Also write an untimed `<song>.lrc` beside each audio file, holding the song's lyrics with a small tag header (plain lyrics, no per-line timestamps). A song with no lyrics gets no file. |

Any account key except `token`, `root`, and `account_id` may also be set under
`[defaults]` to apply to every account.

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
# Per-playlist overrides, keyed by playlist id (see `suno ls-playlists`).
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

### Multiple accounts

Each account has its own token and its own `root`. Account roots must not nest
inside one another: a config where one account's root is a parent of another's
is rejected, so two libraries can never share or overwrite files. Run one
account with `--account <label>`, or every account in isolation with `--all`
(each writes to its own `root`).

If exactly one account is configured, it is used automatically and you can omit
`--account`.

## Precedence

For every setting, the first value found wins, in this order:

1. Command-line flag (for example `--format wav`).
2. Environment variable (per-account `SUNO_<LABEL>_*` before global `SUNO_*`).
3. Config file (`[accounts.<label>]` before `[defaults]`).
4. The built-in default.

## Environment variables

| Variable | Equivalent | Notes |
|---|---|---|
| `SUNO_TOKEN` | `--token` | Also `SUNO_<LABEL>_TOKEN` for one account. |
| `SUNO_ACCOUNT` | `--account` | |
| `SUNO_CONFIG` | `--config` | |
| `SUNO_DRY_RUN` | `--dry-run` | |
| `SUNO_YES` | `--yes` | |
| `SUNO_FORMAT` | `--format` | `mp3`, `flac`, or `wav`. |
| `SUNO_RETRIES` | `--retries` | |
| `SUNO_MIN_NEWEST` | `--min-newest` | |
| `SUNO_ANIMATED_COVERS` | `--animated-covers` | `true` or `false`. |
| `SUNO_DETAILS_SIDECAR` | `--details-sidecar` | `true` or `false`. |
| `SUNO_LYRICS_SIDECAR` | `--lyrics-sidecar` | `true` or `false`. |
| `SUNO_LRC_SIDECAR` | `--lrc-sidecar` | `true` or `false`. |

Per-account variants use the account label upper-cased with hyphens turned into
underscores, so account `my-lib` reads `SUNO_MY_LIB_TOKEN`,
`SUNO_MY_LIB_FORMAT`, and so on. A per-account variable overrides the matching
global one.

## Running without a config

You do not need a config file for the read-only and one-off commands. With
`--token` (or `SUNO_TOKEN`) set and no config present, `rs-suno` runs against a
single implicit account, which is handy for `ls`, `lsjson`, and `fetch`.
