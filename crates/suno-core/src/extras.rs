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
///
/// An **empty `relative_path` marks a member absent from the local library**
/// (Liked from another creator, or filtered out by `--limit`/`--since`). Such an
/// entry renders as a `# (not in library) <title>` comment line rather than an
/// `#EXTINF` + path pair, so the playlist never carries a dangling path
/// (HARDENING L1). A present member always has a non-empty relative path.
#[derive(Debug, Clone, Copy)]
pub struct M3u8Entry<'a> {
    pub title: &'a str,
    pub duration_secs: f64,
    pub relative_path: &'a str,
}

/// Render an extended-M3U8 playlist named `name` from `entries`, preserving
/// their order.
///
/// The output opens with the `#EXTM3U` header and a `#PLAYLIST:<name>` line,
/// then per entry emits either an `#EXTINF:<seconds>,<title>` line followed by
/// the relative path line (a member present in the library), or a
/// `# (not in library) <title>` comment line (an [`M3u8Entry`] with an empty
/// relative path — HARDENING L1). Seconds are rounded to the nearest whole
/// number. Carriage returns and line feeds in the name, title, and path are
/// folded to spaces so a single field can never break the line structure.
pub fn render_m3u8(name: &str, entries: &[M3u8Entry<'_>]) -> String {
    let mut out = String::from("#EXTM3U\n");
    let _ = writeln!(out, "#PLAYLIST:{}", to_single_line(name));
    for entry in entries {
        let title = to_single_line(entry.title);
        if entry.relative_path.is_empty() {
            // L1: a member absent from the local library — a comment, never a
            // dangling path line.
            let _ = writeln!(out, "# (not in library) {title}");
            continue;
        }
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

        let rendered = render_m3u8("Road Trip", &entries);

        let expected = "#EXTM3U\n\
            #PLAYLIST:Road Trip\n\
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

        let rendered = render_m3u8("Mix", &entries);

        assert_eq!(
            rendered,
            "#EXTM3U\n#PLAYLIST:Mix\n#EXTINF:12,Hello, World Second, Line\nArtist/Track.flac\n"
        );
        assert!(!rendered.contains('\r'));
        // Header, playlist name, one EXTINF line, one path line, trailing newline.
        assert_eq!(rendered.lines().count(), 4);
    }

    #[test]
    fn m3u8_folds_newlines_in_the_playlist_name() {
        let rendered = render_m3u8("Road\r\nTrip", &[]);
        assert_eq!(rendered, "#EXTM3U\n#PLAYLIST:Road Trip\n");
    }

    #[test]
    fn m3u8_empty_list_is_header_and_name_only() {
        assert_eq!(render_m3u8("Empty", &[]), "#EXTM3U\n#PLAYLIST:Empty\n");
    }

    #[test]
    fn m3u8_absent_member_renders_a_comment_not_a_path() {
        // L1: an empty relative path means the member is not in the local
        // library, so it is a comment line with no #EXTINF and no path.
        let entries = [
            M3u8Entry {
                title: "In Library",
                duration_secs: 60.0,
                relative_path: "Artist/In.flac",
            },
            M3u8Entry {
                title: "Missing, Song",
                duration_secs: 42.0,
                relative_path: "",
            },
            M3u8Entry {
                title: "Also Present",
                duration_secs: 30.0,
                relative_path: "Artist/Also.flac",
            },
        ];

        let rendered = render_m3u8("Liked Songs", &entries);

        let expected = "#EXTM3U\n\
            #PLAYLIST:Liked Songs\n\
            #EXTINF:60,In Library\n\
            Artist/In.flac\n\
            # (not in library) Missing, Song\n\
            #EXTINF:30,Also Present\n\
            Artist/Also.flac\n";
        assert_eq!(rendered, expected);
        // The absent member never contributes a bare path line.
        assert!(!rendered.contains("#EXTINF:42"));
    }

    #[test]
    fn m3u8_non_finite_duration_is_zero() {
        let entries = [M3u8Entry {
            title: "Unknown",
            duration_secs: f64::NAN,
            relative_path: "Artist/Unknown.flac",
        }];

        assert_eq!(
            render_m3u8("Odd", &entries),
            "#EXTM3U\n#PLAYLIST:Odd\n#EXTINF:0,Unknown\nArtist/Unknown.flac\n"
        );
    }
}
