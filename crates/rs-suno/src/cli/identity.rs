//! CLI-layer application of the account-identity guard.
//!
//! The pure verdicts live in [`suno_core`] ([`owner_gate`](suno_core::owner_gate)
//! and [`adopt_decision`](suno_core::adopt_decision)); this module applies their
//! side effects for a run: pin the durable [`LineageStore`], mark it dirty,
//! queue the deferred pin notice, and arm or disarm deletion via
//! `force_additive`. Each application returns an [`IdentityOutcome`] the caller
//! prints, so the wiring is unit-testable with just a `LineageStore` and no auth
//! handshake or feed listing.
//!
//! [`Identity`] threads the mutable state across the two identity phases of a
//! run: [`apply_owner_gate`](Identity::apply_owner_gate) before any listing, and
//! [`apply_adopt_decision`](Identity::apply_adopt_decision) once the listing is
//! known.

use std::path::Path;

use suno_core::{AdoptDecision, LineageStore, Owner, OwnerGate};

use crate::cli::desired::ExitCode;
use crate::cli::output::short_id;

/// A pin/adopt/re-pin this run will apply: its audit action (`PIN`, `ADOPT`, or
/// `REPIN`) and the deferred stderr notice. Both are emitted only on the
/// executing path, where the pin is actually persisted, so check/dry-run never
/// claims a pin it never saves (F1).
pub struct PendingPin {
    pub action: &'static str,
    pub notice: String,
}

/// The result of applying an identity decision: either continue the run
/// (optionally printing a notice now) or abort with an exit code and message.
pub enum IdentityOutcome {
    /// Continue. `notice`, when set, is already verbosity-gated and should be
    /// printed immediately.
    Continue { notice: Option<String> },
    /// Refuse the run. The caller prints `message` and returns `code`.
    Abort { code: ExitCode, message: String },
}

/// Everything the identity phases need to build their messages and pin the
/// owner. Built once (after authentication yields the account) and reused for
/// both phases.
pub struct IdentityContext<'a> {
    /// A configured `account_id`, if any (its short prefix appears in the
    /// config-mismatch abort).
    pub configured_id: Option<&'a str>,
    /// The authenticated account's stable id.
    pub user_id: &'a str,
    /// The authenticated account's display name.
    pub account: &'a str,
    /// The destination library (named in the first-use abort).
    pub dest: &'a Path,
    /// Whether `--allow-account-change` was passed.
    pub allow_account_change: bool,
    /// The run's verbosity, gating the `Proceed` no-op notice.
    pub verbosity: i8,
}

/// Mutable identity state threaded across a run's two identity phases.
#[derive(Default)]
pub struct Identity {
    force_additive: bool,
    owner_dirty: bool,
    pending_pin: Option<PendingPin>,
}

impl Identity {
    /// Whether identity forced this run additive (a re-pin or forced adoption),
    /// which disarms every deletion.
    pub fn force_additive(&self) -> bool {
        self.force_additive
    }

    /// Whether the owner pin or display name changed and must be persisted.
    pub fn owner_dirty(&self) -> bool {
        self.owner_dirty
    }

    /// The pin this run will announce and audit on the executing path, if any.
    pub fn pending_pin(&self) -> Option<&PendingPin> {
        self.pending_pin.as_ref()
    }

    /// PHASE 1: apply the [`OwnerGate`] verdict for an authenticated account.
    ///
    /// Sets `force_additive` from the gate, then either aborts (a configured-id
    /// or owner mismatch), re-pins additively, refreshes the display name of a
    /// matching owner (noting a no-op `--allow-account-change`), or defers a
    /// first-use library to PHASE 2.
    pub fn apply_owner_gate(
        &mut self,
        store: &mut LineageStore,
        gate: OwnerGate,
        ctx: &IdentityContext,
    ) -> IdentityOutcome {
        self.force_additive = gate.is_additive();
        match gate {
            OwnerGate::AbortConfigMismatch => IdentityOutcome::Abort {
                code: ExitCode::Safety,
                message: format!(
                    "error: the configured account_id ({}) does not match the authenticated account (id {}). Refusing to run to protect the library.",
                    short_id(ctx.configured_id.unwrap_or_default()),
                    short_id(ctx.user_id)
                ),
            },
            OwnerGate::AbortMismatch => {
                let pinned = store.owner().expect("mismatch implies a pinned owner");
                IdentityOutcome::Abort {
                    code: ExitCode::Safety,
                    message: format!(
                        "error: this library belongs to {} (id {}) but the token authenticates as {} (id {}). Refusing to run to protect the library. Pass --allow-account-change to re-pin it to the authenticated account, or use a different destination.",
                        pinned.display_name,
                        short_id(&pinned.user_id),
                        ctx.account,
                        short_id(ctx.user_id)
                    ),
                }
            }
            OwnerGate::Repin => {
                let previous = store
                    .owner()
                    .map(|owner| owner.display_name.clone())
                    .unwrap_or_default();
                self.set_pin(
                    store,
                    ctx.user_id,
                    ctx.account,
                    "REPIN",
                    format!(
                        "notice: re-pinned this library from {} to {} (id {}); this run is additive (no deletions). Run 'sync' again to mirror.",
                        previous,
                        ctx.account,
                        short_id(ctx.user_id)
                    ),
                );
                IdentityOutcome::Continue { notice: None }
            }
            OwnerGate::Proceed => {
                if store.refresh_display_name(ctx.account) {
                    self.owner_dirty = true;
                }
                let notice = (ctx.allow_account_change && ctx.verbosity >= 0).then(|| {
                    format!(
                        "notice: --allow-account-change had no effect; this library already belongs to {} (id {}).",
                        ctx.account,
                        short_id(ctx.user_id)
                    )
                });
                IdentityOutcome::Continue { notice }
            }
            OwnerGate::FirstUse => IdentityOutcome::Continue { notice: None },
        }
    }

    /// PHASE 2: apply the [`AdoptDecision`] for a not-yet-pinned library.
    ///
    /// Folds the decision into `force_additive`, then pins a fresh or adopted
    /// library, aborts when a complete listing shares nothing with the existing
    /// library, or leaves a narrowed listing unpinned.
    pub fn apply_adopt_decision(
        &mut self,
        store: &mut LineageStore,
        decision: AdoptDecision,
        ctx: &IdentityContext,
    ) -> IdentityOutcome {
        self.force_additive = self.force_additive || decision.is_additive();
        match decision {
            AdoptDecision::PinFresh => {
                self.set_pin(
                    store,
                    ctx.user_id,
                    ctx.account,
                    "PIN",
                    format!(
                        "notice: pinned this library to {} (id {}).",
                        ctx.account,
                        short_id(ctx.user_id)
                    ),
                );
                IdentityOutcome::Continue { notice: None }
            }
            AdoptDecision::PinAdopt => {
                self.set_pin(
                    store,
                    ctx.user_id,
                    ctx.account,
                    "ADOPT",
                    format!(
                        "notice: adopted this existing library for {} (id {}).",
                        ctx.account,
                        short_id(ctx.user_id)
                    ),
                );
                IdentityOutcome::Continue { notice: None }
            }
            AdoptDecision::AdoptForced => {
                self.set_pin(
                    store,
                    ctx.user_id,
                    ctx.account,
                    "ADOPT",
                    format!(
                        "notice: adopted this library for {} (id {}) despite no overlap; this run is additive (no deletions). Run 'sync' again to mirror.",
                        ctx.account,
                        short_id(ctx.user_id)
                    ),
                );
                IdentityOutcome::Continue { notice: None }
            }
            AdoptDecision::Abort => IdentityOutcome::Abort {
                code: ExitCode::Safety,
                message: format!(
                    "error: none of the authenticated account's clips ({}, id {}) match this library at {}. Refusing to run in case the token authenticates as a different Suno account. Pass --allow-account-change to adopt it, or use a different destination.",
                    ctx.account,
                    short_id(ctx.user_id),
                    ctx.dest.display()
                ),
            },
            AdoptDecision::SkipPin => IdentityOutcome::Continue { notice: None },
        }
    }

    /// Pin the owner in `store`, mark it dirty, and queue the deferred notice.
    fn set_pin(
        &mut self,
        store: &mut LineageStore,
        user_id: &str,
        account: &str,
        action: &'static str,
        notice: String,
    ) {
        store.pin_owner(Owner {
            user_id: user_id.to_owned(),
            display_name: account.to_owned(),
        });
        self.owner_dirty = true;
        self.pending_pin = Some(PendingPin { action, notice });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn owner(id: &str, name: &str) -> Owner {
        Owner {
            user_id: id.to_owned(),
            display_name: name.to_owned(),
        }
    }

    fn ctx<'a>(user_id: &'a str, account: &'a str, dest: &'a Path) -> IdentityContext<'a> {
        IdentityContext {
            configured_id: None,
            user_id,
            account,
            dest,
            allow_account_change: false,
            verbosity: 0,
        }
    }

    fn notice(outcome: &IdentityOutcome) -> Option<&str> {
        match outcome {
            IdentityOutcome::Continue { notice } => notice.as_deref(),
            IdentityOutcome::Abort { .. } => panic!("expected Continue, got Abort"),
        }
    }

    fn abort(outcome: &IdentityOutcome) -> (ExitCode, &str) {
        match outcome {
            IdentityOutcome::Abort { code, message } => (*code, message.as_str()),
            IdentityOutcome::Continue { .. } => panic!("expected Abort, got Continue"),
        }
    }

    #[test]
    fn config_mismatch_aborts_without_pinning() {
        let dest = PathBuf::from("/lib");
        let mut store = LineageStore::new();
        let mut identity = Identity::default();
        let c = IdentityContext {
            configured_id: Some("user_c"),
            ..ctx("user_a", "Alice", &dest)
        };
        let outcome = identity.apply_owner_gate(&mut store, OwnerGate::AbortConfigMismatch, &c);
        let (code, message) = abort(&outcome);
        assert_eq!(code, ExitCode::Safety);
        assert!(message.contains("configured account_id"));
        assert!(store.owner().is_none());
        assert!(!identity.force_additive());
        assert!(!identity.owner_dirty());
        assert!(identity.pending_pin().is_none());
    }

    #[test]
    fn owner_mismatch_aborts_and_leaves_the_pin_untouched() {
        let dest = PathBuf::from("/lib");
        let mut store = LineageStore::new();
        store.pin_owner(owner("user_a", "Alice"));
        let mut identity = Identity::default();
        let outcome = identity.apply_owner_gate(
            &mut store,
            OwnerGate::AbortMismatch,
            &ctx("user_b", "Bob", &dest),
        );
        let (code, message) = abort(&outcome);
        assert_eq!(code, ExitCode::Safety);
        assert!(message.contains("this library belongs to Alice"));
        // The pin is unchanged: a mismatch never re-pins.
        assert_eq!(store.owner().unwrap().user_id, "user_a");
        assert!(!identity.force_additive());
    }

    #[test]
    fn repin_pins_the_new_owner_additively() {
        let dest = PathBuf::from("/lib");
        let mut store = LineageStore::new();
        store.pin_owner(owner("user_a", "Alice"));
        let mut identity = Identity::default();
        let outcome =
            identity.apply_owner_gate(&mut store, OwnerGate::Repin, &ctx("user_b", "Bob", &dest));
        assert!(notice(&outcome).is_none());
        assert!(identity.force_additive(), "re-pin disarms deletion");
        assert!(identity.owner_dirty());
        let pin = identity.pending_pin().expect("a re-pin is queued");
        assert_eq!(pin.action, "REPIN");
        assert!(
            pin.notice
                .contains("re-pinned this library from Alice to Bob")
        );
        assert_eq!(store.owner().unwrap().user_id, "user_b");
    }

    #[test]
    fn proceed_refreshes_a_changed_display_name() {
        let dest = PathBuf::from("/lib");
        let mut store = LineageStore::new();
        store.pin_owner(owner("user_a", "Alice"));
        let mut identity = Identity::default();
        let outcome = identity.apply_owner_gate(
            &mut store,
            OwnerGate::Proceed,
            &ctx("user_a", "Alice Cooper", &dest),
        );
        assert!(notice(&outcome).is_none());
        assert!(
            identity.owner_dirty(),
            "a changed name marks the store dirty"
        );
        assert!(!identity.force_additive());
        assert!(identity.pending_pin().is_none());
        assert_eq!(store.owner().unwrap().display_name, "Alice Cooper");
    }

    #[test]
    fn proceed_notes_a_redundant_allow_account_change() {
        let dest = PathBuf::from("/lib");
        let mut store = LineageStore::new();
        store.pin_owner(owner("user_a", "Alice"));
        let mut identity = Identity::default();
        let c = IdentityContext {
            allow_account_change: true,
            ..ctx("user_a", "Alice", &dest)
        };
        let outcome = identity.apply_owner_gate(&mut store, OwnerGate::Proceed, &c);
        assert!(notice(&outcome).unwrap().contains("had no effect"));
    }

    #[test]
    fn proceed_suppresses_the_note_when_quiet() {
        let dest = PathBuf::from("/lib");
        let mut store = LineageStore::new();
        store.pin_owner(owner("user_a", "Alice"));
        let mut identity = Identity::default();
        let c = IdentityContext {
            allow_account_change: true,
            verbosity: -1,
            ..ctx("user_a", "Alice", &dest)
        };
        let outcome = identity.apply_owner_gate(&mut store, OwnerGate::Proceed, &c);
        assert!(notice(&outcome).is_none());
    }

    #[test]
    fn first_use_defers_without_side_effects() {
        let dest = PathBuf::from("/lib");
        let mut store = LineageStore::new();
        let mut identity = Identity::default();
        let outcome = identity.apply_owner_gate(
            &mut store,
            OwnerGate::FirstUse,
            &ctx("user_a", "Alice", &dest),
        );
        assert!(notice(&outcome).is_none());
        assert!(store.owner().is_none());
        assert!(!identity.force_additive());
        assert!(!identity.owner_dirty());
    }

    #[test]
    fn adopt_pin_fresh_pins_a_new_library() {
        let dest = PathBuf::from("/lib");
        let mut store = LineageStore::new();
        let mut identity = Identity::default();
        let outcome = identity.apply_adopt_decision(
            &mut store,
            AdoptDecision::PinFresh,
            &ctx("user_a", "Alice", &dest),
        );
        assert!(notice(&outcome).is_none());
        let pin = identity.pending_pin().expect("a pin is queued");
        assert_eq!(pin.action, "PIN");
        assert!(pin.notice.contains("pinned this library to Alice"));
        assert!(identity.owner_dirty());
        assert!(!identity.force_additive());
        assert_eq!(store.owner().unwrap().user_id, "user_a");
    }

    #[test]
    fn adopt_existing_pins_without_arming_additive() {
        let dest = PathBuf::from("/lib");
        let mut store = LineageStore::new();
        let mut identity = Identity::default();
        let outcome = identity.apply_adopt_decision(
            &mut store,
            AdoptDecision::PinAdopt,
            &ctx("user_a", "Alice", &dest),
        );
        assert!(notice(&outcome).is_none());
        let pin = identity.pending_pin().expect("a pin is queued");
        assert_eq!(pin.action, "ADOPT");
        assert!(pin.notice.contains("adopted this existing library"));
        assert!(!identity.force_additive());
    }

    #[test]
    fn adopt_forced_pins_additively() {
        let dest = PathBuf::from("/lib");
        let mut store = LineageStore::new();
        let mut identity = Identity::default();
        let outcome = identity.apply_adopt_decision(
            &mut store,
            AdoptDecision::AdoptForced,
            &ctx("user_a", "Alice", &dest),
        );
        assert!(notice(&outcome).is_none());
        let pin = identity.pending_pin().expect("a pin is queued");
        assert_eq!(pin.action, "ADOPT");
        assert!(pin.notice.contains("despite no overlap"));
        assert!(
            identity.force_additive(),
            "forced adoption disarms deletion"
        );
    }

    #[test]
    fn adopt_abort_refuses_and_names_the_destination() {
        let dest = PathBuf::from("/some/library");
        let mut store = LineageStore::new();
        let mut identity = Identity::default();
        let outcome = identity.apply_adopt_decision(
            &mut store,
            AdoptDecision::Abort,
            &ctx("user_a", "Alice", &dest),
        );
        let (code, message) = abort(&outcome);
        assert_eq!(code, ExitCode::Safety);
        assert!(message.contains("/some/library"));
        assert!(store.owner().is_none());
    }

    #[test]
    fn adopt_skip_leaves_the_library_unpinned() {
        let dest = PathBuf::from("/lib");
        let mut store = LineageStore::new();
        let mut identity = Identity::default();
        let outcome = identity.apply_adopt_decision(
            &mut store,
            AdoptDecision::SkipPin,
            &ctx("user_a", "Alice", &dest),
        );
        assert!(notice(&outcome).is_none());
        assert!(store.owner().is_none());
        assert!(!identity.owner_dirty());
        assert!(identity.pending_pin().is_none());
    }

    #[test]
    fn messages_only_ever_show_the_short_id_prefix() {
        let dest = PathBuf::from("/lib");
        let full = "user_abcdefghijklmnop";
        let mut store = LineageStore::new();
        let mut identity = Identity::default();
        let outcome = identity.apply_adopt_decision(
            &mut store,
            AdoptDecision::Abort,
            &ctx(full, "Alice", &dest),
        );
        let (_, message) = abort(&outcome);
        assert!(message.contains("user_abc"), "the 8-char prefix is shown");
        assert!(!message.contains(full), "the full id is never printed");
    }
}
