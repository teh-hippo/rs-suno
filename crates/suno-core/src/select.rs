//! Pure clip selection and filtering.

use std::cmp::Reverse;
use std::collections::HashSet;

use crate::model::Clip;

/// A recency filter specification.
pub enum RecencySpec {
    /// Keep clips created within the last N seconds before `now`.
    Relative(u64),
    /// Keep clips created after the last-run timestamp.
    LastRun,
}

impl RecencySpec {
    /// Parse a spec string such as `"7d"`, `"2w"`, or `"last-run"`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        if spec == "last-run" {
            return Ok(RecencySpec::LastRun);
        }
        let split = spec
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(spec.len());
        let (digits, unit) = spec.split_at(split);
        let n: u64 = digits
            .parse()
            .map_err(|_| format!("invalid recency spec: {spec}"))?;
        let secs = match unit {
            "d" => n
                .checked_mul(86_400)
                .ok_or_else(|| format!("recency spec overflows: {spec}"))?,
            "w" => n
                .checked_mul(7)
                .and_then(|v| v.checked_mul(86_400))
                .ok_or_else(|| format!("recency spec overflows: {spec}"))?,
            _ => return Err(format!("unknown unit in recency spec: {spec}")),
        };
        Ok(RecencySpec::Relative(secs))
    }
}

/// Parameters for clip selection.
pub struct SelectParams {
    /// Keep at most the N most recent clips (by `created_at`).
    pub limit: Option<usize>,
    /// Keep only clips newer than this spec.
    pub since: Option<RecencySpec>,
    /// Always retain at least this many newest clips, regardless of the recency filter.
    pub min_newest: usize,
    /// Current Unix timestamp in seconds; used for relative recency specs.
    pub now: u64,
    /// Last-run Unix timestamp in seconds; used when `since` is `RecencySpec::LastRun`.
    pub last_run: Option<u64>,
}

impl Default for SelectParams {
    fn default() -> Self {
        Self {
            limit: None,
            since: None,
            min_newest: 1,
            now: 0,
            last_run: None,
        }
    }
}

/// Produce the final ordered selection from a slice of clips.
///
/// Deduplicates by ID (first occurrence wins), applies recency and limit
/// filters, and enforces the min-newest floor. The original input order is
/// always preserved in the output.
pub fn select<'a>(clips: &'a [Clip], params: &SelectParams) -> Vec<&'a Clip> {
    let mut seen: HashSet<&str> = HashSet::new();
    let deduped: Vec<&Clip> = clips
        .iter()
        .filter(|c| seen.insert(c.id.as_str()))
        .collect();

    let threshold: Option<u64> = match &params.since {
        None => None,
        Some(RecencySpec::Relative(secs)) => Some(params.now.saturating_sub(*secs)),
        Some(RecencySpec::LastRun) => params.last_run,
    };

    // Indices into deduped sorted by clip_ts descending; computed once and
    // reused by both the min-newest floor and the limit step.
    let recency_order: Vec<usize> = {
        let mut idx: Vec<usize> = (0..deduped.len()).collect();
        idx.sort_by_cached_key(|&i| Reverse(clip_ts(deduped[i])));
        idx
    };

    // Apply recency filter. Clips with an unparseable timestamp are kept;
    // they are not given an epoch timestamp that would make them a deletion candidate.
    let mut keep: HashSet<&str> = match threshold {
        Some(t) => deduped
            .iter()
            .filter(|c| parse_timestamp(&c.created_at).is_none_or(|ts| ts > t))
            .map(|c| c.id.as_str())
            .collect(),
        None => deduped.iter().map(|c| c.id.as_str()).collect(),
    };

    // Min-newest floor: when a recency threshold was active and fewer than
    // min_newest clips passed it, pull in enough of the newest clips to meet
    // the floor.
    if threshold.is_some() && keep.len() < params.min_newest {
        for &i in recency_order.iter().take(params.min_newest) {
            keep.insert(deduped[i].id.as_str());
        }
    }

    // Limit: keep only the N most recent. When a recency threshold is active the
    // floor is authoritative, so the effective limit cannot drop below min_newest.
    let effective_limit = params.limit.map(|n| {
        if threshold.is_some() {
            n.max(params.min_newest)
        } else {
            n
        }
    });
    if let Some(n) = effective_limit
        && keep.len() > n
    {
        keep = recency_order
            .iter()
            .filter(|&&i| keep.contains(deduped[i].id.as_str()))
            .take(n)
            .map(|&i| deduped[i].id.as_str())
            .collect();
    }

    deduped
        .into_iter()
        .filter(|c| keep.contains(c.id.as_str()))
        .collect()
}

/// Return the Unix timestamp (seconds) for a clip, or 0 if unparseable.
fn clip_ts(clip: &Clip) -> u64 {
    parse_timestamp(&clip.created_at).unwrap_or(0)
}

/// Parse an ISO 8601 UTC timestamp string to Unix seconds.
///
/// Accepts `YYYY-MM-DDTHH:MM:SS[.fff]Z`.
fn parse_timestamp(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let time = time.split_once('.').map_or(time, |(t, _)| t);
    let mut dp = date.split('-');
    let year: u32 = dp.next()?.parse().ok()?;
    let month: u32 = dp.next()?.parse().ok()?;
    let day: u32 = dp.next()?.parse().ok()?;
    let mut tp = time.split(':');
    let hour: u64 = tp.next()?.parse().ok()?;
    let minute: u64 = tp.next()?.parse().ok()?;
    let second: u64 = tp.next()?.parse().ok()?;
    let days = crate::civil::civil_to_days(year, month, day)?;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(id: &str, created_at: &str) -> Clip {
        Clip {
            id: id.to_string(),
            created_at: created_at.to_string(),
            ..Default::default()
        }
    }

    // --- parse_timestamp ---

    #[test]
    fn parse_timestamp_epoch() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parse_timestamp_one_day() {
        assert_eq!(parse_timestamp("1970-01-02T00:00:00Z"), Some(86_400));
    }

    #[test]
    fn parse_timestamp_with_millis() {
        assert_eq!(
            parse_timestamp("2024-01-15T08:30:00.000Z"),
            parse_timestamp("2024-01-15T08:30:00Z")
        );
    }

    #[test]
    fn parse_timestamp_missing_z_returns_none() {
        assert!(parse_timestamp("2024-01-15T08:30:00").is_none());
    }

    #[test]
    fn parse_timestamp_empty_returns_none() {
        assert!(parse_timestamp("").is_none());
    }

    // --- RecencySpec::parse ---

    #[test]
    fn parse_recency_days() {
        let RecencySpec::Relative(secs) = RecencySpec::parse("7d").unwrap() else {
            panic!("expected Relative");
        };
        assert_eq!(secs, 7 * 86_400);
    }

    #[test]
    fn parse_recency_weeks() {
        let RecencySpec::Relative(secs) = RecencySpec::parse("2w").unwrap() else {
            panic!("expected Relative");
        };
        assert_eq!(secs, 2 * 7 * 86_400);
    }

    #[test]
    fn parse_recency_last_run() {
        assert!(matches!(
            RecencySpec::parse("last-run").unwrap(),
            RecencySpec::LastRun
        ));
    }

    #[test]
    fn parse_recency_invalid_unit() {
        assert!(RecencySpec::parse("3x").is_err());
    }

    #[test]
    fn parse_recency_invalid_number() {
        assert!(RecencySpec::parse("wd").is_err());
    }

    #[test]
    fn parse_recency_overflow_returns_error() {
        assert!(RecencySpec::parse(&format!("{}d", u64::MAX)).is_err());
        assert!(RecencySpec::parse(&format!("{}w", u64::MAX)).is_err());
    }

    // --- select: deduplication ---

    #[test]
    fn dedup_keeps_first_occurrence() {
        let clips = vec![
            clip("a", "2024-01-01T00:00:00Z"),
            clip("b", "2024-01-02T00:00:00Z"),
            clip("a", "2024-01-03T00:00:00Z"),
        ];
        let result = select(&clips, &SelectParams::default());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "a");
        assert_eq!(result[1].id, "b");
    }

    // --- select: order preservation ---

    #[test]
    fn preserves_original_order() {
        // Clips are given newest-last; select must not reorder them.
        let clips = vec![
            clip("a", "2024-01-03T00:00:00Z"),
            clip("b", "2024-01-01T00:00:00Z"),
            clip("c", "2024-01-02T00:00:00Z"),
        ];
        let result = select(&clips, &SelectParams::default());
        assert_eq!(
            result.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    // --- select: limit ---

    #[test]
    fn limit_keeps_n_most_recent() {
        let clips = vec![
            clip("a", "2024-01-01T00:00:00Z"),
            clip("b", "2024-01-03T00:00:00Z"),
            clip("c", "2024-01-02T00:00:00Z"),
        ];
        let params = SelectParams {
            limit: Some(2),
            ..Default::default()
        };
        let result = select(&clips, &params);
        // b (newest) and c should be kept, in original order a=0, b=1, c=2 -> b, c
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "b");
        assert_eq!(result[1].id, "c");
    }

    #[test]
    fn limit_larger_than_set_keeps_all() {
        let clips = vec![
            clip("a", "2024-01-01T00:00:00Z"),
            clip("b", "2024-01-02T00:00:00Z"),
        ];
        let params = SelectParams {
            limit: Some(10),
            ..Default::default()
        };
        assert_eq!(select(&clips, &params).len(), 2);
    }

    // --- select: recency filter ---

    #[test]
    fn since_filters_old_clips() {
        // now = 2024-01-10T00:00:00Z = 1704844800; threshold = now - 7d
        let now = parse_timestamp("2024-01-10T00:00:00Z").unwrap();
        let clips = vec![
            clip("old", "2024-01-01T00:00:00Z"),
            clip("new", "2024-01-05T00:00:00Z"),
        ];
        let params = SelectParams {
            since: Some(RecencySpec::Relative(7 * 86_400)),
            min_newest: 0,
            now,
            ..Default::default()
        };
        let result = select(&clips, &params);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "new");
    }

    #[test]
    fn since_last_run_uses_supplied_timestamp() {
        let last_run = parse_timestamp("2024-01-05T00:00:00Z").unwrap();
        let clips = vec![
            clip("old", "2024-01-04T00:00:00Z"),
            clip("new", "2024-01-06T00:00:00Z"),
        ];
        let params = SelectParams {
            since: Some(RecencySpec::LastRun),
            min_newest: 0,
            last_run: Some(last_run),
            ..Default::default()
        };
        let result = select(&clips, &params);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "new");
    }

    // --- select: min-newest floor ---

    #[test]
    fn min_newest_floor_prevents_empty_selection() {
        let now = parse_timestamp("2024-01-10T00:00:00Z").unwrap();
        let clips = vec![
            clip("a", "2024-01-01T00:00:00Z"),
            clip("b", "2024-01-02T00:00:00Z"),
        ];
        // All clips are older than the 1-day threshold; min_newest=1 should save the newest.
        let params = SelectParams {
            since: Some(RecencySpec::Relative(86_400)),
            min_newest: 1,
            now,
            ..Default::default()
        };
        let result = select(&clips, &params);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "b");
    }

    #[test]
    fn min_newest_floor_keeps_n_when_all_filtered() {
        let now = parse_timestamp("2024-01-10T00:00:00Z").unwrap();
        let clips = vec![
            clip("a", "2024-01-01T00:00:00Z"),
            clip("b", "2024-01-02T00:00:00Z"),
            clip("c", "2024-01-03T00:00:00Z"),
        ];
        let params = SelectParams {
            since: Some(RecencySpec::Relative(86_400)),
            min_newest: 2,
            now,
            ..Default::default()
        };
        let result = select(&clips, &params);
        assert_eq!(result.len(), 2);
        // b and c are the two newest; original order preserved -> b, c
        let ids: Vec<&str> = result.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
    }

    #[test]
    fn min_newest_not_applied_without_recency_filter() {
        // min_newest only kicks in when a since filter is active.
        let clips = vec![
            clip("a", "2024-01-01T00:00:00Z"),
            clip("b", "2024-01-02T00:00:00Z"),
        ];
        let params = SelectParams {
            min_newest: 5,
            ..Default::default()
        };
        assert_eq!(select(&clips, &params).len(), 2);
    }

    #[test]
    fn min_newest_does_not_reduce_passing_set() {
        // When the recency filter already keeps more than min_newest, the floor is a no-op.
        let now = parse_timestamp("2024-01-10T00:00:00Z").unwrap();
        let clips = vec![
            clip("a", "2024-01-08T00:00:00Z"),
            clip("b", "2024-01-09T00:00:00Z"),
        ];
        let params = SelectParams {
            since: Some(RecencySpec::Relative(7 * 86_400)),
            min_newest: 1,
            now,
            ..Default::default()
        };
        assert_eq!(select(&clips, &params).len(), 2);
    }

    // --- select: combined limit + recency + min-newest ---

    #[test]
    fn limit_trims_when_above_min_newest() {
        let now = parse_timestamp("2024-01-10T00:00:00Z").unwrap();
        let clips = vec![
            clip("a", "2024-01-04T00:00:00Z"),
            clip("b", "2024-01-05T00:00:00Z"),
            clip("c", "2024-01-06T00:00:00Z"),
            clip("d", "2024-01-07T00:00:00Z"),
            clip("e", "2024-01-08T00:00:00Z"),
        ];
        // All 5 pass the 7-day threshold; min_newest=2, limit=3;
        // effective_limit=max(3,2)=3 → e, d, c kept in original order.
        let params = SelectParams {
            since: Some(RecencySpec::Relative(7 * 86_400)),
            min_newest: 2,
            limit: Some(3),
            now,
            ..Default::default()
        };
        let result = select(&clips, &params);
        assert_eq!(result.len(), 3);
        let ids: Vec<&str> = result.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["c", "d", "e"]);
    }

    #[test]
    fn limit_below_min_newest_is_clamped_to_floor() {
        let now = parse_timestamp("2024-01-10T00:00:00Z").unwrap();
        let clips = vec![
            clip("a", "2024-01-01T00:00:00Z"),
            clip("b", "2024-01-02T00:00:00Z"),
            clip("c", "2024-01-03T00:00:00Z"),
        ];
        // All fail recency; min_newest=3 but limit=1; floor must win -> all 3 kept.
        let params = SelectParams {
            since: Some(RecencySpec::Relative(86_400)),
            min_newest: 3,
            limit: Some(1),
            now,
            ..Default::default()
        };
        let result = select(&clips, &params);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn unparseable_timestamp_is_kept_through_recency_filter() {
        let now = parse_timestamp("2024-01-10T00:00:00Z").unwrap();
        let clips = vec![clip("good", "2024-01-09T00:00:00Z"), clip("bad_ts", "")];
        let params = SelectParams {
            since: Some(RecencySpec::Relative(7 * 86_400)),
            min_newest: 0,
            now,
            ..Default::default()
        };
        let result = select(&clips, &params);
        assert_eq!(result.len(), 2);
    }
}
