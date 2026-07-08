//! Small pure text formatters shared by the sidecar and playlist renderers.
//!
//! These have no tag, clip, or IO coupling: they fold a value onto one line and
//! render a `mm:ss` duration. Both the details/m3u8 renderers in `extras` and
//! the `.lrc` renderers in `lyrics` share them, so they live in a neutral leaf.

/// Fold carriage returns and line feeds to spaces, keeping the value on one line
/// so it cannot break the surrounding text format.
pub(crate) fn to_single_line(text: &str) -> String {
    text.replace('\r', "").replace('\n', " ")
}

/// Format a duration in seconds as `mm:ss`, or the empty string when it is
/// non-finite or non-positive (so an unknown duration is omitted, not `00:00`).
pub(crate) fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return String::new();
    }
    let total = secs.round() as i64;
    format!("{}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_folds_newlines_to_spaces_and_drops_carriage_returns() {
        assert_eq!(to_single_line("a\r\nb"), "a b");
        assert_eq!(to_single_line("one\ntwo\nthree"), "one two three");
        assert_eq!(to_single_line("no breaks"), "no breaks");
        assert_eq!(to_single_line(""), "");
        // A lone carriage return is removed, not turned into a space.
        assert_eq!(to_single_line("a\rb"), "ab");
    }

    #[test]
    fn duration_renders_minutes_and_zero_padded_seconds() {
        assert_eq!(format_duration(0.0), "");
        assert_eq!(format_duration(5.0), "0:05");
        assert_eq!(format_duration(59.0), "0:59");
        assert_eq!(format_duration(60.0), "1:00");
        assert_eq!(format_duration(125.0), "2:05");
        assert_eq!(format_duration(3599.0), "59:59");
    }

    #[test]
    fn duration_rounds_to_the_nearest_second() {
        assert_eq!(format_duration(90.4), "1:30");
        assert_eq!(format_duration(90.6), "1:31");
    }

    #[test]
    fn duration_omits_unknown_or_invalid_values() {
        assert_eq!(format_duration(-1.0), "");
        assert_eq!(format_duration(f64::NAN), "");
        assert_eq!(format_duration(f64::INFINITY), "");
        assert_eq!(format_duration(f64::NEG_INFINITY), "");
    }
}
