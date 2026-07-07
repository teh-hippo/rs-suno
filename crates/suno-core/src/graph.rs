//! The durable lineage graph store: a relational archive of clips, their parent
//! edges, and cached root resolutions.
//!
//! A pure serde type with no IO; the CLI persists it beside the library. The
//! shape is relational (separate `nodes`, `edges`, `resolution_cache`) so it
//! migrates cleanly to SQLite later, and a root's title is read from its node
//! rather than copied into every row where it would go stale.
//!
//! [`LineageStore::update`] is the only mutator, taking the wall clock as a
//! `now` string so the store stays free of IO. The cache is monotonic
//! (HARDENING H3): a resolved root is never downgraded by a later transient
//! miss. Gap-filled (often trashed) ancestors are persisted as nodes so lineage
//! survives Suno's ~30-day trash purge.

mod node;
mod store;

#[cfg(test)]
mod tests;

pub use node::{CacheEntry, Node, StoredEdge};
pub use store::LineageStore;
