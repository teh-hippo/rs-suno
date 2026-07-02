# Sync, copy and deletion safety

`sync` is the reason `rs-suno` exists: it keeps a local directory as a faithful
mirror of your Suno library, and it does so without ever putting your files at
risk. This chapter explains what a run does and the rules that make deletion
safe.

## The mirror model

- **`copy`** is additive. It downloads new clips and updates existing files, but
  it never deletes anything.
- **`sync`** is a full mirror. It does everything `copy` does, and it also
  removes local files whose clips are no longer in your library.

Both verbs share the same selection and the same incremental engine. The only
difference is whether local files may be removed.

## What a run does

Each run works in three stages:

1. **Select.** Enumerate the library, liked feed, and playlists, then apply any
   `--limit` or `--since` filter.
2. **Plan.** Compare the desired state against a manifest of what is already on
   disk, and decide a set of actions.
3. **Execute.** Apply the actions: download, re-encode, retag, rename, write
   artwork, and (for `sync`) delete.

A `--dry-run`, or the `check` command, stops after the plan and prints what it
would do, touching nothing.

### Incremental by default

`rs-suno` keeps a manifest beside the destination and only does work that is
needed:

- **Skip unchanged.** A clip whose metadata hash, artwork hash, and file size
  all match the manifest is left alone.
- **Retag and re-art in place.** When only tags or artwork changed, the file is
  updated in place. The audio is not downloaded again.
- **Rename in place.** When only the target path changed (for example a retitled
  clip), the existing file is moved, not re-downloaded.
- **Re-encode on format change.** Changing `--format` replaces the file by
  re-encoding, without pre-deleting the old one.
- **Re-download missing or empty files.** A clip whose local file is absent, or
  is zero bytes, is treated as missing and downloaded again.

This makes repeat runs fast and cheap, which is what makes frequent scheduled
runs practical.

## Deletion safety

Deletion is the one irreversible action, so it is hedged with several
independent rules. All of them must agree before a single file is removed.

### Delete only what has truly left every source

A file is a candidate for deletion only when its clip is absent from **every**
mirror source feeding that destination. A clip that is still present in any
source is kept. In addition:

- **`copy` always wins.** A clip held by a `copy` source is never deleted, even
  if a `sync` source no longer lists it.
- **Private clips are preserved.** A clip marked private is never deleted.
- **Trashed counts as removed.** A clip you have trashed in Suno is treated as
  gone and its local file is removed (unless a `copy` source or the private rule
  preserves it).

### The fully-enumerated gate

`rs-suno` will not delete anything unless the listing it is comparing against was
**fully enumerated**: the feed drained completely, with no transport error and
no truncation, and no narrowing filter was applied. In practice this means:

- A network or listing error disables deletion for that run.
- `--limit` and `--since` narrow the listing, so a run using either **never
  deletes**. Use them freely for quick top-ups without any deletion risk.
- `--liked` and `--playlist` scope a run to a subset of your library, so a scoped
  run **never deletes** either. Scoped runs also skip `.m3u8` playlist
  maintenance, though they may still write folder art for the albums they touch.

A missing clip in a partial or filtered listing might still exist upstream, so it
is never read as a deletion.

### The mass-deletion abort

As a final backstop, a run aborts before deleting when the listing looks
catastrophically wrong:

- An **empty listing** that would delete your whole library is refused.
- A delete that would remove **at least half** of a non-trivial library is
  refused.

Either abort exits with the safety code (7) and removes nothing. If you really
do intend a mass deletion, confirm it explicitly with `--min-newest 0 --yes`. A
stored `min_newest = 0` or a habitual `--yes` alone will not disarm the
empty-listing guard.

### The account identity guard

The mass-deletion abort still has a blind spot: it is disarmed by `--min-newest
0 --yes`, and it does not fire on a small library (below the mass-deletion
floor). Both leave a gap where pointing one account's library at another
account's token would make every local file look absent from the source and be
deleted.

To close that gap, each library remembers the account it belongs to. On its
first run, `suno` pins the library to the authenticated account (trust on first
use): a fresh, empty destination is adopted outright, and an existing library is
adopted only when the account's listing overlaps the clips already on disk.
Once pinned, every later `sync`, `copy`, and `check` compares the authenticated
account against the pin and **refuses to run on a mismatch**, exiting with the
safety code (7) and touching nothing:

```text
error: this library belongs to Alice (id user_abc) but the token authenticates
as Bob (id user_xyz). Refusing to run to protect the library. Pass
--allow-account-change to re-pin it to the authenticated account, or use a
different destination.
```

If you genuinely mean to move a library to a different account, pass
`--allow-account-change`. That run re-pins the library to the authenticated
account and runs **additively** (it deletes nothing, like `copy`), so it can
never wipe the previous account's files in the same step. A subsequent normal
`sync`, now pinned to the new account, mirrors as usual. The flag applies only
to an executing `sync` or `copy`; `check` and `--dry-run` reject it (exit 2)
because they never persist a pin. The same flag also adopts an unpinned legacy
library whose listing shares no clips with the files on disk, in case a genuine
account shares nothing with an older download. The pin lives in the lineage
store (`.suno-lineage.json`); a pin, adoption, or re-pin is recorded in
`.suno-audit.log`.

For a belt-and-braces check you can also set `account_id` in an account's config
(see the configuration guide): the run then refuses before contacting Suno if
the token authenticates as a different id.

### The confirmation prompt

When a `sync` would delete files and you did not pass `--yes`:

- On an interactive terminal, it lists the files and asks `Proceed? [y/N]`.
  Anything other than `y` or `yes` aborts with no changes.
- Without a terminal (a pipe, cron, or CI), it refuses and tells you to pass
  `--yes` or use `copy`.

```text
suno sync will delete 3 local file(s) that are no longer in the source:
  me/Weather/me-Old Draft [b3c4d5e6].flac
  ...
Proceed? [y/N]
```

### Tidying up

After removing files, `sync` prunes any directories left empty, so the tree does
not accumulate stale folders. The destination root itself is always kept.

### Per-song sidecars

The optional per-song sidecars follow the same gated deletion path as the audio,
but they differ in when an on-disk file is removed after the feature is turned
off:

- **Covers (`cover.jpg` / `cover.webp`).** A clip's art URL can be missing for a
  run (the feed omits it, or a fetch fails), so an absent cover is treated as
  UNKNOWN and the existing file is kept. A cover is only removed when its whole
  song leaves every source and the audio is deleted with it.
- **Details (`.details.txt`).** The details dump is always renderable, so once
  the feature is off the sidecar can only be intentionally unwanted. Turning
  `details_sidecar` off therefore removes the existing `.details.txt` on the next
  `sync`, through the same fully-enumerated and preserve gates as any deletion.
- **Lyrics (`.lyrics.txt`).** The lyrics file is only written when a song
  actually has lyrics, so an absent lyrics sidecar is ambiguous: it could mean
  the feature is off, or that a single feed read returned empty lyrics. To avoid
  deleting real lyrics on a transient empty read, lyrics opt out of removal the
  same way covers do. Turning `lyrics_sidecar` off leaves existing `.lyrics.txt`
  files in place; delete them by hand if you want them gone.
- **Untimed lyrics (`.lrc`).** The `.lrc` sidecar is written only when a song has
  lyrics, exactly like `.lyrics.txt`, so it opts out of removal the same way.
  Turning `lrc_sidecar` off leaves existing `.lrc` files in place. The lyrics
  carry no per-line timestamps.

Whichever the case, a sidecar is only ever deleted through the shared gate, so
an incomplete listing or a preserved (private or copy-held) song never loses
one.

## Robustness

Beyond deletion, several rules protect an in-progress run:

- **One run at a time.** A `sync` or `copy` takes an exclusive lock
  (`.suno.lock`) on the destination, so two runs cannot corrupt the same
  library.
- **Atomic writes.** Files are written to a temporary sibling and renamed into
  place, so an interrupted write never leaves a half-written file.
- **Size verification.** A download whose byte count does not match what the
  server promised is rejected as a truncated transfer and retried.
- **Rate-limit backoff.** A `429` response is retried with exponential backoff
  that honours the server's `Retry-After` header.
- **Resumable.** Progress is recorded as it happens, so an interrupted run
  simply continues on the next run. This is what makes unattended cron or
  systemd runs safe.

### Failure handling

Failures are classified so one bad clip never derails a whole run:

- **Authentication failure** stops that account cleanly and re-authenticates on
  the next run.
- **Transient failure** (a timeout, a `5xx`, a rate limit) is retried up to
  `--retries` times, then recorded and skipped.
- **A single clip's failure never aborts the run.** Other clips still download,
  and the failure is reported in the summary and log.

## What a run leaves behind

Alongside the mirrored audio, a run keeps a few dotfiles at the destination,
plus one visible index file:

| File | Purpose |
|---|---|
| `.suno-manifest.json` | The record of what is on disk, used for incremental runs. |
| `.suno-lineage.json` | The durable archive of resolved remix and edit lineage. |
| `.suno-last-run` | Timestamp used by `--since last-run`. |
| `.suno-audit.log` | Append-only log of every deletion and rename. |
| `.suno-failures.log` | Append-only log of clips that failed after all retries. |
| `.suno.lock` | Present only while a run is active. |
| `suno-index.json` | A visible, machine-readable catalogue of the mirror for scripting; written best-effort on a fully-enumerated run. |

The audit and failure logs are not written during a `--dry-run` or `check`, and
neither is `suno-index.json`.

## Recipes

```bash
# Full mirror to the configured root, prompting before any deletion:
suno sync

# Full mirror, unattended (approve deletions up front):
suno sync --yes

# Fast top-up of just the last week, with no deletion risk:
suno sync --since 7d

# See exactly what a mirror would change, changing nothing:
suno check --exit-code
```
