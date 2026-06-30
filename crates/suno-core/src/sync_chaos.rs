//! The chaos and full-sync correctness suite.
//!
//! These are not more single-function unit tests; the reconcile invariants
//! (`reconcile::proptests::inv1..inv10`) and the per-action executor tests
//! already cover those. This module drives the *whole* pipeline,
//! [`reconcile`](crate::reconcile) then [`execute`](crate::execute), end to end
//! over the in-memory doubles, across many runs, to prove the single property
//! that matters most: a sync can never damage the user's library.
//!
//! It is organised as the five-layer plan from the strategy doc:
//!
//! - [`harness`] — the reusable driver and clip/world builders every layer uses.
//! - [`full_sync`] — Layer 1: deterministic, hand-built end-to-end scenarios.
//! - [`stateful`] — Layer 2: a property-based state machine over random
//!   multi-run sequences, checking the library-integrity invariants after each
//!   run.
//! - [`faults`] — Layer 3: fault injection across the network, disk, and
//!   transcode ports.
//! - [`fuzz`] — Layer 4: parser robustness against arbitrary and malformed
//!   input.
//!
//! Everything is deterministic and fast: no real network, disk, clock, or
//! sleeping. The recording clock returns immediately, so even retry/backoff
//! paths run in microseconds.

mod harness;

mod full_sync;

mod stateful;

mod faults;

mod fuzz;
