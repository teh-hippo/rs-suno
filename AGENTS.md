# AGENTS.md

## Stack

Rust (edition 2024, toolchain 1.96+) · Cargo workspace · tokio + reqwest (rustls) · clap · serde_json · ports-and-adapters

## Commands

```bash
cargo fmt --all --check                                   # format check (CI gate)
cargo clippy --all-targets --all-features -- -D warnings  # lint (CI gate)
cargo test --all-features                                 # tests (CI gate)
cargo run -p suno-cli -- ls --limit 5                     # run the CLI
```

Auto-fix: `cargo fmt --all && cargo clippy --fix --all-targets`

The live integration test needs a real token, held in Bitwarden Secrets Manager as secret `SUNO_TOKEN`:

```bash
SUNO_TOKEN="$(bws secret list -o json | jq -r '[.[]|select(.key=="SUNO_TOKEN")][0].value')" \
  cargo run -p suno-cli -- ls --limit 5
```

Never print the token value.

## Structure

```
crates/
  suno-core/   # pure engine, no direct IO
    src/auth.rs      # Clerk cookie -> JWT, lifecycle
    src/client.rs    # SunoClient: feed listing, filter, retry
    src/model.rs     # Clip, mapped from the API JSON shape
    src/http.rs      # the Http port (trait) plus request/response types
    src/consts.rs    # endpoints and tunables
    src/error.rs     # Error and Result
    src/testutil.rs  # in-memory Http double (test only)
  suno-cli/    # thin binary `suno`
    src/main.rs      # clap commands
    src/http.rs      # reqwest adapter implementing Http
```

## Architecture

Ports and adapters. `suno-core` is runtime-agnostic and performs no direct IO; its only window to the network is the `Http` trait, which the CLI implements with reqwest. Disk and ffmpeg will become further ports. Selection, filtering, naming, reconciliation, and tagging stay pure and are unit-tested with in-memory doubles. Dry-run is a recording adapter, never an `if dry_run` branch. The port returns `impl Future + Send` (no async-trait).

## Decisions

- CLI model follows rclone verbs: `sync` mirrors with deletion, `copy` is additive.
- Formats: MP3, FLAC, and WAV; default FLAC; per-source override in TOML.
- Auth: there is no public Suno API key. A Clerk `__client` cookie (a pasted token) mints short-lived JWTs that refresh automatically. The cookie is sent only to Clerk.
- Deletion safety (critical): one file per account; delete only when a clip is absent from every mirror source; copy and archive always win; never delete on an empty, failed, partial, or truncated listing.
- Album model: lineage album by default, with opt-in `--playlists-as-albums` (off).

## Rules

### Must

- Run fmt, clippy (`-D warnings`), and test before committing.
- Use conventional commits (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`).
- Type every signature and keep `suno-core` free of direct IO (use ports).
- Never leak the `__client` token or a JWT in output, logs, or errors.

### Should

- Prefer root-cause refactoring over band-aids; treat code as living.
- Prove a new capability against a real library before locking logic, then lock it with unit tests (preferred over integration); run dry-run integration routinely.
- Keep comments minimal and purposeful.
- Aim for about 80% coverage on `suno-core`.

### Never

- Commit code that fails fmt, clippy, or test.
- Add `if dry_run` branches; use a recording adapter instead.
- Hardcode secrets or print the token.
- Touch code outside the issue's stated scope, or restyle or refactor unrelated files.

## Design and contributing

- The CLI surface, flags, output formats, exit codes, and help text are specified in [docs/cli-ux.md](docs/cli-ux.md). Match it; if you must diverge, say so and update it.
- Exit codes: `0` ok, `1` general error, `2` usage, `3` config, `4` auth, `5` partial, `6` transient-exhausted, `7` safety abort, `8` interrupted. Usage is `2` to match clap's default.
- Keep each change tightly scoped to one module or concern. Add new modules as their own files and export them from `lib.rs`; avoid wide edits that cause integration conflicts.
- We integrate fast-forward only (rebase onto `main`, then `git merge --ff-only`). See [docs/cloud-agents.md](docs/cloud-agents.md) for how cloud-agent work is dispatched and reviewed.

## Reference

This tool reimplements, in spirit and not by code extraction, the `ha-suno` Home Assistant integration, which keeps running as the behaviour reference at `/home/sena/ha-suno`. For the undocumented Suno API and tagging, see `custom_components/suno/{api,auth,audio_metadata,models}.py` and `downloaded_library/` there.

## CI

`.github/workflows/ci.yml` runs fmt, clippy (`-D warnings`), and test on push and pull request to `main`.
