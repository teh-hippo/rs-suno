//! The chaos and full-sync correctness suite.
//!
//! Drives the whole pipeline — [`reconcile`](crate::reconcile) then
//! [`execute`](crate::execute) — end to end over the in-memory doubles across
//! many runs, to prove the one property that matters most: a sync can never
//! damage the user's library.
//!
//! - [`harness`] — the reusable driver and clip/world builders every layer uses.
//! - [`full_sync`] — deterministic, hand-built end-to-end scenarios.
//! - [`stateful`] — a property-based state machine over random multi-run
//!   sequences, checking library-integrity invariants after each run.
//! - [`faults`] — fault injection across the network, disk, and transcode ports.
//! - [`fuzz`] — parser robustness against arbitrary and malformed input.
//!
//! Everything is deterministic and fast: no real network, disk, clock, or
//! sleeping.

mod harness;

mod full_sync;

mod stateful;

mod faults;

mod fuzz;
