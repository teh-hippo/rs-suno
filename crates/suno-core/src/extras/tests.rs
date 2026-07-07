use super::*;
use crate::lineage::{EdgeType, ResolveStatus};

fn full_clip() -> Clip {
    Clip {
        id: "clip-1234abcd".to_owned(),
        title: "Electric Storm".to_owned(),
        tags: "ambient, cinematic".to_owned(),
        duration: 211.6,
        created_at: "2024-03-10T14:22:01Z".to_owned(),
        display_name: "alice".to_owned(),
        handle: "alice".to_owned(),
        prompt: "an orchestral storm".to_owned(),
        gpt_description_prompt: "a moody cinematic build".to_owned(),
        lyrics: "thunder rolls\nover the plains".to_owned(),
        model_name: "chirp-v4".to_owned(),
        major_model_version: "v4".to_owned(),
        image_large_url: "https://cdn1.suno.ai/signed?token=secret".to_owned(),
        audio_url: "https://cdn1.suno.ai/clip-1234abcd.mp3".to_owned(),
        ..Clip::default()
    }
}

fn full_lineage() -> LineageContext {
    LineageContext {
        root_id: "rootid567890".to_owned(),
        root_title: "Weather Series".to_owned(),
        root_date: String::new(),
        parent_id: "parentid1234".to_owned(),
        edge_type: Some(EdgeType::Extend),
        status: ResolveStatus::Resolved,
        track: 0,
        track_total: 0,
    }
}

#[test]
fn details_render_is_exact_and_fixed_order() {
    let rendered = render_clip_details(&full_clip(), &full_lineage());
    let expected = "Title: Electric Storm\n\
        Artist: alice\n\
        Album: Weather Series\n\
        Album Artist: alice\n\
        Date: 2024-03-10\n\
        Duration: 3:32\n\
        Model: chirp-v4 (v4)\n\
        Handle: alice\n\
        Style: ambient, cinematic\n\
        Style Summary: a moody cinematic build\n\
        Comment: a moody cinematic build\n\
        Prompt: an orchestral storm\n\
        Parent: parentid1234\n\
        Root: rootid567890\n\
        Lineage: Extended from parentid Root rootid56 (Weather Series)\n\
        Id: clip-1234abcd\n\
        Url: https://suno.com/song/clip-1234abcd\n";
    assert_eq!(rendered, expected);
}

#[test]
fn details_omit_empty_fields() {
    let clip = Clip {
        id: "only-id".to_owned(),
        title: "Bare".to_owned(),
        ..Clip::default()
    };
    let rendered = render_clip_details(&clip, &LineageContext::own_root(&clip));
    // Only the always-present fields survive: title, artist/album fallbacks,
    // the self-root id (SUNO_ROOT mirrors the embedded tag), and id/url. No
    // Date, Duration, Style, Prompt, Parent, or Lineage.
    let expected = "Title: Bare\n\
        Artist: Suno\n\
        Album: Bare\n\
        Album Artist: Suno\n\
        Root: only-id\n\
        Id: only-id\n\
        Url: https://suno.com/song/only-id\n";
    assert_eq!(rendered, expected);
    assert!(!rendered.contains("Duration:"));
    assert!(!rendered.contains("Prompt:"));
}

#[test]
fn details_exclude_signed_cdn_urls() {
    let rendered = render_clip_details(&full_clip(), &full_lineage());
    assert!(!rendered.contains("cdn1.suno.ai"));
    assert!(!rendered.contains("token=secret"));
    assert!(!rendered.contains(".mp3"));
}

#[test]
fn details_use_canonical_song_url() {
    let rendered = render_clip_details(&full_clip(), &full_lineage());
    assert!(rendered.contains("Url: https://suno.com/song/clip-1234abcd\n"));
}

#[test]
fn details_label_prompt_not_lyrics() {
    let rendered = render_clip_details(&full_clip(), &full_lineage());
    assert!(rendered.contains("Prompt: an orchestral storm\n"));
    // The details dump never labels the generation prompt as lyrics, and it
    // never carries the actual lyrics.
    assert!(!rendered.contains("Lyrics:"));
    assert!(!rendered.contains("thunder rolls"));
}

#[test]
fn details_use_resolved_lineage_not_feed_fields() {
    let clip = Clip {
        id: "child".to_owned(),
        title: "Child".to_owned(),
        ..Clip::default()
    };
    let lineage = LineageContext {
        root_id: "root-01".to_owned(),
        root_title: "Resolved Album".to_owned(),
        root_date: String::new(),
        parent_id: "root-01".to_owned(),
        edge_type: Some(EdgeType::Cover),
        status: ResolveStatus::Resolved,
        track: 0,
        track_total: 0,
    };
    let rendered = render_clip_details(&clip, &lineage);
    // The album is the resolved root title, never the clip's own title.
    assert!(rendered.contains("Album: Resolved Album\n"));
    assert!(!rendered.contains("Album: Child"));
}

#[test]
fn details_for_a_pure_root_omit_lineage_and_parent() {
    let clip = Clip {
        id: "root".to_owned(),
        title: "Root".to_owned(),
        ..Clip::default()
    };
    let rendered = render_clip_details(&clip, &LineageContext::own_root(&clip));
    // A pure root has no parent edge and no lineage summary; SUNO_ROOT still
    // mirrors the embedded tag (the clip's own id).
    assert!(!rendered.contains("Parent:"));
    assert!(!rendered.contains("Lineage:"));
    assert!(rendered.contains("Root: root\n"));
}

#[test]
fn lyrics_render_verbatim_with_one_trailing_newline() {
    let clip = Clip {
        lyrics: "line one\nline two".to_owned(),
        ..Clip::default()
    };
    assert_eq!(
        render_clip_lyrics(&clip),
        Some("line one\nline two\n".to_owned())
    );
}

#[test]
fn lyrics_normalise_trailing_whitespace_to_one_newline() {
    let clip = Clip {
        lyrics: "verse\n\n\n".to_owned(),
        ..Clip::default()
    };
    assert_eq!(render_clip_lyrics(&clip), Some("verse\n".to_owned()));
}

#[test]
fn lyrics_none_when_empty_or_whitespace_only() {
    assert_eq!(render_clip_lyrics(&Clip::default()), None);
    let clip = Clip {
        lyrics: "  \n\t \n".to_owned(),
        ..Clip::default()
    };
    assert_eq!(render_clip_lyrics(&clip), None);
}

#[test]
fn lyrics_use_clip_lyrics_not_prompt() {
    let clip = Clip {
        prompt: "the generation prompt".to_owned(),
        lyrics: "the actual sung words".to_owned(),
        ..Clip::default()
    };
    let rendered = render_clip_lyrics(&clip).unwrap();
    assert!(rendered.contains("the actual sung words"));
    assert!(!rendered.contains("the generation prompt"));
}

#[test]
fn lrc_none_when_lyrics_blank() {
    let empty = Clip::default();
    assert_eq!(
        render_clip_lrc(&empty, &LineageContext::own_root(&empty)),
        None
    );
    let clip = Clip {
        lyrics: "  \n\t \n".to_owned(),
        ..Clip::default()
    };
    assert_eq!(
        render_clip_lrc(&clip, &LineageContext::own_root(&clip)),
        None
    );
}

#[test]
fn lrc_renders_untimed_body_with_headers() {
    let rendered = render_clip_lrc(&full_clip(), &full_lineage()).unwrap();
    let expected = "[ti:Electric Storm]\n\
        [ar:alice]\n\
        [al:Weather Series]\n\
        [length:3:32]\n\
        [re:rs-suno]\n\
        thunder rolls\n\
        over the plains\n";
    assert_eq!(rendered, expected);
    // Untimed: no per-line `[mm:ss.xx]` timestamps.
    assert!(!rendered.contains("[00:"));
}

#[test]
fn lrc_omits_unknown_headers() {
    let clip = Clip {
        title: "Bare".to_owned(),
        lyrics: "one line".to_owned(),
        ..Clip::default()
    };
    let rendered = render_clip_lrc(&clip, &LineageContext::own_root(&clip)).unwrap();
    // No duration, so `[length:]` is omitted; artist falls back to Suno and
    // album to the title. The constant tool tag is always present.
    assert!(!rendered.contains("[length:"));
    assert!(rendered.contains("[ti:Bare]\n"));
    assert!(rendered.contains("[re:rs-suno]\n"));
    assert!(rendered.ends_with("one line\n"));
}

fn sample_aligned() -> crate::lyrics::AlignedLyrics {
    crate::lyrics::AlignedLyrics::from_json(&serde_json::json!({
        "aligned_words": [],
        "aligned_lyrics": [
            {"text": "thunder rolls", "start_s": 1.5, "end_s": 2.4, "section": "Verse 1",
             "words": [
                 {"text": "thunder", "start_s": 1.5, "end_s": 2.0},
                 {"text": "rolls", "start_s": 2.1, "end_s": 2.4}
             ]}
        ]
    }))
}

#[test]
fn synced_lrc_has_headers_then_line_stamps() {
    let rendered = render_synced_lrc(&full_clip(), &full_lineage(), &sample_aligned()).unwrap();
    let expected = "[ti:Electric Storm]\n\
        [ar:alice]\n\
        [al:Weather Series]\n\
        [length:3:32]\n\
        [re:rs-suno]\n\
        [00:01.50]thunder rolls\n";
    assert_eq!(rendered, expected);
}

#[test]
fn synced_lrc_is_none_for_empty_alignment() {
    // An instrumental (empty arrays) writes no synced `.lrc`, exactly as an
    // empty cover URL writes no cover.
    let empty = crate::lyrics::AlignedLyrics::default();
    assert_eq!(
        render_synced_lrc(&full_clip(), &full_lineage(), &empty),
        None
    );
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

use crate::lineage::{Resolution, RootInfo};
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
        bridges: Vec::new(),
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
        bridges: Vec::new(),
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
