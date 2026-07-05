# Commands reference

```text
suno [OPTIONS] <COMMAND>
```

| Command | Purpose |
|---|---|
| [`sync`](#sync) | Mirror your library to a directory, including deletions. |
| [`copy`](#copy) | Download and update, never delete. |
| [`check`](#check) | Report what `sync` or `copy` would change, touching nothing. |
| [`ls`](#ls) | List clips in a readable table. |
| [`lsjson`](#lsjson) | List clips as newline-delimited JSON. |
| [`fetch`](#fetch) | Download one clip by ID or URL. |
| [`config`](#config) | Manage the configuration file. |
| [`auth`](#auth) | Manage authentication. |
| [`doctor`](#doctor) | Diagnose environment, config, auth, and credits. |
| [`version`](#version) | Print version and environment information. |
| [`completions`](#completions) | Emit a shell completion script. |

## Global options

These apply to every command and may appear before or after the subcommand.

| Flag | Short | Env | Description |
|---|---|---|---|
| `--account <LABEL>` | | `SUNO_ACCOUNT` | Run against one configured account. |
| `--all` | | | Run every configured account in isolation (`sync`/`copy`). Conflicts with `--account`. |
| `--config <PATH>` | | `SUNO_CONFIG` | Path to the config file. |
| `--dry-run` | `-n` | `SUNO_DRY_RUN` | Report changes without writing to disk or deleting. |
| `--verbose` | `-v` | | Increase verbosity. Repeatable (`-vv`). |
| `--quiet` | `-q` | | Decrease verbosity. Repeatable (`-qq`). |
| `--yes` | `-y` | `SUNO_YES` | Skip confirmation prompts (such as a destructive `sync`). |
| `--token <TOKEN>` | | `SUNO_TOKEN` | The `__client` token. Never printed. Overrides config and env. |

### Verbosity

Verbosity is relative to the default level of 0.

| Level | Flag | Output |
|---|---|---|
| Silent | `-qq` | Errors only. |
| Quiet | `-q` | Per-run summary, warnings, and errors. |
| Default | | Summary plus a single progress line. |
| Verbose | `-v` | A line per clip as it is downloaded, tagged, renamed, skipped, or deleted, and a line per sidecar written or removed. |

Machine-readable output (`ls` rows and `lsjson` objects) goes to stdout;
progress and summaries go to stderr, so a piped `lsjson` stays clean.

## sync

Mirror selected clips into a destination: download new clips, update tags and
artwork, rename or re-encode changed files, and remove local files whose clips
have left your library. Deletion is governed by strict safety rules; see
[Sync, copy and deletion safety](sync-copy-and-deletion-safety.md).

```text
suno sync [OPTIONS] [DEST]
```

`DEST` is the local directory to mirror into. If omitted, the account's
configured `root` is used.

| Flag | Default | Description |
|---|---|---|
| `--format <mp3\|flac\|wav>` | `flac` | Audio format for downloads. WAV carries full ID3v2.4 tags (lyrics, art, and all SUNO fields) embedded in a RIFF `id3 ` chunk. |
| `--limit <N>` | | Mirror only the N most recent clips. |
| `--since <SPEC>` | | Mirror clips newer than `7d`, `2w`, or `last-run`. |
| `--liked` | off | Scope the run to your liked songs only (additive unless `--mode mirror`). |
| `--playlist <ID_OR_NAME>` | | Scope the run to a playlist, by id or name (repeatable; additive unless `--mode mirror`). |
| `--mode <mirror\|copy>` | scoped default: `copy` | Select the mode for scoped areas: `mirror` arms deletion, `copy` stays additive. Only meaningful with `--liked`, `--playlist`, or an `[areas]` config. |
| `--min-newest <N>` | `1` | Newest clips always kept when a recency filter applies. |
| `--retries <N>` | `3` | Download retry attempts per clip. |
| `--concurrency <N>` | `4` | Simultaneous downloads. |
| `--animated-covers` | off | Also write animated WebP covers from video previews. |
| `--video-cover-retention <neither\|webp\|mp4\|both>` | `neither` | Album video-cover retention: `webp` keeps the transcoded `cover.webp`, `mp4` keeps the raw `cover.mp4` (no transcode), `both` keeps both. Overrides `--animated-covers`; the standalone music video stays on `--video-mp4`. |
| `--animated-cover-quality <N>` | `70` | Animated WebP quality (`0..100`). |
| `--animated-cover-max-fps <N>` | `24` | Animated WebP frame-rate cap. |
| `--animated-cover-max-width <PIXELS>` | native | Animated WebP width cap (omit to keep source width). |
| `--animated-cover-compression-level <N>` | `0` | Animated WebP compression effort (`0..6`). |
| `--allow-account-change` | off | Re-pin this library to the authenticated account. The run is additive and deletes nothing. |
| `--details-sidecar` | off | Also write a plain-text `.details.txt` sidecar next to each song. |
| `--lyrics-sidecar` | off | Also write a plain-text `.lyrics.txt` sidecar next to each song. |
| `--lrc-sidecar` | off | Also write a synced `.lrc` sidecar next to each song. MP3 also gets a timed `SYLT` frame when Suno has alignment. |
| `--video-mp4` | off | Also download the standalone `.mp4` music video beside each song, when available. |
| `--download-stems` | off | Also mirror each song's already-generated stems into a `<song>.stems/` sub-folder. Download-only: it lists and downloads existing stems and never triggers separation or spends credits. |
| `--stem-format <wav\|mp3>` | `wav` | Container for downloaded stems. Stems are stored RAW and are never transcoded to FLAC. |
| `--naming-template <TEMPLATE>` | `{creator}/{album}/{creator}-{title} [{id8}]` | Relative path template. Placeholders: `{creator}`, `{handle}`, `{album}`, `{title}`, `{id}`, `{id8}`, `{root_id8}`. |
| `--character-set <unicode\|ascii>` | `unicode` | Character set for filename sanitisation. |

When `sync` would delete files and `--yes` was not passed, it lists them and
asks for confirmation on an interactive terminal. Without a terminal it refuses
and asks you to pass `--yes` or use `copy`.

### Scoped runs (`--liked` and `--playlist`)

`--liked` and `--playlist` narrow a run to a subset of your library. `--playlist`
takes a playlist id or name, is repeatable, and resolves against your own
non-trashed playlists (shared and trashed playlists are not visible); an unknown
or ambiguous value fails and prints the visible playlists. `--playlist liked` is
an alias for `--liked`. Clips that appear in more than one scope are downloaded
once.

A bare scoped run is **additive**: `--liked` and `--playlist` default to `copy`,
so like `--limit` and `--since` they never delete. Adding `--mode mirror` arms
deletion for the scope, but `rs-suno` then also lists your whole library as an
invisible copy protector, so a scoped mirror deletes only the orphans of the
scope it was pointed at and never a file that lives elsewhere in your library
(see [deletion safety](sync-copy-and-deletion-safety.md)). A scoped mirror
maintains the `.m3u8` only for the playlists it enumerated; a full-library `sync`
maintains every playlist's `.m3u8`. For a durable per-area mode, use the
`[areas]` config (see the configuration guide).

```bash
# Mirror only your liked songs:
suno sync /music/suno --liked --mode mirror

# Mirror two playlists by name and id:
suno sync /music/suno --playlist "Neon Nights" --playlist 6f1e...c3
```

```bash
# Mirror everything to the configured root, in FLAC:
suno sync

# Mirror the last two weeks to a specific directory, in MP3:
suno sync /music/suno --format mp3 --since 2w
```

## copy

Additive download and update: same selection and flags as `sync`, but it never
deletes and never prompts.

```bash
suno copy /music/suno-archive
```

## check

Report what `sync` or `copy` would do without writing anything. It accepts every
`sync` flag.

```text
suno check [OPTIONS] [DEST]
```

| Flag | Description |
|---|---|
| `--exit-code` | Exit 1 when changes are pending, 0 when up to date (useful in CI). |

```bash
suno check /music/suno --exit-code
```

`check` never touches disk, so it is safe to run at any time.

## ls

List selected clips as a tab-separated table (`ID`, `DURATION`, `TITLE`,
`TAGS`). The title is truncated to 48 characters. A header prints only to a
terminal, so piping stays clean.

```text
suno ls [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `--liked` | off | List only liked clips. |
| `--limit <N>` | | Stop after the first N clips. |
| `--since <SPEC>` | | Show clips newer than `7d`, `2w`, or `last-run`. |
| `--format <text\|json>` | `text` | Output format; `json` matches `lsjson`. |

```bash
suno ls --limit 20
suno ls --liked | column -t -s $'\t'
```

## lsjson

List selected clips as newline-delimited JSON (one object per line). Equivalent
to `ls --format json`, and it accepts the same flags. The schema is additive for
scripting: fields are not renamed, and new ones are only appended. The
always-null legacy lineage fields `album_title`, `root_ancestor_id`, and
`lineage_status` were removed once confirmed dead (Suno stopped sending them);
no live response ever populated them. Every remaining field is present on every
object; nullable fields are `null` when Suno supplied no value.

| Field | Type | Description |
|---|---|---|
| `id` | string | Suno clip UUID. |
| `title` | string | Display title; `Untitled` when blank. |
| `status` | string | For example `complete`. |
| `duration` | number | Seconds. |
| `created_at` | string | ISO 8601 UTC. |
| `is_liked` | bool | Whether the clip is liked. |
| `has_vocal` | bool | Whether the clip has a vocal track. |
| `clip_type` | string | For example `gen` or `edit`. |
| `tags` | string | Comma-separated style tags. |
| `prompt` | string \| null | User prompt. |
| `gpt_description_prompt` | string \| null | Auto-generated description prompt. |
| `lyrics` | string \| null | Lyrics text; null if instrumental. |
| `model_name` | string | For example `chirp-v4`. |
| `major_model_version` | string | For example `v4`. |
| `display_name` | string | Account display name. |
| `handle` | string | Account handle. |
| `edited_clip_id` | string \| null | Source clip if this is a remix. |
| `audio_url` | string | Audio CDN URL. |
| `image_url` | string | Cover image URL. |
| `image_large_url` | string | Large cover image URL. |
| `video_url` | string | Clip video URL. |
| `video_cover_url` | string | Video cover image URL. |

```bash
# Titles of liked clips:
suno lsjson --liked | jq -r '.title'
```

## suno-index.json

A `sync` or `copy` that fully enumerates the library writes a single
`suno-index.json` at the library root: a durable, machine-readable catalogue of
every mirrored clip, for offline scripting. Unlike the streamed `lsjson` and the
internal `.suno-manifest.json` engine state, it is a visible file that persists
between runs and reflects the files actually on disk.

The write is best-effort: a failure to write the index never fails an otherwise
successful mirror, because it is regenerable from the manifest, the lineage
store, and the next run. A narrowed run (`--limit` or `--since`) does not write
it, so a rich index from a full run is never regressed to a windowed subset.

The document is a pretty-printed object carrying a `schema_version` and a
`clips` array in clip-id order. The schema is stable for scripting: fields are
only added, never removed or renamed. Genuinely unknown live-only fields are
`null`, never an empty string or `0`.

| Field | Type | Description |
|---|---|---|
| `id` | string | Suno clip UUID (the manifest key). |
| `path` | string | Forward-slash library-relative path to the audio file. |
| `format` | string | `flac`, `mp3`, or `wav`. |
| `size` | number | File size in bytes. |
| `title` | string | Live title, else the archived title, else `Untitled`. |
| `artist` | string \| null | Live display name (`Suno` when blank); null when not seen this run. |
| `handle` | string \| null | Live account handle; null when not seen this run. |
| `album` | string | Raw logical album title; may differ from the sanitised album folder in `path`. |
| `root_id` | string | Resolved lineage root id, or the clip's own id when it is a root. |
| `created_at` | string \| null | Archived creation timestamp; null when unknown. |
| `duration` | number \| null | Seconds; null when not seen this run. |
| `tags` | string \| null | Comma-separated style tags; null when not seen this run. |

```bash
# Paths of every FLAC in the mirror:
jq -r '.clips[] | select(.format == "flac") | .path' suno-index.json
```

## fetch

Download one clip by ID or URL to a path outside any mirrored library. The clip
is written directly and is never tracked or reconciled, so `fetch` never affects
a `sync` destination.

```text
suno fetch [OPTIONS] <ID_OR_URL> [DEST]
```

`ID_OR_URL` is a clip UUID or a Suno URL containing it. `DEST` defaults to the
current directory; when it is a directory the file is named `<id>.<ext>`.

| Flag | Short | Default | Description |
|---|---|---|---|
| `--format <mp3\|flac\|wav>` | | `flac` | Audio format. |
| `--output <PATH>` | `-o` | | Explicit output file path, overriding `DEST` and auto-naming. |

```bash
suno fetch 3f2a1b4c-aaaa-bbbb-cccc-ddddeeee0001
suno fetch https://suno.com/song/3f2a1b4c-... -o track.flac
```

## config

Manage the config file. See [Configuration](configuration.md) for the file
format.

```text
suno config [OPTIONS] <COMMAND>
```

| Subcommand | Usage | Purpose |
|---|---|---|
| `init` | `suno config init` | Interactively create a new config file. |
| `add-account` | `suno config add-account [LABEL]` | Add a new account entry to an existing config file. |
| `show` | `suno config show` | Print the current config with tokens redacted. |

There is no `config path` or `config edit` command in v0.22.0. Use
`suno version` to print the resolved config path, then edit the TOML file with
your editor.

## auth

```text
suno auth [OPTIONS] <COMMAND>
suno auth refresh [ACCOUNT]
```

Re-mint an account's JWT to confirm its stored token still works. With no
account it uses your single configured account, or `--all` to check every one.
See [Authentication](authentication.md).

`refresh` is the only auth subcommand in v0.22.0. There is no `auth login`,
`auth status`, or `auth logout` command; provide or update the Clerk `__client`
token through [Configuration](configuration.md).

## doctor

```text
suno doctor
```

Report the current environment and config state, then for the selected account
live-check authentication and the remaining credits balance. It prints whether
`SUNO_CONFIG`, `SUNO_TOKEN`, and `SUNO_<ACCOUNT>_TOKEN` are set, whether the
config parses, the resolved per-account settings with tokens redacted, and the
detected `ffmpeg`.

With `--account <label>` it checks one configured account; with `--all` it
checks every configured account in turn. Without a config it can still diagnose
an env-only setup using `SUNO_TOKEN` or `--token`.

## version

```text
suno version
```

Print the build version and target, the resolved config path, and the detected
`ffmpeg`.

## completions

```text
suno completions <SHELL>
```

Emit a shell completion script to stdout. `SHELL` is one of `bash`, `zsh`,
`fish`, `powershell`, or `elvish`. Redirect it to the location your shell reads.

```bash
suno completions bash > ~/.local/share/bash-completion/completions/suno
```

## Summaries

A `sync` or `copy` run ends with a summary on stderr:

```text
Sync complete: me
  downloaded    12
  tagged         3
  renamed        1
  deleted        2
  sidecars       8
  skipped      129
  failed         0
  total        155
Duration: 43.2s
```

The `sidecars` line counts external artifact files written this run (`.lrc`,
`.lyrics.txt`, `.details.txt`, cover art, `.mp4`, and playlists); sidecar
removals are counted under `deleted` with the audio deletes. So enabling a
sidecar on an already-synced library reports the files it writes rather than
hiding them as `skipped`.

A `--dry-run` or `check` run reports the pending counts instead, and makes no
changes:

```text
Dry run: me (no changes made)
  to download   12
  to tag         3
  to rename      1
  to delete      2
  sidecars       8
  up to date   129
  total        155
```
