# AGENTS.md

## Stack

Rust (edition 2024, toolchain 1.96+) · Cargo workspace · tokio + reqwest (rustls) · clap · serde_json · ports-and-adapters

## Commands

```bash
cargo fmt --all --check                                   # format check (CI gate)
cargo clippy --all-targets --all-features -- -D warnings  # lint (CI gate)
cargo test --all-features                                 # tests (CI gate)
cargo run -p rs-suno -- ls --limit 5                      # run the CLI
```

Auto-fix: `cargo fmt --all && cargo clippy --fix --all-targets`

The live integration test needs a real token, held in Bitwarden Secrets Manager as secret `SUNO_TOKEN`:

```bash
SUNO_TOKEN="$(bws secret list -o json | jq -r '[.[]|select(.key=="SUNO_TOKEN")][0].value')" \
  cargo run -p rs-suno -- ls --limit 5
```

Never print the token value.

## Structure

```
crates/
  suno-core/   # pure engine, no direct IO
    src/auth.rs        # Clerk cookie -> JWT, lifecycle
    src/client.rs      # SunoClient: feed listing, filter, retry
    src/model.rs       # Clip, mapped from the API JSON shape
    src/http.rs        # the Http port (trait) plus request/response types
    src/reconcile.rs   # desired-vs-local plan and the deletion-safety gates
    src/area.rs        # multi-area sync planner: authority/enumeration predicates
    src/config.rs      # layered settings resolved from one shared `Settings` shape
    src/graph.rs       # lineage node/edge graph + the `LineageStore` on-disk container
    src/roots.rs       # async lineage-root resolver (the IO surface lifted out of lineage)
    src/identity.rs    # account-identity / adoption gate (trust-on-first-use pin)
    src/album_art.rs   # album/playlist art state
    src/consts.rs      # endpoints and tunables
    src/error.rs       # Error and Result
    src/vocab.rs       # shared vocabulary enums (formats, source mode, artifact kind); leaf
    src/testutil.rs    # in-memory Http double (test only)
  rs-suno/     # thin binary `suno`
    src/main.rs        # entry point
    src/http.rs        # reqwest adapter implementing Http
    src/cli/run.rs     # thin sync/copy/check entry plus the run_one orchestrator
    src/cli/*.rs       # one focused concern each: token, account, config_load,
                       # execute, areas, prompt, last_run, signal, ... (see #252)
```

## Architecture

Ports and adapters. `suno-core` is runtime-agnostic and performs no direct IO; its only window to the network is the `Http` trait, which the CLI implements with reqwest. Disk and ffmpeg will become further ports. Selection, filtering, naming, reconciliation, and tagging stay pure and are unit-tested with in-memory doubles. Dry-run is a recording adapter, never an `if dry_run` branch. The port returns `impl Future + Send` (no async-trait).

## Decisions

- CLI model follows rclone verbs: `sync` mirrors with deletion, `copy` is additive.
- Formats: MP3, FLAC, and WAV; default FLAC; per-source override in TOML.
- Auth: there is no public Suno API key. A Clerk `__client` cookie (a pasted token) mints short-lived JWTs that refresh automatically. The cookie is sent only to Clerk.
- Deletion safety (critical): one file per account; delete only when a clip is absent from every mirror source; copy and archive always win; never delete on an empty, failed, partial, or truncated listing.
- Account identity guard (critical): each library is pinned (trust on first use) to its owning Suno account in `.suno-lineage.json`; `sync`, `copy`, and `check` refuse (exit 7) on a token/account mismatch, closing the hole where `mass_delete_abort` is disarmed by `--yes` with `min_newest 0` or by a small library. `--allow-account-change` re-pins and runs additively.
- A disk-full write (or transcode) is systemic, not a per-clip skip: it aborts the run like an auth failure and exits `9`, leaving the library unchanged for the failing action.
- Album model: lineage album only.

## Rules

### Must

- Run fmt, clippy (`-D warnings`), and test before committing.
- Use conventional commits (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`).
- Type every signature and keep `suno-core` free of direct IO (use ports).
- Never leak the `__client` token or a JWT in output, logs, or errors.

### Should

- Prefer root-cause refactoring over band-aids; treat code as living.
- Prefer OS-agnostic code: reach for the standard library or a well-maintained cross-platform crate before hand-rolling `#[cfg(os)]` branches. Where a platform difference is genuine, justify it case by case and isolate it behind a single documented helper (still justifying any new dependency).
- Prove a new capability against a real library before locking logic, then lock it with unit tests (preferred over integration); run dry-run integration routinely.
- Keep comments minimal and purposeful.
- Aim for about 80% coverage on `suno-core`.

### Never

- Commit code that fails fmt, clippy, or test.
- Add `if dry_run` branches; use a recording adapter instead.
- Hardcode secrets or print the token.
- Touch code outside the issue's stated scope, or restyle or refactor unrelated files.

## Testing standards

- Tests are deterministic: no wall clock, no network, no disk. Inject the time, and use the in-memory `Http` double in `testutil.rs` or fixtures.
- Cover failure and edge cases, not just the happy path: empty, missing, and malformed inputs; boundary and overflow values; Unicode and reserved characters.
- Library code never panics on untrusted input; it returns `Result`. Add a test that feeds bad input and asserts an error rather than a panic.
- Test every safety and validation rule explicitly, especially deletion safety and the minimum-newest floor.
- Never let a secret (the `__client` token or a JWT) reach an error, log, or output; where an error can echo user input, add a test asserting it is redacted.
- Ship new `suno-core` modules with their tests; keep about 80% coverage on `suno-core`.

## Design and contributing

- The CLI surface, flags, output formats, exit codes, and help text are documented in the user guide (`docs/src/`, published to GitHub Pages). Match it; if you must diverge, say so and update it.
- Exit codes: `0` ok, `1` general error, `2` usage, `3` config, `4` auth, `5` partial, `6` transient-exhausted, `7` safety abort, `8` interrupted, `9` disk full. Usage is `2` to match clap's default.
- Keep each change tightly scoped to one module or concern. Add new modules as their own files and export them from `lib.rs`; avoid wide edits that cause integration conflicts.
- We integrate fast-forward only (rebase onto `main`, then `git merge --ff-only`).

## Reference

This tool reimplements, in spirit and not by code extraction, the `ha-suno` Home Assistant integration, which keeps running as the behaviour reference at `/home/sena/ha-suno`. For the undocumented Suno API and tagging, see `custom_components/suno/{api,auth,audio_metadata,models}.py` and `downloaded_library/` there.

## CI

`.github/workflows/ci.yml` runs fmt, clippy (`-D warnings`), and test on push and pull request to `main`.
