//! JSON Schema generation for the config file (feature `schema`).
//!
//! Emits a JSON Schema (schemars 0.9, draft 2020-12, which Taplo/Even Better
//! TOML validates) for [`Config`], published to GitHub Pages and referenced by
//! the `#:schema` header directive so editors validate and autocomplete
//! `config.toml`. Kept behind the optional `schema` feature so the shipped
//! binary never links schemars; the schema is regenerated from the same types
//! the parser uses, so it can never drift from them.

use crate::config::Config;

/// The canonical, pretty-printed JSON Schema for the config file, with a
/// trailing newline so it round-trips cleanly through editors and `git`.
pub fn config_schema_json() -> String {
    let schema = schemars::schema_for!(Config);
    let mut json = serde_json::to_string_pretty(&schema).expect("schema serialises");
    json.push('\n');
    json
}

#[cfg(test)]
mod tests {
    use super::config_schema_json;

    /// The published copy, relative to this crate. `mdbook build` copies it from
    /// `docs/src/` to the site root, where the `#:schema` directive points.
    const SCHEMA_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/src/config.schema.json"
    );

    #[test]
    fn checked_in_schema_is_current() {
        let generated = config_schema_json();
        if std::env::var_os("UPDATE_SCHEMA").is_some() {
            std::fs::write(SCHEMA_PATH, &generated).expect("write schema");
            return;
        }
        let on_disk = std::fs::read_to_string(SCHEMA_PATH).unwrap_or_default();
        assert_eq!(
            generated, on_disk,
            "docs/src/config.schema.json is stale; regenerate with \
             `UPDATE_SCHEMA=1 cargo test -p suno-core --features schema checked_in_schema_is_current`"
        );
    }
}
