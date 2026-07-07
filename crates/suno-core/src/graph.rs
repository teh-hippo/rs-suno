//! The durable lineage graph store: a relational archive of clips, their parent
//! edges, and cached root resolutions.
//!
//! This is a pure serde type with no IO of its own; the CLI persists it beside
//! the library (mirroring the manifest). The shape is deliberately relational —
//! separate `nodes`, `edges`, and `resolution_cache` collections rather than an
//! adjacency blob per clip — so it migrates cleanly to SQLite later. A root's
//! title is read from its node, never copied into every row where it would go
//! stale.
//!
//! [`LineageStore::update`] is the only mutator: given the clips seen this run
//! and their [`Resolution`], it upserts nodes and edges and refreshes the
//! resolution cache. The store takes the wall clock as a `now` string from the
//! caller so it stays free of IO. The cache is monotonic (HARDENING H3): a
//! resolved root is never downgraded by a later transient miss. Gap-filled
//! (often trashed) ancestors are persisted as nodes so lineage survives Suno's
//! ~30-day trash purge.

mod node;
mod store;

#[cfg(test)]
mod tests;

pub use node::{CacheEntry, Node, StoredEdge};
pub use store::LineageStore;
