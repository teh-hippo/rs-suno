//! Pure "media extras" generators: M3U8 playlists.
//!
//! Every function here is pure. It takes clip data plus relative paths and
//! returns the text the CLI writes to disk later, with no IO, no clock, and no
//! network, so the logic stays deterministic and is unit-tested in isolation.

use std::fmt::Write as _;

/// One ordered entry in an extended-M3U8 playlist.
///
/// Order is significant: the liked and playlist ordering is preserved exactly
/// as given.
#[derive(Debug, Clone, Copy)]
pub struct M3u8Entry<'a> {
    pub title: &'a str,
    pub duration_secs: f64,
    pub relative_path: &'a str,
}

/// Render an extended-M3U8 playlist from `entries`, preserving their order.
///
/// The output opens with the `#EXTM3U` header, then emits one
/// `#EXTINF:<seconds>,<title>` line followed by the relative path line for each
/// entry. Seconds are rounded to the nearest whole number. Carriage returns and
/// line feeds in the title and path are folded to spaces so a single entry can
/// never break the line structure.
pub fn render_m3u8(entries: &[M3u8Entry<'_>]) -> String {
    let mut out = String::from("#EXTM3U\n");
    for entry in entries {
        let title = to_single_line(entry.title);
        let path = to_single_line(entry.relative_path);
        let seconds = extinf_seconds(entry.duration_secs);
        let _ = write!(out, "#EXTINF:{seconds},{title}\n{path}\n");
    }
    out
}

/// Round a duration in seconds to the nearest whole second for `#EXTINF`.
///
/// Non-finite inputs fold to `0` so the playlist line stays well-formed.
fn extinf_seconds(duration_secs: f64) -> i64 {
    if duration_secs.is_finite() {
        duration_secs.round() as i64
    } else {
        0
    }
}

/// Fold carriage returns and line feeds to spaces, keeping the value on one line
/// so it cannot break the surrounding text format.
fn to_single_line(text: &str) -> String {
    text.replace('\r', "").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m3u8_preserves_order_and_rounds_extinf() {
        let entries = [
            M3u8Entry {
                title: "First",
                duration_secs: 211.6,
                relative_path: "Artist/Album/First.flac",
            },
            M3u8Entry {
                title: "Second, Take",
                duration_secs: 90.5,
                relative_path: "Artist/Album/Second.flac",
            },
            M3u8Entry {
                title: "Third\nLine",
                duration_secs: 30.2,
                relative_path: "Artist/Album/Third.flac",
            },
        ];

        let rendered = render_m3u8(&entries);

        let expected = "#EXTM3U\n\
            #EXTINF:212,First\n\
            Artist/Album/First.flac\n\
            #EXTINF:91,Second, Take\n\
            Artist/Album/Second.flac\n\
            #EXTINF:30,Third Line\n\
            Artist/Album/Third.flac\n";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn m3u8_strips_newlines_but_keeps_commas() {
        let entries = [M3u8Entry {
            title: "Hello, World\r\nSecond, Line",
            duration_secs: 12.0,
            relative_path: "Artist/Track.flac",
        }];

        let rendered = render_m3u8(&entries);

        assert_eq!(
            rendered,
            "#EXTM3U\n#EXTINF:12,Hello, World Second, Line\nArtist/Track.flac\n"
        );
        assert!(!rendered.contains('\r'));
        // Header, one EXTINF line, one path line, and a trailing newline.
        assert_eq!(rendered.lines().count(), 3);
    }

    #[test]
    fn m3u8_empty_list_is_header_only() {
        assert_eq!(render_m3u8(&[]), "#EXTM3U\n");
    }

    #[test]
    fn m3u8_non_finite_duration_is_zero() {
        let entries = [M3u8Entry {
            title: "Unknown",
            duration_secs: f64::NAN,
            relative_path: "Artist/Unknown.flac",
        }];

        assert_eq!(
            render_m3u8(&entries),
            "#EXTM3U\n#EXTINF:0,Unknown\nArtist/Unknown.flac\n"
        );
    }
}
