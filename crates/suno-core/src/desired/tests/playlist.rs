use super::*;

#[test]
fn build_playlist_desired_orders_members_and_marks_absent() {
    let a = clip("id-a", "Song A", "alice");
    let b = clip("id-b", "Song B", "alice");
    let desired = desired_of(
        &[&a, &b],
        AudioFormat::Flac,
        SourceMode::Mirror,
        ArtifactToggles::default(),
    );
    let missing = clip("id-x", "Missing Song", "bob");
    let members = vec![b.clone(), missing.clone(), a.clone()];
    let inputs = vec![PlaylistInput {
        id: "pl1",
        name: "Road/Trip",
        members: &members,
    }];

    let out = build_playlist_desired(&inputs, &desired);
    assert_eq!(out.len(), 1);
    let pl = &out[0];
    assert_eq!(pl.id, "pl1");
    assert_eq!(pl.path, "Road Trip.m3u8");
    assert!(pl.content.starts_with("#EXTM3U\n#PLAYLIST:Road/Trip\n"));

    let pos_b = pl.content.find(path_of(&desired, "id-b")).unwrap();
    let pos_missing = pl.content.find("# (not in library) Missing Song").unwrap();
    let pos_a = pl.content.find(path_of(&desired, "id-a")).unwrap();
    assert!(pos_b < pos_missing && pos_missing < pos_a);
    assert!(!pl.content.contains("Missing Song\nbob/"));
    assert_eq!(pl.hash, content_hash(&pl.content));
}

#[test]
fn build_playlist_desired_builds_liked_and_multiple_in_order() {
    let a = clip("id-a", "Song A", "alice");
    let desired = desired_of(
        &[&a],
        AudioFormat::Flac,
        SourceMode::Mirror,
        ArtifactToggles::default(),
    );
    let members = vec![a.clone()];
    let inputs = vec![
        PlaylistInput {
            id: "pl1",
            name: "First",
            members: &members,
        },
        PlaylistInput {
            id: LIKED_PLAYLIST_ID,
            name: "Liked Songs",
            members: &members,
        },
    ];

    let out = build_playlist_desired(&inputs, &desired);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].id, "pl1");
    assert_eq!(out[1].id, LIKED_PLAYLIST_ID);
    assert_eq!(out[1].path, "Liked Songs.m3u8");
    assert!(out[0].content.contains(path_of(&desired, "id-a")));
    assert!(out[1].content.contains(path_of(&desired, "id-a")));
}

#[test]
fn build_playlist_desired_is_empty_for_no_inputs() {
    assert!(build_playlist_desired(&[], &[]).is_empty());
}
