//! Wall-clock reads: the current Unix time and its RFC 3339 rendering.
//!
//! Isolated so the rest of the CLI takes a single, shared view of "now" for
//! selection windows, the last-run marker, and the lineage graph's
//! `first_seen_at`/`last_seen_at` stamps.

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The current UTC instant as an RFC 3339 timestamp (`YYYY-MM-DDThh:mm:ssZ`),
/// used to stamp `first_seen_at`/`last_seen_at` on graph nodes and edges.
pub(crate) fn now_rfc3339() -> String {
    rfc3339_from_unix(now_secs())
}

/// Format Unix seconds as an RFC 3339 UTC timestamp (`YYYY-MM-DDThh:mm:ssZ`),
/// backed by the shared civil-calendar conversion so audit stamps and the core
/// selection parser cannot drift.
pub(crate) fn rfc3339_from_unix(secs: u64) -> String {
    let tod = secs % 86_400;
    let (hour, minute, second) = (tod / 3_600, (tod % 3_600) / 60, tod % 60);
    let (year, month, day) = suno_core::days_to_civil(secs / 86_400);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}
