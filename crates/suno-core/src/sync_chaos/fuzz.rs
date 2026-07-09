//! Layer 4: parser and template robustness against arbitrary, malformed input.
//!
//! The reconcile and execute layers trust their inputs; the parsers are the
//! boundary where untrusted bytes enter. A panic here is a crash, and a bad
//! path is a corrupt library, so every parse and naming entry point, plus the
//! shared clip mapper they funnel through, must survive arbitrary input without
//! panicking. Where the result shape lets us say
//! more, we assert more: a rendered name is additionally checked to be a safe
//! relative path. These properties feed garbage into [`map_clip`], the
//! feed reader behind [`SunoClient::list_clips`], [`RecencySpec::parse`],
//! [`Config::from_toml`], and [`render_clip_name`], and assert exactly that.

use std::path::Component;

use proptest::collection::vec;
use proptest::prelude::*;
use serde_json::{Map, Value};

use crate::auth::ClerkAuth;
use crate::client::SunoClient;
use crate::config::Config;
use crate::lineage::LineageContext;
use crate::model::Clip;
use crate::naming::{
    CharacterSet, DEFAULT_TEMPLATE, NamingConfig, NamingRequest, is_reserved_name, render_clip_name,
};
use crate::select::RecencySpec;
use crate::testutil::{ChaosHttp, Outcome, RecordingClock};
use crate::wire::map_clip;

/// A recursive arbitrary JSON value: nulls, bools, integers, arbitrary strings,
/// and nested arrays and objects. Floats are omitted so the generator can never
/// itself fail to build a `serde_json::Number`.
fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| Value::Number(n.into())),
        any::<String>().prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 48, 8, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..6).prop_map(Value::Array),
            vec(("[a-zA-Z0-9_]{0,8}", inner), 0..6).prop_map(|pairs| {
                Value::Object(pairs.into_iter().collect::<Map<String, Value>>())
            }),
        ]
    })
}

/// A clip whose path-bearing fields are arbitrary, to stress the namer.
fn arb_clip() -> impl Strategy<Value = Clip> {
    (
        any::<String>(),
        any::<String>(),
        any::<String>(),
        any::<String>(),
    )
        .prop_map(|(id, title, display_name, handle)| Clip {
            id,
            title,
            display_name,
            handle,
            ..Default::default()
        })
}

/// A naming template: the default, a fully arbitrary string, or a join of
/// adversarial segments (placeholders mixed with dot, dot-dot, and literals).
fn arb_template() -> impl Strategy<Value = String> {
    let segment = prop_oneof![
        Just("{creator}".to_string()),
        Just("{handle}".to_string()),
        Just("{album}".to_string()),
        Just("{title}".to_string()),
        Just("{id}".to_string()),
        Just(".".to_string()),
        Just("..".to_string()),
        Just("lit".to_string()),
        Just(String::new()),
    ];
    prop_oneof![
        Just(DEFAULT_TEMPLATE.to_string()),
        any::<String>(),
        vec(segment, 0..6).prop_map(|segs| segs.join("/")),
    ]
}

proptest! {
    /// Mapping any JSON value to a clip never panics.
    #[test]
    fn map_clip_never_panics(value in arb_json()) {
        let _ = map_clip(&value);
    }

    /// Reading a feed page of arbitrary bytes never panics. The result (clips or
    /// an error) is not inspected; only the absence of a panic is asserted.
    #[test]
    fn list_clips_survives_arbitrary_feed_bytes(body in any::<Vec<u8>>()) {
        let http = ChaosHttp::new()
            .with_auth()
            .program("/api/feed/v3", vec![Outcome::ok(body)]);
        let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
        let _ = pollster::block_on(client.list_clips(&http, false, Some(3)));
    }

    /// Reading a feed page of arbitrary *valid* JSON never panics either, which
    /// exercises the clip-array and `has_more` navigation rather than only the
    /// JSON syntax error path.
    #[test]
    fn list_clips_survives_arbitrary_feed_json(value in arb_json()) {
        let body = serde_json::to_vec(&value).expect("arb_json is serialisable");
        let http = ChaosHttp::new()
            .with_auth()
            .program("/api/feed/v3", vec![Outcome::ok(body)]);
        let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
        let _ = pollster::block_on(client.list_clips(&http, true, Some(3)));
    }

    /// Parsing any recency spec never panics. Its specific value is checked by
    /// the deterministic test below; here only the absence of a panic matters.
    #[test]
    fn recency_spec_parse_never_panics(spec in any::<String>()) {
        let _ = RecencySpec::parse(&spec);
    }

    /// Parsing any TOML string never panics. The `Result` is discarded: this
    /// asserts only that arbitrary input cannot crash the config reader.
    #[test]
    fn config_from_toml_never_panics(text in any::<String>()) {
        let _ = Config::from_toml(&text);
    }

    /// Rendering a clip name for an arbitrary clip, template, length cap, and
    /// character set never panics and always yields a safe relative path: at
    /// least one component, every component non-empty, free of separators, and
    /// never `.` or `..` (so a hostile title can never escape the library
    /// root). Every component is additionally free of the Windows-forbidden
    /// characters and control codes, never a reserved device name (`CON`,
    /// `NUL`, `COM1`, ...), and pure ASCII under the ASCII character set, so a
    /// title can neither smuggle a `:` or a control byte into a path nor, in
    /// ASCII mode, a non-ASCII byte.
    #[test]
    fn render_clip_name_is_always_a_safe_relative_path(
        clip in arb_clip(),
        template in arb_template(),
        character_set in prop_oneof![
            Just(CharacterSet::Unicode),
            Just(CharacterSet::Ascii),
        ],
        max_component_len in 1usize..120,
    ) {
        let config = NamingConfig {
            template,
            character_set,
            max_component_len,
        };
        let lineage = LineageContext::own_root(&clip);
        let request = NamingRequest { clip: &clip, lineage: &lineage };
        let rendered = render_clip_name(request, &config);

        prop_assert!(rendered.relative_path.is_relative(), "the path must be relative");
        prop_assert!(
            rendered.relative_path.components().count() >= 1,
            "the path must have at least one component",
        );
        for component in rendered.relative_path.components() {
            match component {
                Component::Normal(part) => {
                    let text = part.to_string_lossy();
                    prop_assert!(!text.is_empty(), "no empty component");
                    prop_assert!(
                        !text.contains('/') && !text.contains('\\'),
                        "no separator inside a component: {text:?}",
                    );
                    prop_assert_ne!(text.as_ref(), ".", "no current-dir component");
                    prop_assert_ne!(text.as_ref(), "..", "no parent-dir component");
                    prop_assert!(
                        !is_reserved_name(&text),
                        "no Windows reserved device name: {text:?}",
                    );
                    prop_assert!(
                        !text.chars().any(|c| {
                            matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_control()
                        }),
                        "no forbidden or control character: {text:?}",
                    );
                    if character_set == CharacterSet::Ascii {
                        prop_assert!(
                            text.is_ascii(),
                            "the ASCII character set must yield a pure-ASCII component: {text:?}",
                        );
                    }
                }
                other => prop_assert!(false, "unexpected non-normal component: {other:?}"),
            }
        }
    }
}

/// A targeted sanity check that the recency grammar still means what the fuzz
/// assumes: the known-good forms parse and obvious garbage is rejected.
#[test]
fn recency_spec_parses_known_forms_and_rejects_garbage() {
    assert!(matches!(
        RecencySpec::parse("last-run"),
        Ok(RecencySpec::LastRun)
    ));
    assert!(matches!(RecencySpec::parse("7d"), Ok(RecencySpec::Relative(s)) if s == 7 * 86_400));
    assert!(matches!(
        RecencySpec::parse("2w"),
        Ok(RecencySpec::Relative(s)) if s == 2 * 7 * 86_400
    ));
    assert!(
        RecencySpec::parse("12x").is_err(),
        "unknown unit must error"
    );
    assert!(
        RecencySpec::parse("notaspec").is_err(),
        "non-numeric must error"
    );
    assert!(RecencySpec::parse("").is_err(), "empty must error");
}
