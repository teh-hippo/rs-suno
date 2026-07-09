use super::*;
use crate::model::HistoryEntry;

fn history(id: &str) -> HistoryEntry {
    HistoryEntry {
        id: id.to_owned(),
        ..Default::default()
    }
}

/// An external (out-of-library) cover root with no title or date — the
/// fallback shape the `album`/`year` own-value cases share.
fn external_cover() -> LineageContext {
    LineageContext {
        root_id: "outside".into(),
        root_title: String::new(),
        root_date: String::new(),
        parent_id: "outside".into(),
        edge_type: Some(EdgeType::Cover),
        status: ResolveStatus::External,
        track: 0,
        track_total: 0,
    }
}

// A clean six-clip chain modelled on the real `chain1` grounding data:
// upsample -> cover -> upsample -> cover -> edit -> root. For every hop the
// op pointer and `edited_clip_id` agree, as they do in the live shape.
fn chain1_clips() -> Vec<Clip> {
    vec![
        Clip {
            id: "40068b49".into(),
            title: "Zac and the Sea Eagles (Lullaby Version)".into(),
            clip_type: "upsample".into(),
            task: "upsample".into(),
            is_remix: true,
            upsample_clip_id: "52962dae".into(),
            edited_clip_id: "52962dae".into(),
            ..Default::default()
        },
        Clip {
            id: "52962dae".into(),
            title: "Zac and the Sea Eagles (Edit) (Remastered)".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            is_remix: true,
            cover_clip_id: "536e1b92".into(),
            edited_clip_id: "536e1b92".into(),
            ..Default::default()
        },
        Clip {
            id: "536e1b92".into(),
            title: "Zac and the Sea Eagles (Edit) (Remastered)".into(),
            clip_type: "upsample".into(),
            task: "upsample".into(),
            is_remix: true,
            upsample_clip_id: "b9f27ee1".into(),
            edited_clip_id: "b9f27ee1".into(),
            ..Default::default()
        },
        Clip {
            id: "b9f27ee1".into(),
            title: "Zac and the Sea Eagles (Edit)".into(),
            clip_type: "gen".into(),
            task: "cover".into(),
            is_remix: true,
            cover_clip_id: "c1997d52".into(),
            edited_clip_id: "c1997d52".into(),
            ..Default::default()
        },
        Clip {
            id: "c1997d52".into(),
            title: "Zac and the Sea Eagles (Rework)".into(),
            clip_type: "edit_v3_export".into(),
            edited_clip_id: "dfb59a04".into(),
            ..Default::default()
        },
        Clip {
            id: "dfb59a04".into(),
            title: "Zac and the Sea Eagles".into(),
            clip_type: "gen".into(),
            ..Default::default()
        },
    ]
}

#[test]
fn edge_type_labels_read_naturally() {
    assert_eq!(EdgeType::Cover.label(), "Cover of");
    assert_eq!(EdgeType::Remaster.label(), "Remaster of");
    assert_eq!(EdgeType::SpeedEdit.label(), "Speed-edited from");
    assert_eq!(EdgeType::Edit.label(), "Edited from");
    assert_eq!(EdgeType::Extend.label(), "Extended from");
    assert_eq!(EdgeType::SectionReplace.label(), "Section replaced from");
    assert_eq!(EdgeType::Stitch.label(), "Stitched from");
    assert_eq!(EdgeType::Derived.label(), "Derived from");
    assert_eq!(EdgeType::Uploaded.label(), "Uploaded");
}

#[test]
fn classifies_remaster_cover_edit_and_root_across_chain1() {
    let clips = chain1_clips();

    assert_eq!(edge_type(&clips[0]), Some(EdgeType::Remaster));
    assert_eq!(
        immediate_parent(&clips[0]),
        Some(("52962dae".into(), EdgeType::Remaster))
    );

    assert_eq!(edge_type(&clips[1]), Some(EdgeType::Cover));
    assert_eq!(
        immediate_parent(&clips[1]),
        Some(("536e1b92".into(), EdgeType::Cover))
    );

    assert_eq!(edge_type(&clips[4]), Some(EdgeType::Edit));
    assert_eq!(
        immediate_parent(&clips[4]),
        Some(("dfb59a04".into(), EdgeType::Edit))
    );

    assert_eq!(edge_type(&clips[5]), None);
    assert_eq!(immediate_parent(&clips[5]), None);
}

#[test]
fn edge_classification_covers_every_shape() {
    // Each clip shape resolves to exactly one edge_type and one immediate_parent.
    // The rows span the real op shapes (speed-edit, extend, infill/section
    // replace, stitch, cover-of-a-stitch, derived) and the several root shapes
    // (empty-task gen, inherited-concat-without-type, upload, unmarked).
    struct Row {
        label: &'static str,
        clip: Clip,
        edge: Option<EdgeType>,
        parent: Option<(String, EdgeType)>,
    }
    let rows = vec![
        Row {
            label: "speed edit from speed pointer, no edited_clip_id",
            clip: Clip {
                id: "6e5193b1".into(),
                title: "Go Xavi Go, Fast. (Drum n' Bass Version)".into(),
                clip_type: "edit_speed".into(),
                is_remix: true,
                speed_clip_id: "2b69882c".into(),
                ..Default::default()
            },
            edge: Some(EdgeType::SpeedEdit),
            parent: Some(("2b69882c".into(), EdgeType::SpeedEdit)),
        },
        Row {
            label: "empty-task gen is a root",
            clip: Clip {
                id: "b4f16694".into(),
                title: "Go Xavi Go, Fast.".into(),
                clip_type: "gen".into(),
                task: String::new(),
                ..Default::default()
            },
            edge: None,
            parent: None,
        },
        Row {
            label: "extend from history head",
            clip: Clip {
                id: "9a3dcb67".into(),
                title: "Extended".into(),
                clip_type: "gen".into(),
                task: "extend".into(),
                edited_clip_id: "0a3c311a".into(),
                history: vec![HistoryEntry {
                    id: "0a3c311a".into(),
                    continue_at: Some(115.35),
                    ..Default::default()
                }],
                ..Default::default()
            },
            edge: Some(EdgeType::Extend),
            parent: Some(("0a3c311a".into(), EdgeType::Extend)),
        },
        Row {
            label: "infill: override_history wins over history/edited",
            clip: Clip {
                id: "c0ce5c48".into(),
                title: "Section replaced".into(),
                clip_type: "gen".into(),
                task: "infill".into(),
                edited_clip_id: "cf37e05f".into(),
                override_history_clip_id: "d3d28e59".into(),
                override_future_clip_id: "ea88571e".into(),
                history: vec![HistoryEntry {
                    id: "cf37e05f".into(),
                    infill: true,
                    infill_start_s: Some(20.4),
                    infill_end_s: Some(24.92),
                    ..Default::default()
                }],
                ..Default::default()
            },
            edge: Some(EdgeType::SectionReplace),
            parent: Some(("d3d28e59".into(), EdgeType::SectionReplace)),
        },
        Row {
            label: "fixed_infill is also a section replace",
            clip: Clip {
                task: "fixed_infill".into(),
                override_history_clip_id: "past".into(),
                edited_clip_id: "edited".into(),
                ..Default::default()
            },
            edge: Some(EdgeType::SectionReplace),
            parent: Some(("past".into(), EdgeType::SectionReplace)),
        },
        Row {
            label: "stitch from concat base segment",
            clip: Clip {
                id: "43ba1ce3".into(),
                title: "Stitched".into(),
                clip_type: "concat".into(),
                concat_history: vec![
                    HistoryEntry {
                        id: "ead64fbe".into(),
                        continue_at: Some(149.19),
                        ..Default::default()
                    },
                    history("da47b824"),
                ],
                ..Default::default()
            },
            edge: Some(EdgeType::Stitch),
            parent: Some(("ead64fbe".into(), EdgeType::Stitch)),
        },
        Row {
            label: "inherited concat_history without type=concat is a root",
            clip: Clip {
                clip_type: "gen".into(),
                concat_history: vec![history("base"), history("second")],
                ..Default::default()
            },
            edge: None,
            parent: None,
        },
        Row {
            label: "cover of a stitch classifies as cover, not stitch",
            clip: Clip {
                id: "cov".into(),
                title: "Cover of a stitch".into(),
                clip_type: "gen".into(),
                task: "cover".into(),
                cover_clip_id: "stitch-parent".into(),
                edited_clip_id: "stitch-parent".into(),
                concat_history: vec![history("inherited-base"), history("inherited-seg")],
                ..Default::default()
            },
            edge: Some(EdgeType::Cover),
            parent: Some(("stitch-parent".into(), EdgeType::Cover)),
        },
        Row {
            label: "upload is a root",
            clip: Clip {
                id: "4770ef56".into(),
                title: "Uploaded audio".into(),
                clip_type: "upload".into(),
                ..Default::default()
            },
            edge: None,
            parent: None,
        },
        Row {
            label: "edited-only clip with an unknown task is derived",
            clip: Clip {
                clip_type: "gen".into(),
                task: "chop_sample_condition".into(),
                edited_clip_id: "parent-x".into(),
                ..Default::default()
            },
            edge: Some(EdgeType::Derived),
            parent: Some(("parent-x".into(), EdgeType::Derived)),
        },
        Row {
            label: "unmarked clip without a parent pointer is a root",
            clip: Clip {
                clip_type: "gen".into(),
                task: "chop_sample_condition".into(),
                ..Default::default()
            },
            edge: None,
            parent: None,
        },
    ];
    for Row {
        label,
        clip,
        edge,
        parent,
    } in rows
    {
        assert_eq!(edge_type(&clip), edge, "edge_type [{label}]");
        assert_eq!(
            immediate_parent(&clip),
            parent,
            "immediate_parent [{label}]"
        );
    }
}

#[test]
fn is_remix_does_not_change_classification() {
    let base = Clip {
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "root-1".into(),
        edited_clip_id: "root-1".into(),
        ..Default::default()
    };
    let mut with_flag = base.clone();
    with_flag.is_remix = true;
    let mut without_flag = base;
    without_flag.is_remix = false;

    assert_eq!(edge_type(&with_flag), edge_type(&without_flag));
    assert_eq!(
        immediate_parent(&with_flag),
        immediate_parent(&without_flag)
    );
    assert_eq!(edge_type(&with_flag), Some(EdgeType::Cover));
    assert_eq!(
        immediate_parent(&with_flag),
        Some(("root-1".into(), EdgeType::Cover))
    );
}

#[test]
fn zero_uuid_cover_falls_back_to_edited() {
    let clip = Clip {
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: ZERO_UUID.into(),
        edited_clip_id: "real-parent".into(),
        ..Default::default()
    };
    assert_eq!(
        immediate_parent(&clip),
        Some(("real-parent".into(), EdgeType::Cover))
    );
}

#[test]
fn m_prefix_is_stripped_from_history_and_concat_ids() {
    let extend = Clip {
        clip_type: "gen".into(),
        task: "extend".into(),
        history: vec![history("m_abc123")],
        ..Default::default()
    };
    assert_eq!(
        immediate_parent(&extend),
        Some(("abc123".into(), EdgeType::Extend))
    );

    let stitch = Clip {
        clip_type: "concat".into(),
        concat_history: vec![history("m_base"), history("m_second")],
        ..Default::default()
    };
    let edges = lineage_edges(&stitch);
    assert_eq!(edges[0].parent_id, "base");
    assert_eq!(edges[1].parent_id, "second");
    assert_eq!(edges[1].role, EdgeRole::Secondary);
}

#[test]
fn lineage_edges_enumerates_stitch_segments() {
    // lineage_edges lists a stitch's segments in order: the base segment is the
    // primary and the rest are ordinal secondaries. A root has none, and an
    // empty base id still yields the real secondaries (never dropped).
    struct Row {
        label: &'static str,
        clip: Clip,
        edges: Vec<Edge>,
    }
    let rows = vec![
        Row {
            label: "a root has no lineage edges",
            clip: Clip {
                clip_type: "gen".into(),
                ..Default::default()
            },
            edges: vec![],
        },
        Row {
            label: "stitch records base primary then ordered secondaries",
            clip: Clip {
                clip_type: "concat".into(),
                concat_history: vec![history("base"), history("seg1"), history("seg2")],
                ..Default::default()
            },
            edges: vec![
                Edge {
                    parent_id: "base".into(),
                    edge_type: EdgeType::Stitch,
                    role: EdgeRole::Primary,
                    ordinal: 0,
                    source_field: "concat_history",
                },
                Edge {
                    parent_id: "seg1".into(),
                    edge_type: EdgeType::Stitch,
                    role: EdgeRole::Secondary,
                    ordinal: 1,
                    source_field: "concat_history",
                },
                Edge {
                    parent_id: "seg2".into(),
                    edge_type: EdgeType::Stitch,
                    role: EdgeRole::Secondary,
                    ordinal: 2,
                    source_field: "concat_history",
                },
            ],
        },
        Row {
            label: "secondaries survive an empty primary base segment",
            clip: Clip {
                clip_type: "concat".into(),
                concat_history: vec![history(""), history("seg1"), history("seg2")],
                ..Default::default()
            },
            edges: vec![
                Edge {
                    parent_id: "seg1".into(),
                    edge_type: EdgeType::Stitch,
                    role: EdgeRole::Secondary,
                    ordinal: 1,
                    source_field: "concat_history",
                },
                Edge {
                    parent_id: "seg2".into(),
                    edge_type: EdgeType::Stitch,
                    role: EdgeRole::Secondary,
                    ordinal: 2,
                    source_field: "concat_history",
                },
            ],
        },
    ];
    for Row { label, clip, edges } in rows {
        assert_eq!(lineage_edges(&clip), edges, "{label}");
    }
}

#[test]
fn lineage_edges_records_infill_future_as_secondary() {
    let clip = Clip {
        task: "infill".into(),
        override_history_clip_id: "past".into(),
        override_future_clip_id: "future".into(),
        ..Default::default()
    };
    let edges = lineage_edges(&clip);
    assert_eq!(edges[0].parent_id, "past");
    assert_eq!(edges[0].role, EdgeRole::Primary);
    assert_eq!(edges[0].source_field, "override_history_clip_id");
    assert_eq!(
        edges[1],
        Edge {
            parent_id: "future".into(),
            edge_type: EdgeType::SectionReplace,
            role: EdgeRole::Secondary,
            ordinal: 1,
            source_field: "override_future_clip_id",
        }
    );
}

fn clip_root(id: &str, handle: &str) -> crate::model::ClipRoot {
    crate::model::ClipRoot {
        id: id.to_owned(),
        handle: handle.to_owned(),
        ..Default::default()
    }
}

#[test]
fn attribution_edges_map_clip_roots_in_order() {
    let clip = Clip {
        id: "child".into(),
        handle: "me".into(),
        clip_attribution_type: "remix".into(),
        clip_roots: vec![
            clip_root("own-root", "me"),
            clip_root("foreign-root", "stranger"),
        ],
        ..Default::default()
    };
    let edges = attribution_edges(&clip);
    assert_eq!(edges.len(), 2);
    assert_eq!(
        edges[0],
        AttributionEdge {
            parent_id: "own-root".into(),
            edge_slug: "remix".into(),
            role: EdgeRole::Secondary,
            ordinal: 0,
            source_field: "clip_roots",
            same_owner: true,
        }
    );
    assert_eq!(edges[1].parent_id, "foreign-root");
    assert_eq!(edges[1].ordinal, 1);
    assert!(
        !edges[1].same_owner,
        "a differently-handled root is foreign, and still emits an edge"
    );
}

#[test]
fn attribution_edges_are_empty_without_clip_roots() {
    let clip = Clip {
        id: "child".into(),
        handle: "me".into(),
        ..Default::default()
    };
    assert!(attribution_edges(&clip).is_empty());
}

#[test]
fn attribution_edges_same_owner_is_fail_closed() {
    // Matching non-empty handles are same-owner; an empty handle on either
    // side, or a mismatch, is foreign (never fold a foreign remix in).
    let matched = Clip {
        handle: "me".into(),
        clip_roots: vec![clip_root("r", "me")],
        ..Default::default()
    };
    assert!(attribution_edges(&matched)[0].same_owner);

    let clip_blank = Clip {
        handle: String::new(),
        clip_roots: vec![clip_root("r", "me")],
        ..Default::default()
    };
    assert!(
        !attribution_edges(&clip_blank)[0].same_owner,
        "an empty clip handle is fail-closed to foreign"
    );

    let root_blank = Clip {
        handle: "me".into(),
        clip_roots: vec![clip_root("r", "   ")],
        ..Default::default()
    };
    assert!(
        !attribution_edges(&root_blank)[0].same_owner,
        "a whitespace-only root handle is fail-closed to foreign"
    );
}

#[test]
fn attribution_edges_skip_a_root_with_no_id_and_keep_contiguous_ordinals() {
    let clip = Clip {
        handle: "me".into(),
        clip_attribution_type: "remix".into(),
        clip_roots: vec![
            clip_root("", "me"),
            clip_root(ZERO_UUID, "me"),
            clip_root("real-root", "me"),
        ],
        ..Default::default()
    };
    let edges = attribution_edges(&clip);
    assert_eq!(edges.len(), 1, "empty and sentinel root ids are dropped");
    assert_eq!(edges[0].parent_id, "real-root");
    assert_eq!(edges[0].ordinal, 0, "ordinals stay contiguous after a skip");
}

fn resolution_with(roots: Vec<(&str, RootInfo)>) -> Resolution {
    Resolution {
        roots: roots
            .into_iter()
            .map(|(id, info)| (id.to_owned(), info))
            .collect(),
        gap_filled: Vec::new(),
        bridges: Vec::new(),
    }
}

#[test]
fn context_for_a_root_uses_its_own_id_and_title() {
    let root = Clip {
        id: "root-1".into(),
        title: "Original".into(),
        ..Default::default()
    };
    let resolution = resolution_with(vec![(
        "root-1",
        RootInfo {
            root_id: "root-1".into(),
            root_title: "Original".into(),
            status: ResolveStatus::Resolved,
        },
    )]);

    let ctx = LineageContext::for_clip(&root, &resolution);
    assert_eq!(ctx.root_id, "root-1");
    assert_eq!(ctx.root_title, "Original");
    assert_eq!(ctx.parent_id, "");
    assert_eq!(ctx.edge_type, None);
    // A root folders under its own title.
    assert_eq!(ctx.album("Original"), "Original");
}

#[test]
fn context_for_a_remix_carries_root_and_parent() {
    let child = Clip {
        id: "child-1".into(),
        title: "Remix".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "root-1".into(),
        edited_clip_id: "root-1".into(),
        ..Default::default()
    };
    let resolution = resolution_with(vec![(
        "child-1",
        RootInfo {
            root_id: "root-1".into(),
            root_title: "Original".into(),
            status: ResolveStatus::Resolved,
        },
    )]);

    let ctx = LineageContext::for_clip(&child, &resolution);
    assert_eq!(ctx.root_id, "root-1");
    assert_eq!(ctx.root_title, "Original");
    assert_eq!(ctx.parent_id, "root-1");
    assert_eq!(ctx.edge_type, Some(EdgeType::Cover));
    // A remix folders under the root's album title, not its own.
    assert_eq!(ctx.album("Remix"), "Original");
}

#[test]
fn context_absent_from_resolution_is_its_own_root() {
    let clip = Clip {
        id: "lonely".into(),
        title: "Solo".into(),
        ..Default::default()
    };
    let ctx = LineageContext::for_clip(&clip, &resolution_with(vec![]));
    assert_eq!(ctx.root_id, "lonely");
    assert_eq!(ctx.root_title, "Solo");
    assert_eq!(ctx.status, ResolveStatus::Resolved);
    assert_eq!(ctx.album("Solo"), "Solo");
}

#[test]
fn album_falls_back_to_own_title_when_root_title_is_empty() {
    let ctx = external_cover();
    assert_eq!(ctx.album("My Title"), "My Title");
}

#[test]
fn own_root_has_no_parent() {
    let clip = Clip {
        id: "solo".into(),
        title: "Solo".into(),
        ..Default::default()
    };
    let ctx = LineageContext::own_root(&clip);
    assert_eq!(ctx.root_id, "solo");
    assert_eq!(ctx.parent_id, "");
    assert_eq!(ctx.edge_type, None);
}

#[test]
fn year_prefers_root_then_falls_back_to_own() {
    // year() tags the root's year (so a revision across a year boundary still
    // groups under the root), falls back to the clip's own date when the root
    // date is unknown, and is empty when no date is known at all.
    struct Row {
        label: &'static str,
        ctx: LineageContext,
        now: &'static str,
        want: &'static str,
    }
    let rows = vec![
        Row {
            label: "root year wins across a December/January boundary",
            ctx: LineageContext {
                root_id: "root-1".into(),
                root_title: "Origin".into(),
                root_date: "2023-12-30T23:00:00Z".into(),
                parent_id: "root-1".into(),
                edge_type: Some(EdgeType::Extend),
                status: ResolveStatus::Resolved,
                track: 0,
                track_total: 0,
            },
            now: "2024-01-02T08:00:00Z",
            want: "2023",
        },
        Row {
            label: "falls back to own year when the root date is unavailable",
            ctx: external_cover(),
            now: "2024-07-01T00:00:00Z",
            want: "2024",
        },
        Row {
            label: "empty when no date is known",
            ctx: LineageContext::own_root(&Clip::default()),
            now: "",
            want: "",
        },
    ];
    for Row {
        label,
        ctx,
        now,
        want,
    } in rows
    {
        assert_eq!(ctx.year(now), want, "{label}");
    }
}

#[test]
fn own_root_tags_its_own_year() {
    let clip = Clip {
        id: "solo".into(),
        title: "Solo".into(),
        created_at: "2022-05-06T12:00:00Z".into(),
        ..Default::default()
    };
    let ctx = LineageContext::own_root(&clip);
    assert_eq!(ctx.root_date, "2022-05-06T12:00:00Z");
    assert_eq!(ctx.year(&clip.created_at), "2022");
}
