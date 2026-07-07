use super::*;
use crate::model::HistoryEntry;

fn history(id: &str) -> HistoryEntry {
    HistoryEntry {
        id: id.to_owned(),
        ..Default::default()
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
fn classifies_speed_edit_from_speed_pointer_without_edited() {
    // Real `chain2` shape: edit_speed carries speed_clip_id and no edited_clip_id.
    let clip = Clip {
        id: "6e5193b1".into(),
        title: "Go Xavi Go, Fast. (Drum n' Bass Version)".into(),
        clip_type: "edit_speed".into(),
        is_remix: true,
        speed_clip_id: "2b69882c".into(),
        ..Default::default()
    };
    assert_eq!(edge_type(&clip), Some(EdgeType::SpeedEdit));
    assert_eq!(
        immediate_parent(&clip),
        Some(("2b69882c".into(), EdgeType::SpeedEdit))
    );
}

#[test]
fn empty_task_gen_is_a_root() {
    // Real `chain2` root: gen with an empty task string.
    let clip = Clip {
        id: "b4f16694".into(),
        title: "Go Xavi Go, Fast.".into(),
        clip_type: "gen".into(),
        task: String::new(),
        ..Default::default()
    };
    assert_eq!(edge_type(&clip), None);
    assert_eq!(immediate_parent(&clip), None);
}

#[test]
fn classifies_extend_from_history_head() {
    let clip = Clip {
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
    };
    assert_eq!(edge_type(&clip), Some(EdgeType::Extend));
    assert_eq!(
        immediate_parent(&clip),
        Some(("0a3c311a".into(), EdgeType::Extend))
    );
}

#[test]
fn classifies_infill_with_override_history_precedence() {
    // Real infill shape: override_history wins over future, history, and edited.
    let clip = Clip {
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
    };
    assert_eq!(edge_type(&clip), Some(EdgeType::SectionReplace));
    assert_eq!(
        immediate_parent(&clip),
        Some(("d3d28e59".into(), EdgeType::SectionReplace))
    );
}

#[test]
fn fixed_infill_is_also_section_replace() {
    let clip = Clip {
        task: "fixed_infill".into(),
        override_history_clip_id: "past".into(),
        edited_clip_id: "edited".into(),
        ..Default::default()
    };
    assert_eq!(edge_type(&clip), Some(EdgeType::SectionReplace));
    assert_eq!(
        immediate_parent(&clip),
        Some(("past".into(), EdgeType::SectionReplace))
    );
}

#[test]
fn classifies_stitch_from_concat_base() {
    // Real concat shape: type=concat, base segment first in concat_history.
    let clip = Clip {
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
    };
    assert_eq!(edge_type(&clip), Some(EdgeType::Stitch));
    assert_eq!(
        immediate_parent(&clip),
        Some(("ead64fbe".into(), EdgeType::Stitch))
    );
}

#[test]
fn inherited_concat_history_without_concat_type_is_not_a_stitch() {
    // Suno copies a parent stitch's concat_history onto derived clips. A
    // plain `gen` that merely carries it (no type=concat, no other marker)
    // must NOT be read as a stitch; here it has no parent pointer, so it is
    // a root.
    let clip = Clip {
        clip_type: "gen".into(),
        concat_history: vec![history("base"), history("second")],
        ..Default::default()
    };
    assert_eq!(edge_type(&clip), None);
    assert_eq!(immediate_parent(&clip), None);
}

#[test]
fn cover_of_a_stitch_classifies_as_cover_not_stitch() {
    // A cover OF a stitched track inherits the parent's concat_history but is
    // itself a cover: it must classify as Cover and parent via cover_clip_id,
    // never as a Stitch pointing at an inherited concat segment.
    let clip = Clip {
        id: "cov".into(),
        title: "Cover of a stitch".into(),
        clip_type: "gen".into(),
        task: "cover".into(),
        cover_clip_id: "stitch-parent".into(),
        edited_clip_id: "stitch-parent".into(),
        concat_history: vec![history("inherited-base"), history("inherited-seg")],
        ..Default::default()
    };
    assert_eq!(edge_type(&clip), Some(EdgeType::Cover));
    assert_eq!(
        immediate_parent(&clip),
        Some(("stitch-parent".into(), EdgeType::Cover))
    );
}

#[test]
fn upload_is_a_root() {
    let clip = Clip {
        id: "4770ef56".into(),
        title: "Uploaded audio".into(),
        clip_type: "upload".into(),
        ..Default::default()
    };
    assert_eq!(edge_type(&clip), None);
    assert_eq!(immediate_parent(&clip), None);
}

#[test]
fn edited_only_clip_is_derived() {
    // A task the resolver has no specific rule for, but a parent pointer.
    let clip = Clip {
        clip_type: "gen".into(),
        task: "chop_sample_condition".into(),
        edited_clip_id: "parent-x".into(),
        ..Default::default()
    };
    assert_eq!(edge_type(&clip), Some(EdgeType::Derived));
    assert_eq!(
        immediate_parent(&clip),
        Some(("parent-x".into(), EdgeType::Derived))
    );
}

#[test]
fn unmarked_clip_without_pointer_is_a_root() {
    let clip = Clip {
        clip_type: "gen".into(),
        task: "chop_sample_condition".into(),
        ..Default::default()
    };
    assert_eq!(edge_type(&clip), None);
    assert_eq!(immediate_parent(&clip), None);
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
fn lineage_edges_of_a_root_is_empty() {
    let clip = Clip {
        clip_type: "gen".into(),
        ..Default::default()
    };
    assert!(lineage_edges(&clip).is_empty());
}

#[test]
fn lineage_edges_records_stitch_secondaries_in_order() {
    let clip = Clip {
        clip_type: "concat".into(),
        concat_history: vec![history("base"), history("seg1"), history("seg2")],
        ..Default::default()
    };
    let edges = lineage_edges(&clip);
    assert_eq!(
        edges,
        vec![
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
        ]
    );
}

#[test]
fn lineage_edges_emits_secondaries_when_the_primary_is_absent() {
    // A stitch whose base segment id is empty still has real secondary
    // segments: they must be emitted (with their own ordinals) rather than
    // dropped for want of a primary.
    let clip = Clip {
        clip_type: "concat".into(),
        concat_history: vec![history(""), history("seg1"), history("seg2")],
        ..Default::default()
    };
    let edges = lineage_edges(&clip);
    assert_eq!(
        edges,
        vec![
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
        "secondaries survive an empty primary base segment"
    );
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
        handle: "".into(),
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
    let ctx = LineageContext {
        root_id: "outside".into(),
        root_title: String::new(),
        root_date: String::new(),
        parent_id: "outside".into(),
        edge_type: Some(EdgeType::Cover),
        status: ResolveStatus::External,
    };
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
fn year_prefers_the_root_year_over_the_clips_own() {
    // A December root with a January revision: the child tags the root's
    // year so the album groups under one year across the boundary.
    let ctx = LineageContext {
        root_id: "root-1".into(),
        root_title: "Origin".into(),
        root_date: "2023-12-30T23:00:00Z".into(),
        parent_id: "root-1".into(),
        edge_type: Some(EdgeType::Extend),
        status: ResolveStatus::Resolved,
    };
    assert_eq!(ctx.year("2024-01-02T08:00:00Z"), "2023");
}

#[test]
fn year_falls_back_to_own_when_the_root_date_is_unavailable() {
    let ctx = LineageContext {
        root_id: "outside".into(),
        root_title: String::new(),
        root_date: String::new(),
        parent_id: "outside".into(),
        edge_type: Some(EdgeType::Cover),
        status: ResolveStatus::External,
    };
    assert_eq!(ctx.year("2024-07-01T00:00:00Z"), "2024");
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

#[test]
fn year_is_empty_when_no_date_is_known() {
    let clip = Clip::default();
    let ctx = LineageContext::own_root(&clip);
    assert_eq!(ctx.year(&clip.created_at), "");
}
