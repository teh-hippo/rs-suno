//! Account-identity guard: the trust-on-first-use pin that arms deletion.
//!
//! A library is pinned (trust on first use) to the Suno account it is first
//! synced against; a later run against a different account refuses rather than
//! treating that account's clips as absent from source and deleting the pinned
//! account's files. [`owner_gate`] is the PHASE 1 verdict for an authenticated
//! account and [`adopt_decision`] the PHASE 2 first-use adoption decision; both
//! are pure so the full matrix is unit-tested here rather than inline in the
//! CLI. The pin itself is the [`Owner`] on the durable
//! [`LineageStore`](crate::LineageStore).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::LineageStore;

/// The identity guard pins a library to the account it is first synced against
/// and refuses to run it against a different account, so a mistyped or swapped
/// token can never make one account's clips look absent from source and delete
/// another account's files. `user_id` is the stable identity; `display_name`
/// is cosmetic (for messages) and refreshed opportunistically on a match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub user_id: String,
    pub display_name: String,
}

/// The PHASE 1 identity verdict: whether an authenticated account may run
/// against a library, computed with no network (see [`owner_gate`]).
///
/// This is the composition that gates deletion, kept pure so the full matrix
/// (including the lock-in cases where a configured id or the owner pin refuses
/// even when `--allow-account-change` is set) is unit-tested here rather than
/// inline in the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerGate {
    /// A configured `account_id` differs from the authenticated id: always
    /// refuse, regardless of `--allow-account-change`.
    AbortConfigMismatch,
    /// The pinned owner differs and re-pinning was not permitted: refuse.
    AbortMismatch,
    /// The pinned owner differs but re-pinning was permitted: pin the new owner
    /// and run additively (no deletions this invocation).
    Repin,
    /// The authenticated account owns this library: proceed (the caller then
    /// refreshes the pinned display name).
    Proceed,
    /// The library is not pinned yet: defer to the PHASE 2 adoption decision.
    FirstUse,
}

impl OwnerGate {
    /// Whether this outcome forces an additive (no-deletion) run.
    pub fn is_additive(self) -> bool {
        matches!(self, OwnerGate::Repin)
    }
}

/// Decide whether an authenticated account may run against a library (PHASE 1).
///
/// A configured `account_id` that differs always aborts, even with
/// `allow_change` set, because it is an explicit operator assertion. Otherwise
/// an unpinned library defers to first-use adoption, a matching owner proceeds,
/// and a differing owner either re-pins (when `allow_change`) or aborts.
pub fn owner_gate(
    store_owner: Option<&Owner>,
    configured_id: Option<&str>,
    authed_user_id: &str,
    allow_change: bool,
) -> OwnerGate {
    if let Some(configured) = configured_id
        && configured != authed_user_id
    {
        return OwnerGate::AbortConfigMismatch;
    }
    match store_owner {
        None => OwnerGate::FirstUse,
        Some(owner) if owner.user_id == authed_user_id => OwnerGate::Proceed,
        Some(_) if allow_change => OwnerGate::Repin,
        Some(_) => OwnerGate::AbortMismatch,
    }
}

/// The PHASE 2 first-use adoption decision for a not-yet-pinned library.
///
/// Computed by [`adopt_decision`] from the account's listed clip ids, the
/// library's already-owned clip ids, whether the listing is complete, and
/// whether `--allow-account-change` was passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptDecision {
    /// The destination holds no clips yet: pin it as a fresh library (normal
    /// mode; a fresh library has nothing to delete).
    PinFresh,
    /// A complete listing overlaps the existing library: same account, pin it
    /// (normal mode).
    PinAdopt,
    /// A complete listing shares nothing with the existing library but
    /// `--allow-account-change` was passed: adopt it and run additively.
    AdoptForced,
    /// A complete listing shares nothing with the existing library and no
    /// override was passed: refuse.
    Abort,
    /// A narrowed (incomplete) listing cannot confirm identity: do not pin.
    SkipPin,
}

impl AdoptDecision {
    /// Whether this outcome forces an additive (no-deletion) run.
    pub fn is_additive(self) -> bool {
        matches!(self, AdoptDecision::AdoptForced)
    }
}

/// Decide how to adopt a not-yet-pinned library from this run's listing.
///
/// An empty library is adopted outright; otherwise identity is confirmed by an
/// overlap between the authenticated account's `listed` clip ids and the
/// library's `owned` clip ids, but only on a fully `enumerated` listing. A
/// complete listing with no overlap is a different (or wiped) account: it
/// refuses, unless `allow_change` opts into a forced additive adoption. A
/// narrowed listing (a `--limit`/`--since` run, where deletion is disabled
/// anyway) cannot confirm identity, so the library is left unpinned.
pub fn adopt_decision(
    listed: &[&str],
    owned: &BTreeSet<&str>,
    enumerated: bool,
    allow_change: bool,
) -> AdoptDecision {
    if owned.is_empty() {
        return AdoptDecision::PinFresh;
    }
    if !enumerated {
        return AdoptDecision::SkipPin;
    }
    if listed.iter().any(|id| owned.contains(id)) {
        AdoptDecision::PinAdopt
    } else if allow_change {
        AdoptDecision::AdoptForced
    } else {
        AdoptDecision::Abort
    }
}

impl LineageStore {
    /// The account this library is pinned to, if any.
    pub fn owner(&self) -> Option<&Owner> {
        self.owner.as_ref()
    }

    /// Pin this library to `owner`, replacing any prior pin.
    pub fn pin_owner(&mut self, owner: Owner) {
        self.owner = Some(owner);
    }

    /// Refresh the pinned owner's display name when it has changed, returning
    /// whether it changed. A no-op when the library is not pinned.
    pub fn refresh_display_name(&mut self, display_name: &str) -> bool {
        match &mut self.owner {
            Some(owner) if owner.display_name != display_name => {
                owner.display_name = display_name.to_owned();
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(id: &str, name: &str) -> Owner {
        Owner {
            user_id: id.to_owned(),
            display_name: name.to_owned(),
        }
    }

    #[test]
    fn refresh_display_name_only_when_changed_and_never_when_unpinned() {
        let mut store = LineageStore::new();
        // Unpinned: nothing to refresh.
        assert!(!store.refresh_display_name("Alice"));
        assert!(store.owner().is_none());

        store.pin_owner(owner("user_a", "Alice"));
        // Same name is a no-op.
        assert!(!store.refresh_display_name("Alice"));
        // A changed name updates and reports the change.
        assert!(store.refresh_display_name("Alice Cooper"));
        assert_eq!(store.owner().unwrap().display_name, "Alice Cooper");
        // The user id is left untouched.
        assert_eq!(store.owner().unwrap().user_id, "user_a");
    }

    #[test]
    fn owner_gate_covers_the_full_matrix() {
        let alice = owner("user_a", "Alice");

        // Unpinned defers to first-use, regardless of the flag.
        assert_eq!(owner_gate(None, None, "user_a", false), OwnerGate::FirstUse);
        assert_eq!(owner_gate(None, None, "user_a", true), OwnerGate::FirstUse);

        // A matching owner proceeds.
        assert_eq!(
            owner_gate(Some(&alice), None, "user_a", false),
            OwnerGate::Proceed
        );

        // A differing owner aborts without the flag, re-pins with it.
        assert_eq!(
            owner_gate(Some(&alice), None, "user_b", false),
            OwnerGate::AbortMismatch
        );
        assert_eq!(
            owner_gate(Some(&alice), None, "user_b", true),
            OwnerGate::Repin
        );

        // A configured id that differs ALWAYS aborts, even with the flag and
        // even on a first-use (unpinned) library.
        assert_eq!(
            owner_gate(Some(&alice), Some("user_c"), "user_a", true),
            OwnerGate::AbortConfigMismatch
        );
        assert_eq!(
            owner_gate(None, Some("user_c"), "user_a", true),
            OwnerGate::AbortConfigMismatch
        );
        // A configured id that matches does not interfere.
        assert_eq!(
            owner_gate(Some(&alice), Some("user_a"), "user_a", false),
            OwnerGate::Proceed
        );

        // Only Repin is additive.
        assert!(OwnerGate::Repin.is_additive());
        for gate in [
            OwnerGate::AbortConfigMismatch,
            OwnerGate::AbortMismatch,
            OwnerGate::Proceed,
            OwnerGate::FirstUse,
        ] {
            assert!(!gate.is_additive());
        }
    }

    #[test]
    fn adopt_decision_covers_every_branch() {
        let owned: BTreeSet<&str> = ["c1", "c2"].into_iter().collect();
        let empty: BTreeSet<&str> = BTreeSet::new();

        // Empty library adopts outright regardless of the listing or the flag.
        assert_eq!(
            adopt_decision(&["x", "y"], &empty, true, false),
            AdoptDecision::PinFresh
        );
        // Non-empty but not enumerated: cannot confirm, so leave it unpinned.
        assert_eq!(
            adopt_decision(&["c1"], &owned, false, false),
            AdoptDecision::SkipPin
        );
        assert_eq!(
            adopt_decision(&["c1"], &owned, false, true),
            AdoptDecision::SkipPin
        );
        // Enumerated with overlap: same account, adopt in normal mode.
        assert_eq!(
            adopt_decision(&["c1", "z"], &owned, true, false),
            AdoptDecision::PinAdopt
        );
        // Enumerated with no overlap: refuse without the flag, force-adopt with.
        assert_eq!(
            adopt_decision(&["z1", "z2"], &owned, true, false),
            AdoptDecision::Abort
        );
        assert_eq!(
            adopt_decision(&["z1", "z2"], &owned, true, true),
            AdoptDecision::AdoptForced
        );

        // Only the forced adoption is additive.
        assert!(AdoptDecision::AdoptForced.is_additive());
        for decision in [
            AdoptDecision::PinFresh,
            AdoptDecision::PinAdopt,
            AdoptDecision::Abort,
            AdoptDecision::SkipPin,
        ] {
            assert!(!decision.is_additive());
        }
    }

    #[test]
    fn older_store_without_owner_loads_as_none_and_pinned_roundtrips() {
        // A store written before the owner field existed loads with owner None.
        let json = r#"{"nodes":{},"edges":[]}"#;
        let store: LineageStore = serde_json::from_str(json).unwrap();
        assert!(store.owner().is_none());
        // An unpinned store omits the field entirely (skip_serializing_if).
        let value = serde_json::to_value(&store).unwrap();
        assert!(value.get("owner").is_none());

        // A pinned store round-trips and serialises the owner.
        let mut pinned = LineageStore::new();
        pinned.pin_owner(owner("user_a", "Alice"));
        let back: LineageStore =
            serde_json::from_str(&serde_json::to_string(&pinned).unwrap()).unwrap();
        assert_eq!(back, pinned);
        assert_eq!(back.owner().unwrap().user_id, "user_a");
    }
}
