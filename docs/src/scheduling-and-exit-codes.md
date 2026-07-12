# Scheduling and exit codes

`rs-suno` is built to run unattended. Runs are incremental and resumable, and
every outcome maps to a distinct exit code so a scheduler or CI job can react
correctly.

## Exit codes

| Code | Meaning | When |
|---|---|---|
| 0 | ok | All requested work completed. |
| 1 | general error | An unexpected, uncategorised failure. |
| 2 | usage | Unknown command, invalid flag, or missing argument, for example `--allow-account-change` on `check` or `--dry-run`. |
| 3 | config | Missing or invalid config, unknown account, conflicting flags. |
| 4 | auth | The token expired or was rejected and could not be refreshed. |
| 5 | partial | Some clips failed after all retries; others succeeded. |
| 6 | transient-exhausted | Every clip failed with transient errors; nothing progressed. |
| 7 | safety abort | A deletion safety rule triggered, or the token authenticates as a different account than the library is pinned to; no files were changed. |
| 8 | interrupted | The run received an interrupt; partial progress is preserved. |
| 9 | disk full | The destination ran out of space; free space and re-run. The library is unchanged for the failing action. |

`check --exit-code` is the exception to this table: it exits 1 to signal that
changes are pending, and 0 when the destination is already up to date.

## Running unattended

For a scheduled job, avoid interactive prompts:

- Use `copy` if you never want deletions. It never prompts.
- Use `sync --yes` if you do want the full mirror including deletions. Without a
  terminal, a `sync` that would delete files refuses unless `--yes` is passed.
- Provide the token from the environment or the config file, not a flag in a
  script.

The deletion safety rules still apply under `--yes`: the fully-enumerated gate
and the mass-deletion abort will still stop a run that looks wrong, and the
account identity guard refuses a run whose token belongs to a different account
than the library is pinned to (also exit 7). See
[Sync, copy and deletion safety](sync-copy-and-deletion-safety.md).

### Incremental top-ups

`--since last-run` mirrors only what changed since the previous successful run,
using a timestamp kept beside the library. It is a cheap way to run often.
Remember that any recency filter (`--since` or `--limit`) disables deletion for
that run, so pair frequent top-ups with an occasional full `sync` if you want
deletions reconciled.

### cron

```cron
# Full mirror every night at 02:30, additive only.
30 2 * * *  SUNO_TOKEN=... /usr/local/bin/suno copy /music/suno >> /var/log/suno.log 2>&1
```

Prefer keeping the token in the config file (readable only by your user) over
putting it in the crontab.

### systemd timer

`~/.config/systemd/user/suno.service`:

```ini
[Unit]
Description=Mirror the Suno library
After=network-online.target

[Service]
Type=oneshot
ExecStart=%h/.cargo/bin/suno sync --yes
```

`~/.config/systemd/user/suno.timer`:

```ini
[Unit]
Description=Run suno sync daily

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

Enable it with:

```bash
systemctl --user enable --now suno.timer
```

`Persistent=true` catches up a run missed while the machine was off, which pairs
well with `rs-suno` being resumable.

### Containers

Keep the image one-shot and let cron or a systemd timer invoke
`podman run --rm`. This preserves the `suno` process exit code for the host
scheduler and avoids running a second init system inside the image.

The [container guide](containers.md) includes Podman, Docker, systemd, cron,
and native Proxmox VE examples.

## After a run

- The per-run summary reports counts and duration on stderr.
- Clips that failed after all retries are listed in `.suno-failures.log`.
- Every deletion and rename is recorded in `.suno-audit.log`.

Because runs are resumable, a job that exits 5, 6, or 8 can simply be run again;
it continues from where it left off. An exit of 4 means the token needs
attention (see [Authentication](authentication.md)); an exit of 7 means a safety
rule stopped a suspicious deletion and should be investigated before forcing it.
