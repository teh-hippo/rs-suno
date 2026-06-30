//! Core engine for rs-suno: library selection, sync reconciliation, and tagging.
//!
//! Pure and side-effect free. Network, disk, and ffmpeg access live behind IO
//! ports that the CLI implements, so the engine stays testable in isolation.
