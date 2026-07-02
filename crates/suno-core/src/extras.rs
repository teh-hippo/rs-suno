//! Pure "media extras" generators: M3U8 playlists and the library index.
//!
//! Every function here is pure. It takes clip data plus relative paths and
//! returns the text the CLI writes to disk later, with no IO, no clock, and no
//! network, so the logic stays deterministic and is unit-tested in isolation.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::config::AudioFormat;
use crate::graph::LineageStore;
use crate::manifest::Manifest;
use crate::model::Clip;

/// The schema version of the library index document.
///
/// Bump this only when the index shape changes. The field set is additive:
/// fields may be added, never renamed or repurposed.
pub const INDEX_SCHEMA_VERSION: u32 = 1;

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

/// One clip's row in the library index.
///
/// The field set is stable and additive: add fields, never rename them. Genuinely
/// unknown live-only fields are `null` (`Option::None`), never an empty string or
/// `0`, so a consumer can tell "absent from this run's live feed" from "empty".
#[derive(Debug, Serialize)]
struct IndexEntry {
    id: String,
    path: String,
    format: AudioFormat,
    size: u64,
    title: String,
    artist: Option<String>,
    handle: Option<String>,
    album: String,
    root_id: String,
    created_at: Option<String>,
    duration: Option<f64>,
    tags: Option<String>,
}

/// The serialised shape of the whole-library index.
#[derive(Debug, Serialize)]
struct LibraryIndex {
    schema_version: u32,
    clips: Vec<IndexEntry>,
}

/// Render the whole-library index as a stable, pretty-printed JSON document.
///
/// One row per `manifest` entry, in clip-id order (the manifest is a `BTreeMap`,
/// so the order is deterministic), and only clips whose file exists on disk are
/// listed, so the index never advertises a missing file. Durable fields come
/// from the manifest and the archived [`LineageStore`]; live-only fields (artist,
/// handle, duration, tags) come from `live` when the clip was seen this run and
/// are `null` otherwise. The `album` is the raw logical album title, which
/// legitimately differs from the sanitised, truncated album segment inside
/// `path`. The renderer takes no clock, so the output is fully deterministic.
pub fn render_library_index(
    manifest: &Manifest,
    store: &LineageStore,
    live: &HashMap<&str, &Clip>,
) -> String {
    let clips = manifest
        .iter()
        .map(|(id, entry)| {
            let live_clip = live.get(id.as_str()).copied();
            let title = live_clip
                .map(|clip| clip.title.clone())
                .filter(|title| !title.is_empty())
                .or_else(|| {
                    store
                        .node(id)
                        .map(|node| node.title.clone())
                        .filter(|title| !title.is_empty())
                })
                .unwrap_or_else(|| "Untitled".to_owned());
            let artist =
                live_clip.map(|clip| non_empty(&clip.display_name).unwrap_or("Suno").to_owned());
            let handle = live_clip.and_then(|clip| non_empty(&clip.handle).map(str::to_owned));
            let album = match live_clip {
                Some(clip) => store.context_for(clip).album(&clip.title),
                None => store.album_for_id(id),
            };
            let root_id = store
                .get_root(id)
                .map(|cached| cached.root_id.clone())
                .filter(|root| !root.is_empty())
                .unwrap_or_else(|| id.clone());
            let created_at = store
                .node(id)
                .map(|node| node.created_at.clone())
                .filter(|created| !created.is_empty());
            let duration = live_clip.map(|clip| clip.duration);
            let tags = live_clip.map(|clip| clip.tags.clone());
            IndexEntry {
                id: id.clone(),
                path: entry.path.clone(),
                format: entry.format,
                size: entry.size,
                title,
                artist,
                handle,
                album,
                root_id,
                created_at,
                duration,
                tags,
            }
        })
        .collect();
    let index = LibraryIndex {
        schema_version: INDEX_SCHEMA_VERSION,
        clips,
    };
    serde_json::to_string_pretty(&index).expect("library index serialises")
}

/// `Some(s)` when `s` is non-empty, else `None`.
///
/// Mirrors the tagger's own emptiness test so the index `artist` agrees with the
/// embedded `ARTIST` tag, including the shared `"Suno"` fallback.
fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
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

    use crate::lineage::{Resolution, ResolveStatus, RootInfo};
    use crate::manifest::ManifestEntry;
    use serde_json::Value;
    use std::collections::HashMap as Map;

    fn manifest_entry(path: &str, format: AudioFormat, size: u64) -> ManifestEntry {
        ManifestEntry {
            path: path.to_owned(),
            format,
            size,
            ..Default::default()
        }
    }

    fn clip(id: &str, title: &str) -> Clip {
        Clip {
            id: id.to_owned(),
            title: title.to_owned(),
            display_name: "alice".to_owned(),
            handle: "alice_handle".to_owned(),
            tags: "ambient, cinematic".to_owned(),
            duration: 211.0,
            created_at: "2024-03-10T14:22:01Z".to_owned(),
            ..Default::default()
        }
    }

    /// A store where `child` is a cover rooted at `root` ("Original"), both nodes
    /// archived with created-at dates.
    fn lineage_store() -> LineageStore {
        let child = Clip {
            id: "child".to_owned(),
            title: "Cover Take".to_owned(),
            created_at: "2024-05-01T00:00:00Z".to_owned(),
            clip_type: "gen".to_owned(),
            task: "cover".to_owned(),
            cover_clip_id: "root".to_owned(),
            edited_clip_id: "root".to_owned(),
            ..Default::default()
        };
        let root = Clip {
            id: "root".to_owned(),
            title: "Original".to_owned(),
            created_at: "2024-04-01T00:00:00Z".to_owned(),
            ..Default::default()
        };
        let mut roots = HashMap::new();
        roots.insert(
            "child".to_owned(),
            RootInfo {
                root_id: "root".to_owned(),
                root_title: "Original".to_owned(),
                status: ResolveStatus::Resolved,
            },
        );
        roots.insert(
            "root".to_owned(),
            RootInfo {
                root_id: "root".to_owned(),
                root_title: "Original".to_owned(),
                status: ResolveStatus::Resolved,
            },
        );
        let resolution = Resolution {
            roots,
            gap_filled: Vec::new(),
        };
        let mut store = LineageStore::new();
        store.update(&[child, root], &resolution, "2024-06-01T00:00:00Z");
        store
    }

    fn parse(rendered: &str) -> Value {
        serde_json::from_str(rendered).expect("index is valid JSON")
    }

    #[test]
    fn index_empty_manifest_is_exact() {
        let rendered = render_library_index(&Manifest::new(), &LineageStore::new(), &Map::new());
        assert_eq!(rendered, "{\n  \"schema_version\": 1,\n  \"clips\": []\n}");
    }

    #[test]
    fn index_schema_version_matches_constant() {
        let value = parse(&render_library_index(
            &Manifest::new(),
            &LineageStore::new(),
            &Map::new(),
        ));
        assert_eq!(value["schema_version"], INDEX_SCHEMA_VERSION);
    }

    #[test]
    fn index_live_clip_uses_live_fields_and_canonical_album() {
        let mut manifest = Manifest::new();
        manifest.insert(
            "child",
            manifest_entry("Original/Cover Take.flac", AudioFormat::Flac, 99),
        );
        let store = lineage_store();
        let clip = clip("child", "Cover Take");
        let mut live: Map<&str, &Clip> = Map::new();
        live.insert("child", &clip);

        let value = parse(&render_library_index(&manifest, &store, &live));
        let row = &value["clips"][0];
        assert_eq!(row["id"], "child");
        assert_eq!(row["path"], "Original/Cover Take.flac");
        assert_eq!(row["format"], "flac");
        assert_eq!(row["size"], 99);
        assert_eq!(row["title"], "Cover Take");
        assert_eq!(row["artist"], "alice");
        assert_eq!(row["handle"], "alice_handle");
        // Canonical album is the resolved root's title, matching context_for.
        assert_eq!(row["album"], "Original");
        assert_eq!(row["root_id"], "root");
        assert_eq!(row["created_at"], "2024-05-01T00:00:00Z");
        assert_eq!(row["duration"], 211.0);
        assert_eq!(row["tags"], "ambient, cinematic");
    }

    #[test]
    fn index_on_disk_clip_nulls_live_only_fields() {
        let mut manifest = Manifest::new();
        manifest.insert(
            "child",
            manifest_entry("Original/Cover Take.flac", AudioFormat::Mp3, 7),
        );
        let store = lineage_store();

        // The clip is not in this run's live set.
        let value = parse(&render_library_index(&manifest, &store, &Map::new()));
        let row = &value["clips"][0];
        // Durable fields are still present, sourced from manifest and store.
        assert_eq!(row["format"], "mp3");
        assert_eq!(row["size"], 7);
        assert_eq!(row["title"], "Cover Take");
        assert_eq!(row["album"], "Original");
        assert_eq!(row["root_id"], "root");
        assert_eq!(row["created_at"], "2024-05-01T00:00:00Z");
        // Live-only fields are null, never empty-string or zero.
        assert!(row["artist"].is_null());
        assert!(row["handle"].is_null());
        assert!(row["duration"].is_null());
        assert!(row["tags"].is_null());
    }

    #[test]
    fn index_album_resolves_for_both_live_and_on_disk_clips() {
        let mut manifest = Manifest::new();
        manifest.insert(
            "child",
            manifest_entry("Original/a.flac", AudioFormat::Flac, 1),
        );
        let store = lineage_store();

        let live_clip = clip("child", "Cover Take");
        let mut live: Map<&str, &Clip> = Map::new();
        live.insert("child", &live_clip);
        let live_value = parse(&render_library_index(&manifest, &store, &live));
        let on_disk_value = parse(&render_library_index(&manifest, &store, &Map::new()));

        // Both paths derive the same canonical album via the shared rule.
        assert_eq!(live_value["clips"][0]["album"], "Original");
        assert_eq!(on_disk_value["clips"][0]["album"], "Original");
    }

    #[test]
    fn index_album_differs_from_sanitised_path_segment() {
        // The path album segment is sanitised and truncated; the index album is
        // the raw logical title, so they legitimately differ. Here the root
        // title carries a slash that naming would never leave in a folder name.
        let mut manifest = Manifest::new();
        manifest.insert(
            "child",
            manifest_entry("AC-DC Live/song.flac", AudioFormat::Flac, 1),
        );
        let raw_root = Clip {
            id: "root".to_owned(),
            title: "AC/DC: Live!".to_owned(),
            created_at: "2024-04-01T00:00:00Z".to_owned(),
            ..Default::default()
        };
        let child = Clip {
            id: "child".to_owned(),
            title: "song".to_owned(),
            clip_type: "gen".to_owned(),
            task: "cover".to_owned(),
            cover_clip_id: "root".to_owned(),
            edited_clip_id: "root".to_owned(),
            ..Default::default()
        };
        let mut roots = HashMap::new();
        roots.insert(
            "child".to_owned(),
            RootInfo {
                root_id: "root".to_owned(),
                root_title: "AC/DC: Live!".to_owned(),
                status: ResolveStatus::Resolved,
            },
        );
        let resolution = Resolution {
            roots,
            gap_filled: Vec::new(),
        };
        let mut store = LineageStore::new();
        store.update(&[child, raw_root], &resolution, "2024-06-01T00:00:00Z");

        let value = parse(&render_library_index(&manifest, &store, &Map::new()));
        let row = &value["clips"][0];
        assert_eq!(row["album"], "AC/DC: Live!");
        // The path segment is the sanitised, slash-free folder.
        let album = row["album"].as_str().unwrap();
        let path_segment = row["path"].as_str().unwrap().split('/').next().unwrap();
        assert_ne!(album, path_segment);
        assert_eq!(path_segment, "AC-DC Live");
    }

    #[test]
    fn index_iterates_in_clip_id_order() {
        let mut manifest = Manifest::new();
        manifest.insert("c", manifest_entry("c.flac", AudioFormat::Flac, 1));
        manifest.insert("a", manifest_entry("a.flac", AudioFormat::Flac, 1));
        manifest.insert("b", manifest_entry("b.flac", AudioFormat::Flac, 1));

        let value = parse(&render_library_index(
            &manifest,
            &LineageStore::new(),
            &Map::new(),
        ));
        let ids: Vec<&str> = value["clips"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn index_unknown_clip_is_well_formed_with_defaults() {
        // A manifest id absent from both live and the store nodes: self-root,
        // "Untitled", null live fields, no panic.
        let mut manifest = Manifest::new();
        manifest.insert("orphan", manifest_entry("orphan.wav", AudioFormat::Wav, 3));

        let value = parse(&render_library_index(
            &manifest,
            &LineageStore::new(),
            &Map::new(),
        ));
        let row = &value["clips"][0];
        assert_eq!(row["id"], "orphan");
        assert_eq!(row["title"], "Untitled");
        assert_eq!(row["format"], "wav");
        assert_eq!(row["album"], "");
        assert_eq!(row["root_id"], "orphan");
        assert!(row["created_at"].is_null());
        assert!(row["artist"].is_null());
        assert!(row["tags"].is_null());
    }

    #[test]
    fn index_title_falls_back_to_store_node_then_untitled() {
        let mut manifest = Manifest::new();
        manifest.insert("child", manifest_entry("x.flac", AudioFormat::Flac, 1));
        let store = lineage_store();
        // No live clip, so title comes from the archived node.
        let value = parse(&render_library_index(&manifest, &store, &Map::new()));
        assert_eq!(value["clips"][0]["title"], "Cover Take");
    }

    #[test]
    fn index_artist_falls_back_to_suno_when_display_name_empty() {
        let mut manifest = Manifest::new();
        manifest.insert("child", manifest_entry("x.flac", AudioFormat::Flac, 1));
        let mut anon = clip("child", "Cover Take");
        anon.display_name = String::new();
        let mut live: Map<&str, &Clip> = Map::new();
        live.insert("child", &anon);
        let value = parse(&render_library_index(
            &manifest,
            &LineageStore::new(),
            &live,
        ));
        // Matches TrackMetadata::from_clip's "Suno" fallback for the ARTIST tag.
        assert_eq!(value["clips"][0]["artist"], "Suno");
    }

    #[test]
    fn index_unicode_round_trips() {
        let mut manifest = Manifest::new();
        manifest.insert("🎵", manifest_entry("音楽/曲.flac", AudioFormat::Flac, 5));
        let unicode = clip("🎵", "音楽 \"quoted\"");
        let mut live: Map<&str, &Clip> = Map::new();
        live.insert("🎵", &unicode);

        let rendered = render_library_index(&manifest, &LineageStore::new(), &live);
        let value = parse(&rendered);
        let row = &value["clips"][0];
        assert_eq!(row["id"], "🎵");
        assert_eq!(row["path"], "音楽/曲.flac");
        assert_eq!(row["title"], "音楽 \"quoted\"");
    }
}
