# `suno` CLI UX Design

This document specifies the flag surface, output formats, progress/error experience, and help text for `suno`. It is a design document: no Rust code changes are implied here.

---

## 1. Global Options

These flags are accepted by `suno` itself and apply to every subcommand.

| Flag | Short | Type | Default | Env var | Description |
|---|---|---|---|---|---|
| `--account <label>` | | string | | `SUNO_ACCOUNT` | Run against one configured account only. |
| `--all` | | bool | false | | Run every configured account in isolation. Cannot be combined with `--account`. |
| `--config <path>` | | path | platform default¹ | `SUNO_CONFIG` | Path to the TOML config file. |
| `--dry-run` | `-n` | bool | false | `SUNO_DRY_RUN` | Report what would change without writing to disk or deleting files. |
| `--verbose` | `-v` | count | 0 | | Increase verbosity. Repeatable: `-vv` for debug detail. |
| `--quiet` | `-q` | count | 0 | | Decrease verbosity. `-q` suppresses progress; `-qq` suppresses all non-error output. |
| `--format <fmt>` | | enum | `text` | `SUNO_FORMAT` | Output format for `ls` and `lsjson`: `text` or `json`. Subcommand `lsjson` implies `json`. |
| `--yes` | `-y` | bool | false | `SUNO_YES` | Skip interactive confirmation prompts (e.g. destructive `sync`). |
| `--token <token>` | | string | | `SUNO_TOKEN` | Suno `__client` cookie token. Never printed. Overrides config and env for the selected account. |

Precedence for every value: command-line flag > environment variable > config file > compiled default.

Per-account env vars use `SUNO_<LABEL>_TOKEN`, `SUNO_<LABEL>_FORMAT`, etc., where `<LABEL>` is the account label in upper-snake-case (e.g. account `my-lib` uses `SUNO_MY_LIB_TOKEN`).

¹ Platform defaults: `~/.config/suno/config.toml` on Linux/macOS; `%APPDATA%/suno/config.toml` on Windows.

---

## 2. Per-command Flags

### `sync <account>[:source]`

Mirror selected sources: download new clips, update tags and artwork, rename or reformat changed files, and remove local files that are no longer in the source. Deletion is subject to safety rules (see Section 5).

```
suno sync [OPTIONS] <DEST>
```

`<DEST>` is a local directory path. The account and optional source are inferred from `--account` / `--all`, or taken from config.

| Flag | Short | Type | Default | Env var | Description |
|---|---|---|---|---|---|
| `--format <fmt>` | | enum | `flac` | `SUNO_FORMAT` | Audio format: `mp3`, `flac`, `wav`. Per-source config override applies. |
| `--playlists-as-albums` | | bool | false | `SUNO_PLAYLISTS_AS_ALBUMS` | Tag playlists as album names instead of using the lineage album. |
| `--limit <n>` | | uint | | | Mirror only the N most recent clips. |
| `--since <spec>` | | string | | | Mirror clips newer than a relative time (`7d`, `2w`) or `last-run`. |
| `--min-newest <n>` | | uint | 1 | `SUNO_MIN_NEWEST` | Minimum number of newest clips kept even when a recency filter would produce an empty set. |
| `--retries <n>` | | uint | 3 | `SUNO_RETRIES` | Maximum download retry attempts per clip before logging a failure. |
| `--concurrency <n>` | | uint | 4 | `SUNO_CONCURRENCY` | Maximum simultaneous downloads. |
| `--dry-run` | `-n` | bool | false | | (Inherited from global; listed here as it is especially relevant to `sync`.) |
| `--yes` | `-y` | bool | false | | (Inherited from global.) Skip the destructive-sync confirmation prompt. |

**Deletion safety.** `sync` will not delete any local file when: the listing was empty, the listing was truncated (e.g. a `--limit` was in force), a network error occurred during listing, or the clip is referenced by another source that uses the same destination. `copy` and `archive` sources always win over a `sync` deletion.

### `copy <account>[:source]`

Download and update clips; never delete. Accepts the same flags as `sync` except `--yes` (no prompt needed).

### `check`

Report what `sync` or `copy` would do without touching disk. Equivalent to passing `--dry-run` to those commands, but prints a richer diff-style summary by default.

```
suno check [OPTIONS] <DEST>
```

Accepts all flags accepted by `sync`. Always exits non-zero when changes are pending, zero when the destination is already up to date.

### `ls`

List selected clips in human-readable form.

```
suno ls [OPTIONS]
```

| Flag | Short | Type | Default | Env var | Description |
|---|---|---|---|---|---|
| `--token <token>` | | string | | `SUNO_TOKEN` | (Inherited from global.) |
| `--liked` | | bool | false | | List only liked clips. |
| `--limit <n>` | | uint | | | Stop after the first N clips. |
| `--since <spec>` | | string | | | Show clips newer than a relative time (`7d`, `2w`) or `last-run`. |
| `--format <fmt>` | | enum | `text` | | Output format: `text` or `json`. `json` is equivalent to `lsjson`. |

### `lsjson`

List selected clips as newline-delimited JSON. Equivalent to `ls --format json`. Accepts the same flags as `ls`.

### `fetch <id-or-url>`

Download a specific clip by ID or URL to a path outside any managed root. The clip is never tracked or reconciled.

```
suno fetch [OPTIONS] <ID_OR_URL> [DEST]
```

`<DEST>` defaults to the current directory. If `<DEST>` is a directory, the file is named `<id>.<ext>`.

| Flag | Short | Type | Default | Env var | Description |
|---|---|---|---|---|---|
| `--token <token>` | | string | | `SUNO_TOKEN` | (Inherited from global.) |
| `--format <fmt>` | | enum | `flac` | `SUNO_FORMAT` | Audio format: `mp3`, `flac`, `wav`. |
| `--output <path>` | `-o` | path | | | Explicit output file path, overriding `<DEST>` and auto-naming. |

### `config init`

Interactively create a new config file. Prompts for account label and token, then writes the file to the default path (or `--config`). Does not overwrite an existing file without `--yes`.

```
suno config init [OPTIONS]
```

| Flag | Short | Type | Default | Description |
|---|---|---|---|---|
| `--yes` | `-y` | bool | false | Overwrite an existing config without prompting. |

### `config add-account`

Add a new account entry to an existing config file. Prompts for label and token.

```
suno config add-account [OPTIONS] [LABEL]
```

| Flag | Short | Type | Default | Description |
|---|---|---|---|---|
| `--token <token>` | | string | | Token for the new account (hidden in help). |

### `config show`

Print the current config. Tokens are always redacted; the field shows `[redacted]`.

```
suno config show
```

No additional flags.

### `auth refresh <account>`

Re-authenticate one account explicitly by re-minting the short-lived JWT from the stored `__client` cookie. Use this after a session expiry warning.

```
suno auth refresh [OPTIONS] <ACCOUNT>
```

`<ACCOUNT>` is the account label. Falls back to `--account` or `--all` if not provided.

### `version`

Print version, build target, config path, and the detected `ffmpeg` version.

```
suno version
```

No additional flags. Example output:

```
suno 0.1.0 (x86_64-unknown-linux-gnu)
config: /home/alice/.config/suno/config.toml
ffmpeg: 6.1.1 (detected at /usr/bin/ffmpeg)
```

### `completions <shell>`

Emit shell completion script to stdout.

```
suno completions <SHELL>
```

`<SHELL>` is one of `bash`, `zsh`, `fish`, `powershell`, `elvish`. Pipe to a file and source it per your shell's documentation.

---

## 3. Output Formats

### `ls` human-readable layout

Tab-separated columns; suitable for piping to `awk` or `column -t`.

```
<id>    <duration>    <title>    <tags>
```

Example:

```
3f2a1b4c-...   182.4s   Electric Storm          ambient, cinematic, orchestral
8d9e0f1a-...    94.1s   Morning Routine          lo-fi, piano, chill
a1b2c3d4-...   217.8s   Neon Horizon             synthwave, retrowave, 80s
```

Title is truncated to 48 characters with a trailing ellipsis when longer. A header line is printed when outputting to a terminal; suppressed when stdout is not a TTY (pipe/redirect).

### `lsjson` JSON schema

One JSON object per line (newline-delimited JSON / NDJSON). The schema is stable for scripting: fields are only ever added, never removed or renamed. All fields are present on every object; nullable fields may be `null` if the API did not supply a value.

```jsonc
{
  "id":                    "string",          // Suno clip UUID
  "title":                 "string",          // Display title; "Untitled" when blank
  "status":                "string",          // "complete", "error", etc.
  "duration":              123.4,             // seconds; 0.0 if unknown
  "created_at":            "2024-01-15T08:30:00Z", // ISO 8601 UTC; empty string if absent
  "is_liked":              true,              // whether the clip is in the liked list
  "has_vocal":             false,             // whether the model included a vocal track
  "clip_type":             "string",          // e.g. "gen" (generated), "edit" (extended/remaster)
  "tags":                  "string",          // comma-separated style tags
  "prompt":                "string | null",   // user-supplied prompt; null if not recorded
  "gpt_description_prompt": "string | null",  // auto-generated description prompt; null if absent
  "lyrics":                "string | null",   // full lyrics text; null if instrumental
  "model_name":            "string",          // e.g. "chirp-v4"
  "major_model_version":   "string",          // e.g. "v4"
  "display_name":          "string",          // account display name
  "handle":                "string",          // account handle
  "album_title":           "string | null",   // lineage album title; null if not in an album
  "root_ancestor_id":      "string | null",   // UUID of the original clip in a lineage; null for roots
  "lineage_status":        "string | null",   // e.g. "root", "continuation"
  "edited_clip_id":        "string | null",   // UUID of the source clip if this is a remix; null otherwise
  "audio_url":             "string",          // permanent CDN MP3 URL
  "image_url":             "string",          // cover image URL
  "image_large_url":       "string",          // large cover image URL
  "video_url":             "string",          // clip video URL
  "video_cover_url":       "string"           // video cover image URL
}
```

Example:

```json
{"id":"3f2a1b4c-aaaa-bbbb-cccc-ddddeeee0001","title":"Electric Storm","status":"complete","duration":182.4,"created_at":"2024-03-10T14:22:01Z","is_liked":true,"has_vocal":false,"clip_type":"gen","tags":"ambient, cinematic, orchestral","prompt":"an orchestral storm building to a climax","gpt_description_prompt":null,"lyrics":null,"model_name":"chirp-v4","major_model_version":"v4","display_name":"alice","handle":"alice","album_title":"Weather Series","root_ancestor_id":null,"lineage_status":"root","edited_clip_id":null,"audio_url":"https://cdn1.suno.ai/3f2a1b4c-aaaa-bbbb-cccc-ddddeeee0001.mp3","image_url":"https://cdn1.suno.ai/image_3f2a1b4c-aaaa-bbbb-cccc-ddddeeee0001.jpeg","image_large_url":"https://cdn1.suno.ai/image_large_3f2a1b4c-aaaa-bbbb-cccc-ddddeeee0001.jpeg","video_url":"","video_cover_url":""}
```

**Note on WAV.** The WAV format carries only a small set of metadata (ID3-v2 or RIFF INFO chunks); extended fields like lyrics, album art, and per-track replay-gain cannot be embedded reliably. When `--format wav` is selected, `suno` will warn that some tags will be omitted. Use FLAC or MP3 if full metadata is required.

---

## 4. Progress and Verbosity UX

Verbosity levels are relative to the default level (0).

| Level | Flag | What is shown |
|---|---|---|
| -1 (quiet) | `-q` | Per-run summary only (counts and path). Warnings and errors still print. |
| -2 (silent) | `-qq` | Errors only. |
| 0 (default) | | Per-run summary, plus a single progress line that updates in place while running. |
| 1 (verbose) | `-v` | Per-song status lines as each download completes or is skipped. |
| 2 (debug) | `-vv` | Per-song detail: tags applied, retry attempts, timing, resolved file paths. |

Progress lines write to stderr so that stdout can carry `lsjson` output cleanly.

### Default progress (level 0)

A single line updates in place while downloads are running:

```
[sync] alice  12 / 147  Electric Storm...
```

On completion, replaced by the summary.

### Verbose progress (level 1, `-v`)

Each completed clip prints one line:

```
  download  3f2a1b4c  Electric Storm                    [182.4s]  albums/Weather Series/01 Electric Storm.flac
  skip      8d9e0f1a  Morning Routine                   already up to date
  tag       a1b2c3d4  Neon Horizon                      tags updated
  delete    b3c4d5e6  Old Draft                         removed (absent from source)
```

### Per-run summary

Printed to stderr at the end of every `sync`, `copy`, or `check` run:

```
Sync complete: alice
  downloaded   12
  tagged        3
  renamed       1
  deleted       2
  skipped     129
  failed        0
  total       147
Duration: 43.2s
```

For `--dry-run` or `check`:

```
Dry run: alice (no changes made)
  to download  12
  to tag        3
  to rename     1
  to delete     2
  up to date  129
  total       147
```

### Persistent logs

Two append-only log files are written alongside the destination directory:

- `.suno-failures.log` -- clips that failed after all retries, with clip ID, title, URL, and error.
- `.suno-audit.log` -- all deletions and renames, with timestamp, clip ID, old path, and new path.

These files are not written in `--dry-run` mode.

---

## 5. Error UX

### Exit codes

| Code | Category | When used |
|---|---|---|
| 0 | Success | All requested work completed without error. |
| 1 | Usage error | Unknown command, invalid flag, or missing required argument. |
| 2 | Configuration error | Config file missing or invalid, unknown account label, conflicting flags. |
| 3 | Authentication failure | Token expired and could not be refreshed; Clerk returned an auth error. |
| 4 | Partial failure | At least one clip failed after all retries; others succeeded. Summary and `.suno-failures.log` written. |
| 5 | Transient failure (exhausted) | Every clip failed with transient errors (network timeouts, 5xx); nothing was downloaded. |
| 6 | Safety abort | A deletion safety rule was triggered (e.g. empty listing, truncated listing). No files were deleted. |
| 7 | Interrupted | SIGINT or SIGTERM received. Partial progress is preserved; resume on next run. |

### Message shapes

**Usage error (1)**

```
error: unexpected argument '--frobnicate'

Usage: suno sync [OPTIONS] <DEST>

For more information, try 'suno sync --help'.
```

**Configuration error (2)**

```
error: account 'production' not found in config

Configured accounts: dev, staging
Run 'suno config add-account production' to add it.
```

**Authentication failure (3)**

```
error: authentication failed for account 'alice'

The stored token has expired. Re-authenticate with:
  suno auth refresh alice

If the token was rotated in Suno, update it with:
  suno config add-account alice --token <new-token>
```

Token expiry warning (printed to stderr during a run, before failure):

```
warning: token for account 'alice' expires in 5 minutes
  Run 'suno auth refresh alice' before the next sync to avoid interruption.
```

**Partial failure (4)**

```
warning: 3 clip(s) failed after 3 retries
  See .suno-failures.log for details.

Sync complete: alice
  downloaded   44
  failed        3
  ...
```

**Transient failure -- exhausted (5)**

```
error: all downloads failed with transient errors

Network may be unreliable or the Suno CDN may be unavailable.
No files were written. Re-run when connectivity is restored.
```

**Safety abort (6)**

```
error: sync aborted -- deletion safety rule triggered

The remote listing returned 0 clips, which would require deleting 147 local file(s).
This is almost certainly a listing error. No files were deleted.

If you intended to delete everything, pass --min-newest 0 --yes to confirm.
```

**Interrupted (7)**

```
warning: interrupted (SIGINT) -- partial run saved

Downloaded 34 of 147 clips before interruption. Re-run to continue.
```

### Destructive `sync` confirmation

When `sync` would delete one or more files and `--yes` was not passed, it prompts interactively:

```
suno sync will delete 5 local file(s) that are no longer in the source:
  albums/Weather Series/03 Old Draft.flac
  albums/Weather Series/04 Scratch Mix.flac
  ... and 3 more (run with -v to see the full list)

Proceed? [y/N]
```

Typing anything other than `y` or `yes` aborts with exit code 0 and no changes.

In non-interactive mode (piped stdin, CI), `sync` requires `--yes` to proceed with deletions; otherwise it aborts with a clear message:

```
error: sync would delete 5 file(s) but stdin is not a TTY and --yes was not passed
  Pass --yes to confirm, or use 'copy' to skip deletions.
```

---

## 6. Help Text

### Top-level `suno --help`

```
A download-only tool for mirroring your Suno.ai library

Usage: suno [OPTIONS] <COMMAND>

Commands:
  sync          Mirror a source: download, update, and remove local files
  copy          Download and update, never delete
  check         Report what sync or copy would change without touching disk
  ls            List clips in your Suno library
  lsjson        List clips as newline-delimited JSON
  fetch         Download a specific clip by ID or URL
  config        Manage the configuration file
  auth          Manage authentication
  version       Print version and environment information
  completions   Emit shell completion script
  help          Print this message or the help of the given subcommand(s)

Options:
      --account <LABEL>    Run against one configured account [env: SUNO_ACCOUNT]
      --all                Run every configured account in isolation
      --config <PATH>      Path to the config file [env: SUNO_CONFIG]
  -n, --dry-run            Report changes without writing to disk [env: SUNO_DRY_RUN]
  -v, --verbose            Increase verbosity (repeatable: -vv for debug)
  -q, --quiet              Decrease verbosity (repeatable: -qq for errors only)
      --format <FORMAT>    Output format: text, json [env: SUNO_FORMAT] [default: text]
  -y, --yes                Skip confirmation prompts [env: SUNO_YES]
      --token <TOKEN>      __client token [env: SUNO_TOKEN]
  -h, --help               Print help
  -V, --version            Print version
```

### `suno sync --help`

```
Mirror a source: download new clips, update tags and artwork, rename or reformat changed
files, and remove local files that are no longer in the source.

Deletion is subject to safety rules: suno will not delete any local file when the remote
listing is empty, truncated, or failed; or when a clip is still present in another source
sharing the same destination.

Usage: suno sync [OPTIONS] <DEST>

Arguments:
  <DEST>  Local directory to mirror into

Options:
      --format <FORMAT>         Audio format: mp3, flac, wav [env: SUNO_FORMAT] [default: flac]
      --playlists-as-albums     Tag playlists as album names [env: SUNO_PLAYLISTS_AS_ALBUMS]
      --limit <N>               Mirror only the N most recent clips
      --since <SPEC>            Mirror clips newer than a relative time (e.g. 7d, 2w, last-run)
      --min-newest <N>          Minimum newest clips kept when a recency filter applies [default: 1]
      --retries <N>             Download retry attempts per clip [env: SUNO_RETRIES] [default: 3]
      --concurrency <N>         Simultaneous downloads [env: SUNO_CONCURRENCY] [default: 4]
  -n, --dry-run                 Report changes without writing to disk
  -y, --yes                     Skip the deletion confirmation prompt
      --account <LABEL>         Run against one configured account [env: SUNO_ACCOUNT]
      --all                     Run every configured account in isolation
  -v, --verbose                 Increase verbosity (repeatable)
  -q, --quiet                   Decrease verbosity (repeatable)
  -h, --help                    Print help
```

---

## 7. Open Questions

- **Config file schema.** The TOML structure (account sections, per-source overrides, global defaults) needs a full specification. In particular: how are multiple sources per account described, and how does the destination path relate to the `sync`/`copy` argument?

- **Source identifiers.** The `<account>[:source]` syntax is mentioned but not defined. What are valid source identifiers -- "liked", "feed", playlist UUIDs? How are they listed?

- **`--since last-run` tracking.** Where is the last-run timestamp persisted, and what happens on the first run or after a failed run?

- **Lineage album naming.** How is the lineage album title derived when `album_title` is blank? Is it the root clip's title, the root clip's ID, or something else?

- **WAV metadata scope.** Which tags are omitted on WAV -- only extended ones (lyrics, art), or also basic ones (title, artist)? The warning text should be precise once this is confirmed.

- **`fetch` and config.** Does `fetch` require an account in config, or can it run with only `--token`?

- **Completion integration.** Should `suno completions` print install instructions (e.g. where to place the file for each shell), or just the raw script?

- **Concurrent account runs.** When `--all` is used, do accounts run sequentially or in parallel? Should there be a `--concurrency` equivalent at the account level?

- **Failure log location.** Is `.suno-failures.log` always placed next to the destination directory, or is its path configurable?

- **`check` exit code.** Should `check` exit 1 when changes are pending (useful in CI), or should it always exit 0? The current proposal uses non-zero when changes are pending, but this may surprise users who run `check` for informational purposes only.
