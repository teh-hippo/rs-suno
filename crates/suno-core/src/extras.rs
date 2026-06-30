//! Pure "media extras" generators: M3U8 playlists, the library index, and
//! per-clip JSON sidecars.
//!
//! Every function here is pure. It takes clip data plus relative paths and
//! returns the text the CLI writes to disk later, with no IO, no clock, and no
//! network, so the logic stays deterministic and is unit-tested in isolation.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::model::Clip;

/// The schema version of the library index document.
///
/// Bump this only when the index shape changes. The field schema is additive:
/// fields may be added, never renamed or repurposed.
const INDEX_SCHEMA_VERSION: u32 = 1;

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

/// One clip's row in the library index.
///
/// The field set is stable and additive: add fields, never rename them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub tags: String,
    pub duration_secs: f64,
    pub format: String,
    pub relative_path: String,
}

/// The serialised shape of the whole-library index.
#[derive(Debug, Serialize)]
struct LibraryIndex<'a> {
    schema_version: u32,
    clips: &'a [IndexEntry],
}

/// Render the whole-library index as a stable, pretty-printed JSON document.
///
/// The document is an object carrying a `schema_version` and a `clips` array,
/// in the given order, suitable for scripting against.
pub fn render_library_index(entries: &[IndexEntry]) -> String {
    let index = LibraryIndex {
        schema_version: INDEX_SCHEMA_VERSION,
        clips: entries,
    };
    serde_json::to_string_pretty(&index).expect("library index serialises")
}

/// Render a per-clip JSON sidecar capturing the clip's metadata.
///
/// The output is a pretty-printed JSON object with stable field names covering
/// identity, content, lineage, and the CDN URLs.
pub fn render_clip_sidecar(clip: &Clip) -> String {
    let sidecar = ClipSidecar::from_clip(clip);
    serde_json::to_string_pretty(&sidecar).expect("clip sidecar serialises")
}

/// The serialised shape of a per-clip sidecar, borrowing from the [`Clip`].
///
/// Field order is the on-disk order and the names are stable; add fields, never
/// rename them.
#[derive(Debug, Serialize)]
struct ClipSidecar<'a> {
    id: &'a str,
    title: &'a str,
    tags: &'a str,
    duration_secs: f64,
    created_at: &'a str,
    display_name: &'a str,
    handle: &'a str,
    prompt: &'a str,
    gpt_description_prompt: &'a str,
    lyrics: &'a str,
    model_name: &'a str,
    major_model_version: &'a str,
    album_title: &'a str,
    root_ancestor_id: &'a str,
    lineage_status: &'a str,
    edited_clip_id: &'a str,
    audio_url: &'a str,
    image_url: &'a str,
    image_large_url: &'a str,
    video_url: &'a str,
    video_cover_url: &'a str,
}

impl<'a> ClipSidecar<'a> {
    fn from_clip(clip: &'a Clip) -> ClipSidecar<'a> {
        ClipSidecar {
            id: &clip.id,
            title: &clip.title,
            tags: &clip.tags,
            duration_secs: clip.duration,
            created_at: &clip.created_at,
            display_name: &clip.display_name,
            handle: &clip.handle,
            prompt: &clip.prompt,
            gpt_description_prompt: &clip.gpt_description_prompt,
            lyrics: &clip.lyrics,
            model_name: &clip.model_name,
            major_model_version: &clip.major_model_version,
            album_title: &clip.album_title,
            root_ancestor_id: &clip.root_ancestor_id,
            lineage_status: &clip.lineage_status,
            edited_clip_id: &clip.edited_clip_id,
            audio_url: &clip.audio_url,
            image_url: &clip.image_url,
            image_large_url: &clip.image_large_url,
            video_url: &clip.video_url,
            video_cover_url: &clip.video_cover_url,
        }
    }
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
    use serde_json::Value;

    #[derive(Debug, Deserialize)]
    struct OwnedIndex {
        schema_version: u32,
        clips: Vec<IndexEntry>,
    }

    fn full_clip() -> Clip {
        Clip {
            id: "clip-1234abcd".to_owned(),
            title: "Electric Storm".to_owned(),
            audio_url: "https://cdn1.suno.ai/clip-1234abcd.mp3".to_owned(),
            image_url: "https://cdn1.suno.ai/image_clip.jpeg".to_owned(),
            image_large_url: "https://cdn1.suno.ai/image_large_clip.jpeg".to_owned(),
            video_url: "https://cdn1.suno.ai/clip.mp4".to_owned(),
            video_cover_url: "https://cdn1.suno.ai/clip_cover.jpeg".to_owned(),
            tags: "ambient, cinematic".to_owned(),
            duration: 211.0,
            created_at: "2024-03-10T14:22:01Z".to_owned(),
            display_name: "alice".to_owned(),
            handle: "alice".to_owned(),
            prompt: "an orchestral storm".to_owned(),
            gpt_description_prompt: "a moody cinematic build".to_owned(),
            lyrics: "thunder rolls".to_owned(),
            model_name: "chirp-v4".to_owned(),
            major_model_version: "v4".to_owned(),
            album_title: "Weather Series".to_owned(),
            root_ancestor_id: "rootid567890".to_owned(),
            lineage_status: "continuation".to_owned(),
            edited_clip_id: "parentid1234".to_owned(),
            ..Clip::default()
        }
    }

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

    fn sample_index() -> Vec<IndexEntry> {
        vec![
            IndexEntry {
                id: "id-1".to_owned(),
                title: "Alpha".to_owned(),
                artist: "alice".to_owned(),
                album: "Weather Series".to_owned(),
                tags: "ambient, cinematic".to_owned(),
                duration_secs: 211.0,
                format: "flac".to_owned(),
                relative_path: "alice/Weather Series/Alpha.flac".to_owned(),
            },
            IndexEntry {
                id: "id-2".to_owned(),
                title: "Beta".to_owned(),
                artist: "bob".to_owned(),
                album: String::new(),
                tags: String::new(),
                duration_secs: 0.0,
                format: "mp3".to_owned(),
                relative_path: "bob/Beta.mp3".to_owned(),
            },
        ]
    }

    #[test]
    fn library_index_round_trips_and_preserves_order() {
        let entries = sample_index();
        let json = render_library_index(&entries);

        let parsed: OwnedIndex = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed.schema_version, INDEX_SCHEMA_VERSION);
        assert_eq!(parsed.clips, entries);
        assert_eq!(
            parsed
                .clips
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            ["id-1", "id-2"]
        );
    }

    #[test]
    fn library_index_is_object_with_clips_array() {
        let value: Value =
            serde_json::from_str(&render_library_index(&sample_index())).expect("valid JSON");
        assert_eq!(value["schema_version"], 1);
        assert!(value["clips"].is_array());
        assert_eq!(value["clips"].as_array().unwrap().len(), 2);
        assert_eq!(value["clips"][0]["format"], "flac");
        assert_eq!(value["clips"][1]["duration_secs"], 0.0);
    }

    #[test]
    fn library_index_empty_has_empty_clips_array() {
        let value: Value = serde_json::from_str(&render_library_index(&[])).expect("valid JSON");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["clips"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn sidecar_contains_expected_fields_and_is_valid_json() {
        let value: Value =
            serde_json::from_str(&render_clip_sidecar(&full_clip())).expect("valid JSON");

        assert_eq!(value["id"], "clip-1234abcd");
        assert_eq!(value["title"], "Electric Storm");
        assert_eq!(value["tags"], "ambient, cinematic");
        assert_eq!(value["duration_secs"], 211.0);
        assert_eq!(value["created_at"], "2024-03-10T14:22:01Z");
        assert_eq!(value["display_name"], "alice");
        assert_eq!(value["handle"], "alice");
        assert_eq!(value["prompt"], "an orchestral storm");
        assert_eq!(value["gpt_description_prompt"], "a moody cinematic build");
        assert_eq!(value["lyrics"], "thunder rolls");
        assert_eq!(value["model_name"], "chirp-v4");
        assert_eq!(value["major_model_version"], "v4");
        assert_eq!(value["album_title"], "Weather Series");
        assert_eq!(value["root_ancestor_id"], "rootid567890");
        assert_eq!(value["lineage_status"], "continuation");
        assert_eq!(value["edited_clip_id"], "parentid1234");
        assert_eq!(value["audio_url"], "https://cdn1.suno.ai/clip-1234abcd.mp3");
        assert_eq!(value["image_url"], "https://cdn1.suno.ai/image_clip.jpeg");
        assert_eq!(
            value["image_large_url"],
            "https://cdn1.suno.ai/image_large_clip.jpeg"
        );
        assert_eq!(value["video_url"], "https://cdn1.suno.ai/clip.mp4");
        assert_eq!(
            value["video_cover_url"],
            "https://cdn1.suno.ai/clip_cover.jpeg"
        );
    }

    #[test]
    fn sidecar_for_default_clip_is_valid_and_empty() {
        let value: Value =
            serde_json::from_str(&render_clip_sidecar(&Clip::default())).expect("valid JSON");
        assert_eq!(value["id"], "");
        assert_eq!(value["title"], "");
        assert_eq!(value["duration_secs"], 0.0);
        assert_eq!(value["audio_url"], "");
    }

    #[test]
    fn sidecar_escapes_reserved_and_unicode_characters() {
        let clip = Clip {
            title: "Quote \" and newline\nend".to_owned(),
            lyrics: "東京\tlyrics".to_owned(),
            ..Clip::default()
        };

        let value: Value = serde_json::from_str(&render_clip_sidecar(&clip)).expect("valid JSON");
        assert_eq!(value["title"], "Quote \" and newline\nend");
        assert_eq!(value["lyrics"], "東京\tlyrics");
    }
}
