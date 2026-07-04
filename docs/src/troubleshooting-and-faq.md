# Troubleshooting and FAQ

## Troubleshooting

### ffmpeg is not found

`suno version` ends with `ffmpeg: not found on PATH`. Install ffmpeg and make
sure it is on your `PATH`. See
[Installation and ffmpeg](installation-and-ffmpeg.md#ffmpeg). `rs-suno` needs
ffmpeg to produce FLAC (the default format) and animated WebP covers.

### FLAC or animated covers fail to encode

Your ffmpeg build is missing an encoder. Check it with:

```bash
ffmpeg -hide_banner -encoders | grep -E 'flac|libwebp'
```

If `flac` is missing, download in MP3 or install a fuller ffmpeg build. If
`libwebp`/`libwebp_anim` is missing, drop `--animated-covers` or install an
ffmpeg with animated-WebP support.

### Authentication failed (exit 4)

The stored token has expired or was rejected. Diagnose it first:

```bash
suno doctor --account <account>
```

Then confirm and re-mint it:

```bash
suno auth refresh <account>
```

If that still fails, the session was rotated (you logged out, or Suno reset it).
Get a fresh `__client` token and set the account's `token` in your config file
to the new value (run `suno version` to print the config path). If the account
uses `token_command`, the refreshed secret is picked up automatically on the
next run. See [Authentication](authentication.md).

### "multiple accounts configured; pass --account"

You have more than one account and ran a single-account command without saying
which. Add `--account <label>`, or use `--all` for `sync`/`copy` to run every
account.

### "account has no configured root and no DEST was given"

The account has no `root` in its config and you did not pass a destination. Give
a `DEST` on the command line, or set `root` for the account. `--all` always
needs each account to have a `root`.

### "another suno run is active"

A run holds an exclusive lock (`.suno.lock`) on the destination while it works.
If a previous run crashed, the lock file can be left behind. Once you are sure no
run is active, delete the `.suno.lock` file in the destination directory and try
again.

### A sync aborted with a safety warning (exit 7)

A deletion safety rule stopped the run because the deletion looked wrong: the
listing was empty, or it would have removed a large fraction of your library.
Nothing was deleted. This is usually a transient listing problem, so try again
later. If you genuinely intend a mass deletion, confirm it explicitly with
`--min-newest 0 --yes`. See
[deletion safety](sync-copy-and-deletion-safety.md#deletion-safety).

### Nothing is being deleted

That is expected when you use `--since` or `--limit`. A narrowed or filtered
listing is not authoritative, so deletion is disabled for that run. Run a plain
`sync` (no recency filter) to reconcile deletions.

### A run stopped saying the manifest or lineage store is corrupt

`rs-suno` refuses to run against a damaged `.suno-manifest.json` or
`.suno-lineage.json` rather than risk re-downloading everything or losing
archived lineage. Restore the file from a backup, or move it aside to start
fresh (a fresh manifest will re-verify existing files rather than re-download
unchanged ones).

## FAQ

### What is the difference between sync and copy?

`sync` mirrors, including deleting local files whose clips have left your
library. `copy` only ever adds and updates. Use `copy` if you want an archive
that never loses files.

### How do I preview what a run would do?

Use `check` (or add `--dry-run` to `sync`/`copy`). Both report the pending
changes and touch nothing. `check --exit-code` exits 1 when changes are pending,
for CI.

### Can I mirror more than one account?

Yes. Configure each account with its own token and `root`, then run `--all` to
mirror every account into its own directory, or `--account <label>` for one.
Account roots may not nest inside one another.

### Does fetch need a config?

No. `fetch` can run with just `--token` (or `SUNO_TOKEN`). It downloads a single
clip to a path you choose and never touches a mirrored library.

### Where does my library go?

To the `DEST` you pass, or the account's configured `root`. The config path
itself is shown by `suno version`.

### Which format should I choose?

FLAC (the default) is lossless and carries full metadata and embedded art. MP3
is smaller and widely compatible. WAV is lossless but carries limited metadata,
so lyrics and embedded art are omitted.

### Is it safe to run on a schedule?

Yes. Runs are incremental and resumable, an exclusive lock prevents overlap, and
the deletion safety rules prevent a bad listing from wiping files. See
[Scheduling and exit codes](scheduling-and-exit-codes.md).
