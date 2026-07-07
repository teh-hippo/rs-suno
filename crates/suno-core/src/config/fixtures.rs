//! Shared fixtures for the config submodule tests: an empty environment and
//! empty CLI flag overrides, the neutral baseline most precedence tests build on.

use std::collections::HashMap;

use super::FlagOverrides;

pub(super) fn no_env() -> HashMap<String, String> {
    HashMap::new()
}

pub(super) fn no_flags() -> FlagOverrides {
    FlagOverrides::default()
}
