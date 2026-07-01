# rs-suno

A download-only command line tool for mirroring your Suno.ai library, written in Rust.

Early development. Not yet usable.

## Workspace

- `crates/suno-core` -- pure engine: selection, sync reconciliation, and tagging.
- `crates/rs-suno` -- the `suno` binary: IO adapters and command surface.

## Design

[CLI UX design](docs/cli-ux.md) -- flags, output formats, progress, error experience, and help text.

## Licence

MIT. See [LICENSE](LICENSE).
