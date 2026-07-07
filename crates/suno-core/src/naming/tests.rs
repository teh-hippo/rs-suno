//! The clip-naming test suite: exercises render_clip_name/_names, album
//! disambiguation, segment substitution, filesystem sanitisation, and the
//! stems-folder / stem-file path builders across Unicode and reserved names.

use super::*;
use crate::lineage::{EdgeType, ResolveStatus};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn test_clip(id: &str, title: &str) -> Clip {
    Clip {
        id: id.to_string(),
        title: title.to_string(),
        display_name: "München".to_string(),
        handle: "munchen".to_string(),
        ..Clip::default()
    }
}

fn render_own(clip: &Clip, config: &NamingConfig) -> RenderedName {
    let lineage = LineageContext::own_root(clip);
    render_clip_name(
        NamingRequest {
            clip,
            lineage: &lineage,
        },
        config,
    )
}

fn render_all_own(
    clips: &[Clip],
    config: &NamingConfig,
    colliding: &BTreeSet<String>,
) -> Vec<RenderedName> {
    let lineages: Vec<LineageContext> = clips.iter().map(LineageContext::own_root).collect();
    let requests: Vec<NamingRequest> = clips
        .iter()
        .zip(&lineages)
        .map(|(clip, lineage)| NamingRequest { clip, lineage })
        .collect();
    render_clip_names(&requests, config, colliding)
}

/// Render `clip` as its own root but numbered `track` of `total`.
fn render_with_track(clip: &Clip, track: u32, total: u32, config: &NamingConfig) -> RenderedName {
    let lineage = LineageContext {
        track,
        track_total: total,
        ..LineageContext::own_root(clip)
    };
    render_clip_name(
        NamingRequest {
            clip,
            lineage: &lineage,
        },
        config,
    )
}

#[test]
fn unicode_names_are_preserved_and_ascii_falls_back() {
    let clip = test_clip("abc12345", "Beyoncé/東京");

    let unicode = render_own(&clip, &NamingConfig::default());
    assert_eq!(
        unicode.relative_path,
        Path::new("München/Beyoncé 東京/München-Beyoncé 東京 [abc12345]")
    );

    let ascii = render_own(
        &clip,
        &NamingConfig {
            character_set: CharacterSet::Ascii,
            ..NamingConfig::default()
        },
    );
    assert_eq!(
        ascii.relative_path,
        Path::new("Munchen/Beyonce/Munchen-Beyonce [abc12345]")
    );
}

#[test]
fn reserved_and_hostile_names_are_sanitised() {
    let clip = Clip {
        id: "deadbeef".to_string(),
        title: "CON<>:\"/\\|?*.".to_string(),
        display_name: "AUX".to_string(),
        ..Clip::default()
    };

    let rendered = render_own(&clip, &NamingConfig::default());
    assert!(
        rendered.relative_path.starts_with("AUX_/CON_"),
        "path was {}",
        rendered.relative_path.display()
    );
    assert!(rendered.base_name.contains("[deadbeef]"));
}

#[test]
fn reserved_name_with_dotted_title_guards_the_stem_not_the_component() {
    // A bare `{title}` file component whose title is a device name with an
    // extension: the guard must land on the stem (`NUL_.mp3`), not the whole
    // component (`NUL.mp3_`), which would keep `NUL` as the dot-stem.
    let config = NamingConfig {
        template: "{title}".to_string(),
        ..NamingConfig::default()
    };
    let clip = test_clip("abcd1234-x", "NUL.mp3");
    let rendered = render_own(&clip, &config);
    let component = rendered.relative_path.to_string_lossy();
    assert_eq!(component, "NUL_.mp3");
    assert!(
        !is_reserved_name(&component),
        "component {component} is still a reserved device name"
    );
}

#[test]
fn default_template_always_embeds_id8() {
    let clip = test_clip("abcdef1234567890", "Any Title");
    let rendered = render_own(&clip, &NamingConfig::default());
    assert!(
        rendered.base_name.contains("[abcdef12]"),
        "base_name was {}",
        rendered.base_name
    );
}

#[test]
fn default_template_prefixes_the_two_digit_track_number() {
    let clip = test_clip("abc12345", "Any Title");
    let rendered = render_with_track(&clip, 7, 10, &NamingConfig::default());
    assert_eq!(
        rendered.relative_path,
        Path::new("München/Any Title/07 - München-Any Title [abc12345]")
    );
}

#[test]
fn unnumbered_track_leaves_no_orphan_prefix() {
    // track 0 (unnumbered): the "{track2} - " prefix must vanish cleanly, so the
    // name matches the pre-numbering layout with no leading separator.
    let clip = test_clip("abc12345", "Any Title");
    let rendered = render_with_track(&clip, 0, 0, &NamingConfig::default());
    assert_eq!(rendered.base_name, "München-Any Title [abc12345]");
    assert!(!rendered.base_name.starts_with(['-', ' ']));
}

#[test]
fn track_placeholders_render_raw_and_padded() {
    let clip = test_clip("abc12345", "Any Title");
    let config = NamingConfig {
        template: "{track}-{track2}-{title}".to_string(),
        ..NamingConfig::default()
    };
    let rendered = render_with_track(&clip, 7, 10, &config);
    assert_eq!(rendered.base_name, "7-07-Any Title");
}

#[test]
fn two_digit_pad_grows_past_ninety_nine() {
    let clip = test_clip("abc12345", "Any Title");
    let config = NamingConfig {
        template: "{track2}".to_string(),
        ..NamingConfig::default()
    };
    assert_eq!(render_with_track(&clip, 100, 100, &config).base_name, "100");
    assert_eq!(render_with_track(&clip, 5, 120, &config).base_name, "05");
}

#[test]
fn custom_template_replaces_all_known_placeholders_once() {
    let clip = Clip {
        id: "abcdef12-full".to_string(),
        title: "Song".to_string(),
        display_name: "Creator".to_string(),
        handle: "handle".to_string(),
        ..Clip::default()
    };
    let lineage = LineageContext {
        root_id: "rootxyz9-extra".to_string(),
        root_title: "Album".to_string(),
        root_date: String::new(),
        parent_id: "rootxyz9-extra".to_string(),
        edge_type: Some(EdgeType::Cover),
        status: ResolveStatus::Resolved,
        track: 0,
        track_total: 0,
    };
    let config = NamingConfig {
        template: "{creator}-{handle}-{album}-{title}-{root_id8}-{id8}-{id}-{unknown}".to_string(),
        ..NamingConfig::default()
    };

    let rendered = render_clip_name(
        NamingRequest {
            clip: &clip,
            lineage: &lineage,
        },
        &config,
    );

    assert_eq!(
        rendered.relative_path.to_string_lossy(),
        "Creator-handle-Album-Song-rootxyz9-abcdef12-abcdef12-full-{unknown}"
    );
}

#[test]
fn blank_titles_use_a_stable_suffix() {
    let clip = test_clip("12345678-clip", "   ");

    let rendered = render_own(&clip, &NamingConfig::default());
    assert_eq!(rendered.base_name, "München-Untitled [12345678]");
    assert_eq!(
        rendered.relative_path,
        Path::new("München/Untitled/München-Untitled [12345678]")
    );
}

#[test]
fn very_long_titles_are_trimmed() {
    let clip = test_clip("abcdef12", &"a".repeat(120));
    let rendered = render_own(
        &clip,
        &NamingConfig {
            max_component_len: 24,
            ..NamingConfig::default()
        },
    );

    for component in rendered.relative_path.components() {
        let text = component.as_os_str().to_string_lossy();
        assert!(
            text.chars().count() <= 24,
            "component {text:?} exceeds 24 chars"
        );
    }
    // The trailing [id8] must survive the truncation intact (#120).
    assert!(
        rendered.base_name.ends_with(" [abcdef12]"),
        "id8 disambiguator was sliced; base_name was {:?}",
        rendered.base_name
    );
}

#[test]
fn long_names_keep_the_full_id8_disambiguator() {
    // A creator+title long enough to overflow the cap keeps the whole
    // trailing [id8]: the title is shortened, not the id, so the name stays
    // complete and the bracket stays balanced (#120).
    let clip = test_clip("1234abcd-tail", &"a".repeat(120));
    let config = NamingConfig {
        max_component_len: 40,
        ..NamingConfig::default()
    };
    let rendered = render_own(&clip, &config);

    assert!(
        rendered.base_name.ends_with(" [1234abcd]"),
        "base_name must end with the full disambiguator, was {:?}",
        rendered.base_name
    );
    assert_eq!(rendered.base_name.chars().count(), 40);
}

#[test]
fn long_titled_siblings_stay_distinct_with_balanced_brackets() {
    // Two same-(long-)titled clips sharing a root must remain distinct: only
    // the title is shortened, so their [id8] suffixes differ and neither name
    // ends up with an unbalanced bracket (#120).
    let lineage = LineageContext {
        root_id: "root-42".to_string(),
        root_title: "Origin".to_string(),
        root_date: String::new(),
        parent_id: "root-42".to_string(),
        edge_type: Some(EdgeType::Cover),
        status: ResolveStatus::Resolved,
        track: 0,
        track_total: 0,
    };
    let title = "z".repeat(200);
    let first = test_clip("aaaa1111-x", &title);
    let second = test_clip("bbbb2222-y", &title);
    let requests = [
        NamingRequest {
            clip: &first,
            lineage: &lineage,
        },
        NamingRequest {
            clip: &second,
            lineage: &lineage,
        },
    ];

    let names = render_clip_names(&requests, &NamingConfig::default(), &BTreeSet::new());

    assert!(names[0].base_name.ends_with(" [aaaa1111]"));
    assert!(names[1].base_name.ends_with(" [bbbb2222]"));
    assert_ne!(names[0].relative_path, names[1].relative_path);
    for name in &names {
        assert!(name.base_name.chars().count() <= 80);
        assert_eq!(name.base_name.matches('[').count(), 1, "unbalanced '['");
        assert_eq!(name.base_name.matches(']').count(), 1, "unbalanced ']'");
    }
}

#[test]
fn long_colliding_album_keeps_its_root_id8() {
    // The album [root_id8] disambiguator is preserved when a long album title
    // must be truncated, mirroring the file-name fix (#120).
    let long = "Break Through ".repeat(20);
    let title = long.trim().to_string();
    let clip = Clip {
        id: "aaaa1111-x".to_string(),
        title: title.clone(),
        display_name: "München".to_string(),
        ..Clip::default()
    };
    let colliding: BTreeSet<String> = [title].into_iter().collect();
    let names = render_all_own(&[clip], &NamingConfig::default(), &colliding);

    let album = names[0]
        .relative_path
        .components()
        .nth(1)
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_default();
    assert!(album.ends_with(" [aaaa1111]"), "album was {album:?}");
    assert!(album.chars().count() <= 80);
}

#[test]
fn ascii_expanding_chars_do_not_slice_the_disambiguator() {
    // A literal expanding character (`ß` -> `ss` under ascii) in a custom
    // template, right before the trailing ` [{id8}]`, must not grow back over
    // the suffix and slice it: the base is sized after expansion (#120).
    let clip = test_clip("1234abcd", "Title");
    let config = NamingConfig {
        template: format!("{}{{title}} [{{id8}}]", "ß".repeat(80)),
        character_set: CharacterSet::Ascii,
        max_component_len: 40,
    };
    let rendered = render_own(&clip, &config);

    assert!(
        rendered.base_name.ends_with(" [1234abcd]"),
        "expansion sliced the id8; base_name was {:?}",
        rendered.base_name
    );
    assert!(rendered.base_name.chars().count() <= 40);
}

#[test]
fn same_title_siblings_stay_distinct_via_id8() {
    // Two clips sharing a root (same album folder) and the same title must
    // still land on distinct files; the default template's {id8} does that.
    let lineage = LineageContext {
        root_id: "root-9".to_string(),
        root_title: "Origin".to_string(),
        root_date: String::new(),
        parent_id: "root-9".to_string(),
        edge_type: Some(EdgeType::Cover),
        status: ResolveStatus::Resolved,
        track: 0,
        track_total: 0,
    };
    let first = test_clip("11111111-alpha", "Shared");
    let second = test_clip("22222222-beta", "Shared");
    let requests = [
        NamingRequest {
            clip: &first,
            lineage: &lineage,
        },
        NamingRequest {
            clip: &second,
            lineage: &lineage,
        },
    ];

    let names = render_clip_names(&requests, &NamingConfig::default(), &BTreeSet::new());

    assert_eq!(
        names[0].relative_path,
        Path::new("München/Origin/München-Shared [11111111]")
    );
    assert_eq!(
        names[1].relative_path,
        Path::new("München/Origin/München-Shared [22222222]")
    );
}

#[test]
fn id8_prefix_collision_falls_back_to_full_id() {
    // Custom template without {id8} so identical titles collide and the
    // filename fallback (full id) has to keep them distinct.
    let config = NamingConfig {
        template: "{creator}/{title}".to_string(),
        ..NamingConfig::default()
    };
    let first = test_clip("abcd1234-first", "Untitled");
    let second = test_clip("abcd1234-second", "Untitled");

    let names = render_all_own(&[first.clone(), second.clone()], &config, &BTreeSet::new());
    let swapped = render_all_own(&[second.clone(), first.clone()], &config, &BTreeSet::new());

    assert_ne!(
        names[0].relative_path.to_string_lossy(),
        names[1].relative_path.to_string_lossy()
    );

    let ordered = |rendered: &[RenderedName], clips: &[Clip]| {
        clips
            .iter()
            .zip(rendered)
            .map(|(clip, name)| {
                (
                    clip.id.clone(),
                    name.relative_path.to_string_lossy().into_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(
        ordered(&names, &[first.clone(), second.clone()]),
        ordered(&swapped, &[second, first])
    );
}

#[test]
fn album_is_root_title_for_a_remix() {
    let clip = Clip {
        id: "child".to_string(),
        title: "Remix".to_string(),
        display_name: "München".to_string(),
        ..Clip::default()
    };
    let lineage = LineageContext {
        root_id: "root-1".to_string(),
        root_title: "Original".to_string(),
        root_date: String::new(),
        parent_id: "root-1".to_string(),
        edge_type: Some(EdgeType::Cover),
        status: ResolveStatus::Resolved,
        track: 0,
        track_total: 0,
    };

    let rendered = render_clip_name(
        NamingRequest {
            clip: &clip,
            lineage: &lineage,
        },
        &NamingConfig::default(),
    );
    assert_eq!(
        rendered.relative_path,
        Path::new("München/Original/München-Remix [child]")
    );
}

#[test]
fn album_is_own_title_for_a_root() {
    let clip = Clip {
        id: "root-1".to_string(),
        title: "Original".to_string(),
        display_name: "München".to_string(),
        ..Clip::default()
    };

    let rendered = render_own(&clip, &NamingConfig::default());
    assert_eq!(
        rendered.relative_path,
        Path::new("München/Original/München-Original [root-1]")
    );
}

#[test]
fn shared_album_title_from_distinct_roots_is_disambiguated() {
    let first = Clip {
        id: "aaaa1111-x".to_string(),
        title: "Break Through".to_string(),
        display_name: "München".to_string(),
        ..Clip::default()
    };
    let second = Clip {
        id: "bbbb2222-y".to_string(),
        title: "Break Through".to_string(),
        display_name: "München".to_string(),
        ..Clip::default()
    };

    // The colliding set is authoritative (store-driven), so disambiguation
    // does not depend on both roots appearing in the same batch.
    let colliding: BTreeSet<String> = ["Break Through".to_string()].into_iter().collect();
    let names = render_all_own(
        &[first.clone(), second.clone()],
        &NamingConfig::default(),
        &colliding,
    );
    let swapped = render_all_own(
        &[second.clone(), first.clone()],
        &NamingConfig::default(),
        &colliding,
    );

    let album_of = |rendered: &RenderedName| {
        rendered
            .relative_path
            .components()
            .nth(1)
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default()
    };

    assert_eq!(album_of(&names[0]), "Break Through [aaaa1111]");
    assert_eq!(album_of(&names[1]), "Break Through [bbbb2222]");
    // Deterministic regardless of input order.
    assert_eq!(album_of(&swapped[0]), "Break Through [bbbb2222]");
    assert_eq!(album_of(&swapped[1]), "Break Through [aaaa1111]");

    // The MEDIUM fix: a narrowed run showing only one of the two roots
    // still gets the suffixed folder, so folders never oscillate.
    let alone = render_all_own(
        std::slice::from_ref(&first),
        &NamingConfig::default(),
        &colliding,
    );
    assert_eq!(album_of(&alone[0]), "Break Through [aaaa1111]");
}

#[test]
fn unique_root_title_stays_a_bare_album() {
    // A title absent from the colliding set keeps its bare folder even when
    // the batch happens to hold a same-titled sibling of the same root.
    let clip = Clip {
        id: "solo-1".to_string(),
        title: "Solo".to_string(),
        display_name: "München".to_string(),
        ..Clip::default()
    };
    let names = render_all_own(&[clip], &NamingConfig::default(), &BTreeSet::new());
    assert_eq!(
        names[0].relative_path,
        Path::new("München/Solo/München-Solo [solo-1]")
    );
}

#[test]
fn sanitise_name_strips_separators_and_falls_back_when_empty() {
    assert_eq!(sanitise_name("Road/Trip: 2024"), "Road Trip 2024");
    assert_eq!(sanitise_name(""), "playlist");
    // A name made only of illegal characters strips to nothing, so the
    // caller still gets a usable, non-empty stem.
    assert_eq!(sanitise_name("///"), "playlist");
}

#[test]
fn stems_folder_is_a_sibling_suffix_of_the_song_base() {
    assert_eq!(
        stems_folder("Creator/Album/Creator-Song [abcd1234]"),
        "Creator/Album/Creator-Song [abcd1234].stems"
    );
}

#[test]
fn stem_file_path_combines_song_stem_label_and_disambiguator() {
    let path = stem_file_path(
        "Creator/Album/Creator-Song [abcd1234]",
        "Vocals",
        "stem-vocals-9f8e7d6c",
        "mp3",
        CharacterSet::Unicode,
    );
    assert_eq!(
        path,
        "Creator/Album/Creator-Song [abcd1234].stems/Creator-Song [abcd1234] - Vocals [stem-voc].mp3"
    );
}

#[test]
fn stem_file_path_disambiguates_blank_and_duplicate_labels_by_id() {
    // Two stems with the SAME (blank) label must not collide: the stem-id
    // disambiguator keeps them distinct even with no usable label.
    let a = stem_file_path("song", "", "id-aaaaaaaa", "wav", CharacterSet::Unicode);
    let b = stem_file_path("song", "", "id-bbbbbbbb", "wav", CharacterSet::Unicode);
    assert_eq!(a, "song.stems/song [id-aaaaa].wav");
    assert_eq!(b, "song.stems/song [id-bbbbb].wav");
    assert_ne!(a, b);
}

#[test]
fn stem_file_path_sanitises_label_and_extension_and_honours_ascii() {
    // Illegal path characters in the label are stripped, the extension is
    // reduced to a safe lowercase token, and ASCII folding applies.
    let path = stem_file_path(
        "song",
        "Lead/Vocal: Æ",
        "STEMID12",
        ".FLAC",
        CharacterSet::Ascii,
    );
    assert_eq!(path, "song.stems/song - Lead Vocal AE [STEMID12].flac");
    // A junk extension falls back to mp3 (defensive; callers pass wav/mp3).
    let fallback = stem_file_path("s", "Bass", "x", "??", CharacterSet::Unicode);
    assert_eq!(fallback, "s.stems/s - Bass [x].mp3");
}

#[test]
fn case_only_path_difference_is_a_canonical_collision() {
    // A custom template without {id8}: clips whose titles differ only in
    // case produce different exact paths but the same canonical path and
    // must be disambiguated to avoid clobbering on case-insensitive FSes.
    let config = NamingConfig {
        template: "{creator}/{title}".to_string(),
        ..NamingConfig::default()
    };
    let first = test_clip("aaaa1111-x", "sunrise");
    let second = test_clip("bbbb2222-y", "SUNRISE");

    let names = render_all_own(&[first, second], &config, &BTreeSet::new());

    assert_ne!(
        names[0].relative_path.to_string_lossy(),
        names[1].relative_path.to_string_lossy(),
        "canonical collision was not disambiguated"
    );
}

#[test]
fn nfc_nfd_path_difference_is_a_canonical_collision() {
    // The same character encoded as NFC vs NFD produces different byte
    // strings but the same file on NFC-normalising filesystems (macOS APFS).
    let config = NamingConfig {
        template: "{creator}/{title}".to_string(),
        ..NamingConfig::default()
    };
    // "é" as NFC (U+00E9) vs NFD (e + U+0301).
    let nfc_title = "\u{00e9}toile";
    let nfd_title = "e\u{0301}toile";
    let first = test_clip("aaaa1111-x", nfc_title);
    let second = test_clip("bbbb2222-y", nfd_title);

    let names = render_all_own(&[first, second], &config, &BTreeSet::new());

    assert_ne!(
        names[0].relative_path.to_string_lossy(),
        names[1].relative_path.to_string_lossy(),
        "NFC/NFD canonical collision was not disambiguated"
    );
}

#[test]
fn genuinely_distinct_paths_are_never_wrongly_disambiguated() {
    // Clips with distinct titles (not even canonically equivalent) must not
    // receive unnecessary suffixes — the canonical check must not produce
    // false positives.
    let config = NamingConfig {
        template: "{creator}/{title}".to_string(),
        ..NamingConfig::default()
    };
    let first = test_clip("aaaa1111-x", "Alpha");
    let second = test_clip("bbbb2222-y", "Beta");

    let names = render_all_own(&[first, second], &config, &BTreeSet::new());

    assert_eq!(
        names[0].relative_path,
        Path::new("München/Alpha"),
        "distinct path was wrongly suffixed"
    );
    assert_eq!(
        names[1].relative_path,
        Path::new("München/Beta"),
        "distinct path was wrongly suffixed"
    );
}
