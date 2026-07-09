use super::*;

fn album_clip(id: &str, play_count: u64, created_at: &str, image: &str, video: &str) -> Clip {
    Clip {
        id: id.to_string(),
        title: "Song".to_string(),
        image_large_url: image.to_string(),
        video_cover_url: video.to_string(),
        play_count,
        created_at: created_at.to_string(),
        ..Default::default()
    }
}

fn album_member(clip: Clip, root_id: &str, path: &str) -> Desired {
    let mut lineage = LineageContext::own_root(&clip);
    lineage.root_id = root_id.to_string();
    Desired {
        clip,
        lineage,
        path: path.to_string(),
        format: AudioFormat::Flac,
        meta_hash: "m".to_string(),
        art_hash: "a".to_string(),
        embedded_lyrics_hash: String::new(),
        modes: vec![SourceMode::Mirror],
        trashed: false,
        private: false,
        artifacts: Vec::new(),
        stems: None,
    }
}

#[test]
fn folder_jpg_source_is_most_played() {
    let members = vec![
        album_member(album_clip("a", 5, "t0", "art-a", ""), "root", "c/al/a.flac"),
        album_member(album_clip("b", 9, "t1", "art-b", ""), "root", "c/al/b.flac"),
        album_member(album_clip("c", 2, "t2", "art-c", ""), "root", "c/al/c.flac"),
    ];
    let albums = album_desired(&members, false, false, WebpEncodeSettings::default());
    assert_eq!(albums.len(), 1);
    let jpg = albums[0].folder_jpg.as_ref().unwrap();
    // "b" has the highest play_count, so its art content hash wins.
    assert_eq!(jpg.hash, art_url_hash("art-b"));
    assert_eq!(jpg.source_url, "art-b");
    assert_eq!(jpg.path, "c/al/folder.jpg");
    assert_eq!(jpg.kind, ArtifactKind::FolderJpg);
}

#[test]
fn folder_jpg_tie_breaks_earliest_then_lex_id() {
    // Equal play_count: earliest created_at wins.
    let by_time = vec![
        album_member(album_clip("z", 4, "t2", "art-z", ""), "root", "c/al/z.flac"),
        album_member(album_clip("y", 4, "t0", "art-y", ""), "root", "c/al/y.flac"),
        album_member(album_clip("x", 4, "t1", "art-x", ""), "root", "c/al/x.flac"),
    ];
    let jpg = album_desired(&by_time, false, false, WebpEncodeSettings::default())[0]
        .folder_jpg
        .clone()
        .unwrap();
    assert_eq!(jpg.source_url, "art-y");

    // Equal play_count and created_at: lexicographically smallest id wins.
    let by_id = vec![
        album_member(album_clip("m", 4, "t0", "art-m", ""), "root", "c/al/m.flac"),
        album_member(album_clip("g", 4, "t0", "art-g", ""), "root", "c/al/g.flac"),
    ];
    let jpg = album_desired(&by_id, false, false, WebpEncodeSettings::default())[0]
        .folder_jpg
        .clone()
        .unwrap();
    assert_eq!(jpg.source_url, "art-g");
}

#[test]
fn folder_webp_source_is_first_created_animated() {
    let members = vec![
        album_member(
            album_clip("a", 9, "t2", "art-a", "vid-a"),
            "root",
            "c/al/a.flac",
        ),
        album_member(
            album_clip("b", 1, "t0", "art-b", "vid-b"),
            "root",
            "c/al/b.flac",
        ),
        album_member(album_clip("c", 5, "t1", "art-c", ""), "root", "c/al/c.flac"),
    ];
    let webp = album_desired(&members, true, false, WebpEncodeSettings::default())[0]
        .folder_webp
        .clone()
        .unwrap();
    // "b" is earliest-created with an animated source, regardless of plays.
    assert_eq!(webp.source_url, "vid-b");
    assert_eq!(
        webp.hash,
        webp_art_hash("vid-b", &WebpEncodeSettings::default())
    );
    assert_eq!(webp.path, "c/al/cover.webp");
    assert_eq!(webp.kind, ArtifactKind::FolderWebp);

    // The cover.webp hash folds in the encode settings, so raising quality
    // (or any encode knob) re-transcodes an existing album cover.
    let hi = WebpEncodeSettings {
        quality: 40,
        ..WebpEncodeSettings::default()
    };
    let rehashed = album_desired(&members, true, false, hi)[0]
        .folder_webp
        .clone()
        .unwrap();
    assert_ne!(rehashed.hash, webp.hash);
}

#[test]
fn animated_covers_off_yields_no_folder_webp() {
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", "vid-a"),
        "root",
        "c/al/a.flac",
    )];
    let off = album_desired(&members, false, false, WebpEncodeSettings::default());
    assert!(off[0].folder_webp.is_none());
    let on = album_desired(&members, true, false, WebpEncodeSettings::default());
    assert!(on[0].folder_webp.is_some());
}

#[test]
fn raw_cover_yields_folder_mp4_from_the_webp_source_verbatim() {
    let members = vec![
        album_member(
            album_clip("a", 9, "t2", "art-a", "vid-a"),
            "root",
            "c/al/a.flac",
        ),
        album_member(
            album_clip("b", 1, "t0", "art-b", "vid-b"),
            "root",
            "c/al/b.flac",
        ),
    ];
    // `both`: cover.webp (transcoded) and cover.mp4 (raw) come from the SAME
    // earliest-created animated variant, so they describe one animation. The
    // raw cover keeps the `video_cover_url` unchanged and hashes on the URL.
    let album = album_desired(&members, true, true, WebpEncodeSettings::default()).remove(0);
    let webp = album.folder_webp.unwrap();
    let mp4 = album.folder_mp4.unwrap();
    assert_eq!(mp4.kind, ArtifactKind::FolderMp4);
    assert_eq!(mp4.path, "c/al/cover.mp4");
    assert_eq!(mp4.source_url, "vid-b");
    assert_eq!(mp4.hash, art_url_hash("vid-b"));
    assert_eq!(mp4.source_url, webp.source_url, "same variant feeds both");
}

#[test]
fn raw_cover_and_webp_are_independent_toggles() {
    let members = vec![album_member(
        album_clip("a", 1, "t0", "art-a", "vid-a"),
        "root",
        "c/al/a.flac",
    )];
    // webp-only keeps the transcode but no raw mp4.
    let webp_only = album_desired(&members, true, false, WebpEncodeSettings::default()).remove(0);
    assert!(webp_only.folder_webp.is_some());
    assert!(webp_only.folder_mp4.is_none());
    // mp4-only keeps the raw source but no transcode.
    let mp4_only = album_desired(&members, false, true, WebpEncodeSettings::default()).remove(0);
    assert!(mp4_only.folder_webp.is_none());
    assert!(mp4_only.folder_mp4.is_some());
}

#[test]
fn raw_cover_needs_an_animated_source() {
    // No variant carries a video_cover_url, so there is nothing to keep.
    let members = vec![album_member(
        album_clip("a", 3, "t0", "art-a", ""),
        "root",
        "c/al/a.flac",
    )];
    let album = album_desired(&members, true, true, WebpEncodeSettings::default()).remove(0);
    assert!(album.folder_mp4.is_none());
    assert!(album.folder_webp.is_none());
}

#[test]
fn album_with_no_art_yields_no_folder_jpg() {
    let members = vec![album_member(
        album_clip("a", 3, "t0", "", ""),
        "root",
        "c/al/a.flac",
    )];
    let albums = album_desired(&members, true, false, WebpEncodeSettings::default());
    assert!(albums[0].folder_jpg.is_none());
    assert!(albums[0].folder_webp.is_none());
}

#[test]
fn album_desired_groups_by_root_id() {
    let members = vec![
        album_member(album_clip("a", 1, "t0", "art-a", ""), "r1", "c/al1/a.flac"),
        album_member(album_clip("b", 1, "t0", "art-b", ""), "r2", "c/al2/b.flac"),
        album_member(album_clip("c", 9, "t0", "art-c", ""), "r1", "c/al1/c.flac"),
    ];
    let albums = album_desired(&members, false, false, WebpEncodeSettings::default());
    assert_eq!(albums.len(), 2);
    assert_eq!(albums[0].root_id, "r1");
    assert_eq!(albums[0].folder_jpg.as_ref().unwrap().source_url, "art-c");
    assert_eq!(
        albums[0].folder_jpg.as_ref().unwrap().path,
        "c/al1/folder.jpg"
    );
    assert_eq!(albums[1].root_id, "r2");
    assert_eq!(albums[1].folder_jpg.as_ref().unwrap().source_url, "art-b");
    assert_eq!(
        albums[1].folder_jpg.as_ref().unwrap().path,
        "c/al2/folder.jpg"
    );
}
