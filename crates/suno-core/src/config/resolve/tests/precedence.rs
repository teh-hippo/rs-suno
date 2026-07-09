use super::*;

#[test]
fn account_id_parses_and_resolves() {
    let toml = r#"
        [accounts.alice]
        token = "tok"
        root = "/music"
        account_id = "user_abc123"
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    assert_eq!(
        cfg.accounts["alice"].account_id.as_deref(),
        Some("user_abc123")
    );
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(eff.account_id.as_deref(), Some("user_abc123"));
}

#[test]
fn compiled_defaults_when_nothing_set() {
    let toml = "[accounts.alice]\n";
    let cfg = Config::from_toml(toml).unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(
        eff,
        EffectiveSettings {
            token: None,
            stored_token: None,
            token_command: None,
            account_id: None,
            format: AudioFormat::Flac,
            concurrency: 4,
            retries: 3,
            min_newest: 1,
            animated_covers: false,
            raw_animated_cover: false,
            video_cover_retention: VideoCoverRetention::Neither,
            animated_cover_webp: WebpEncodeSettings::default(),
            details_sidecar: false,
            lyrics_sidecar: false,
            lrc_sidecar: false,
            video_mp4: false,
            download_stems: false,
            stem_format: StemFormat::Wav,
            naming_template: crate::naming::DEFAULT_TEMPLATE.to_owned(),
            character_set: CharacterSet::Unicode,
            areas: None,
            album_overrides: BTreeMap::new(),
            lead_tracks: Vec::new(),
            number_singletons: true,
        }
    );
}

/// Guards that every [`Settings`] field is threaded through
/// [`Config::resolve`]. The `sentinel` literal has no `..Default::default()`,
/// so adding a field is a compile error until it is given a distinct,
/// non-default value here, then asserted against the resolved output. The
/// per-field `assert_eq!`s are maintained by hand, so this proves a new field
/// is *named* in the sentinel, not necessarily *asserted*.

#[test]
fn resolve_reflects_every_settings_field() {
    let sentinel = Settings {
        format: Some(AudioFormat::Mp3),
        concurrency: Some(99),
        retries: Some(98),
        min_newest: Some(42),
        token_command: Some("sentinel-token-cmd".into()),
        animated_covers: Some(true),
        video_cover_retention: Some(VideoCoverRetention::Both),
        animated_cover_quality: Some(77),
        animated_cover_max_fps: Some(13),
        animated_cover_max_width: Some(333),
        animated_cover_compression_level: Some(3),
        animated_cover_lossless: Some(true),
        details_sidecar: Some(true),
        lyrics_sidecar: Some(true),
        lrc_sidecar: Some(true),
        video_mp4: Some(true),
        download_stems: Some(true),
        stem_format: Some(StemFormat::Mp3),
        naming_template: Some("SENTINEL/{id}".into()),
        character_set: Some(CharacterSet::Ascii),
        number_singletons: Some(false),
    };
    let cfg = Config {
        defaults: Defaults { settings: sentinel },
        accounts: HashMap::from([("alice".to_owned(), AccountConfig::default())]),
    };

    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();

    assert_eq!(eff.format, AudioFormat::Mp3);
    assert_eq!(eff.concurrency, 99);
    assert_eq!(eff.retries, 98);
    assert_eq!(eff.min_newest, 42);
    assert_eq!(eff.token_command.as_deref(), Some("sentinel-token-cmd"));
    // Retention `both` drives both webp and mp4 retention.
    assert!(eff.animated_covers);
    assert!(eff.raw_animated_cover);
    assert_eq!(eff.video_cover_retention, VideoCoverRetention::Both);
    assert_eq!(eff.animated_cover_webp.quality, 77);
    assert_eq!(eff.animated_cover_webp.max_fps, 13);
    assert_eq!(eff.animated_cover_webp.max_width, Some(333));
    assert_eq!(eff.animated_cover_webp.compression_level, 3);
    assert!(eff.animated_cover_webp.lossless);
    assert!(eff.details_sidecar);
    assert!(eff.lyrics_sidecar);
    assert!(eff.lrc_sidecar);
    assert!(eff.video_mp4);
    assert!(eff.download_stems);
    assert_eq!(eff.stem_format, StemFormat::Mp3);
    assert_eq!(eff.naming_template, "SENTINEL/{id}");
    assert_eq!(eff.character_set, CharacterSet::Ascii);
    assert!(!eff.number_singletons);
}

#[test]
fn unknown_source_falls_back_to_account() {
    let toml = r#"
        [accounts.alice]
        format = "wav"
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    let eff = cfg
        .resolve("alice", Some("nonexistent"), &no_env(), &no_flags())
        .unwrap();
    assert_eq!(eff.format, AudioFormat::Wav);
}

#[test]
fn env_enum_parsing_is_case_sensitive() {
    // cfg-3: the env tiers parse enums case-sensitively, matching serde (TOML)
    // and the JSON schema, so `SUNO_FORMAT=FLAC` errors just like the file's
    // `format = "FLAC"` rather than being silently accepted.
    let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
    let upper: HashMap<String, String> = [("SUNO_FORMAT".into(), "FLAC".into())]
        .into_iter()
        .collect();
    assert!(cfg.resolve("alice", None, &upper, &no_flags()).is_err());
    let lower: HashMap<String, String> = [("SUNO_FORMAT".into(), "flac".into())]
        .into_iter()
        .collect();
    assert_eq!(
        cfg.resolve("alice", None, &lower, &no_flags())
            .unwrap()
            .format,
        AudioFormat::Flac
    );
}

#[test]
fn per_account_env_label_uppersnakedcase() {
    let toml = "[accounts.my-lib]\n";
    let cfg = Config::from_toml(toml).unwrap();
    let env: HashMap<String, String> = [("SUNO_MY_LIB_FORMAT".into(), "wav".into())]
        .into_iter()
        .collect();
    let eff = cfg.resolve("my-lib", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.format, AudioFormat::Wav);
}

#[test]
fn invalid_env_u32_errors() {
    let toml = "[accounts.alice]\n";
    let cfg = Config::from_toml(toml).unwrap();
    let env: HashMap<String, String> = [("SUNO_CONCURRENCY".into(), "many".into())]
        .into_iter()
        .collect();
    assert!(cfg.resolve("alice", None, &env, &no_flags()).is_err());
}

#[test]
fn invalid_env_bool_errors() {
    let toml = "[accounts.alice]\n";
    let cfg = Config::from_toml(toml).unwrap();
    let env: HashMap<String, String> = [("SUNO_ANIMATED_COVERS".into(), "yes".into())]
        .into_iter()
        .collect();
    assert!(cfg.resolve("alice", None, &env, &no_flags()).is_err());
}

#[test]
fn unknown_account_errors() {
    let cfg = Config::from_toml("").unwrap();
    assert!(cfg.resolve("nobody", None, &no_env(), &no_flags()).is_err());
}

#[test]
fn format_follows_precedence() {
    let toml = r#"
        [defaults]
        format = "wav"

        [accounts.alice]
        format = "mp3"

        [accounts.alice.sources.liked]
        format = "alac"
    "#;
    let cfg = Config::from_toml(toml).unwrap();

    // Compiled default (FLAC) when nothing is set.
    let bare = Config::from_toml("[accounts.bob]\n").unwrap();
    assert_eq!(
        bare.resolve("bob", None, &no_env(), &no_flags())
            .unwrap()
            .format,
        AudioFormat::Flac
    );

    // Per-source wins over account and defaults.
    let eff = cfg
        .resolve("alice", Some("liked"), &no_env(), &no_flags())
        .unwrap();
    assert_eq!(eff.format, AudioFormat::Alac);

    // Account wins over defaults.
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(eff.format, AudioFormat::Mp3);

    // Global env overrides file.
    let env: HashMap<String, String> = [("SUNO_FORMAT".into(), "wav".into())].into_iter().collect();
    let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.format, AudioFormat::Wav);

    // Per-account env overrides global env.
    let env: HashMap<String, String> = [
        ("SUNO_FORMAT".into(), "wav".into()),
        ("SUNO_ALICE_FORMAT".into(), "flac".into()),
    ]
    .into_iter()
    .collect();
    let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.format, AudioFormat::Flac);

    // Flag overrides env.
    let flags = FlagOverrides {
        settings: Settings {
            format: Some(AudioFormat::Alac),
            ..Default::default()
        },
        ..Default::default()
    };
    let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
    assert_eq!(eff.format, AudioFormat::Alac);

    // An unknown env value is a config error, never a silent default.
    let bad_env: HashMap<String, String> = [("SUNO_FORMAT".into(), "aiff".into())]
        .into_iter()
        .collect();
    assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());
}

#[test]
fn naming_template_follows_precedence() {
    let toml = r#"
        [defaults]
        naming_template = "{title}"

        [accounts.alice]
        naming_template = "{creator}/{title}"

        [accounts.alice.sources.liked]
        naming_template = "{handle}/{title} [{id8}]"
    "#;
    let cfg = Config::from_toml(toml).unwrap();

    // Per-source wins over account.
    let eff = cfg
        .resolve("alice", Some("liked"), &no_env(), &no_flags())
        .unwrap();
    assert_eq!(eff.naming_template, "{handle}/{title} [{id8}]");

    // Account wins over defaults.
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(eff.naming_template, "{creator}/{title}");

    // Env overrides file.
    let env: HashMap<String, String> = [("SUNO_NAMING_TEMPLATE".into(), "{id}".into())]
        .into_iter()
        .collect();
    let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.naming_template, "{id}");

    // Flag overrides env.
    let flags = FlagOverrides {
        settings: Settings {
            naming_template: Some("{title}/{id8}".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
    assert_eq!(eff.naming_template, "{title}/{id8}");
}

#[test]
fn min_newest_follows_precedence() {
    // `min_newest` is the deletion-safety floor: every tier boundary in its
    // precedence (flag > per-account env > global env > source > account >
    // defaults > compiled 1) must hold exactly.
    let toml = r#"
        [defaults]
        min_newest = 5

        [accounts.alice]
        min_newest = 7

        [accounts.alice.sources.liked]
        min_newest = 9
    "#;
    let cfg = Config::from_toml(toml).unwrap();

    // Compiled default when nothing is set.
    let bare = Config::from_toml("[accounts.bob]\n").unwrap();
    assert_eq!(
        bare.resolve("bob", None, &no_env(), &no_flags())
            .unwrap()
            .min_newest,
        1
    );

    // Per-source wins over account and defaults.
    let eff = cfg
        .resolve("alice", Some("liked"), &no_env(), &no_flags())
        .unwrap();
    assert_eq!(eff.min_newest, 9);

    // Account wins over defaults.
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(eff.min_newest, 7);

    // Global env overrides file.
    let env: HashMap<String, String> = [("SUNO_MIN_NEWEST".into(), "11".into())]
        .into_iter()
        .collect();
    let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.min_newest, 11);

    // Per-account env overrides global env.
    let env: HashMap<String, String> = [
        ("SUNO_MIN_NEWEST".into(), "11".into()),
        ("SUNO_ALICE_MIN_NEWEST".into(), "13".into()),
    ]
    .into_iter()
    .collect();
    let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.min_newest, 13);

    // Flag overrides env.
    let flags = FlagOverrides {
        settings: Settings {
            min_newest: Some(15),
            ..Default::default()
        },
        ..Default::default()
    };
    let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
    assert_eq!(eff.min_newest, 15);

    // An invalid env value is a config error, never a silently lowered floor.
    let bad_env: HashMap<String, String> = [("SUNO_MIN_NEWEST".into(), "notnum".into())]
        .into_iter()
        .collect();
    assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());
}

#[test]
fn character_set_follows_precedence() {
    let toml = r#"
        [defaults]
        character_set = "ascii"

        [accounts.alice]
    "#;
    let cfg = Config::from_toml(toml).unwrap();

    // File default applies.
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(eff.character_set, CharacterSet::Ascii);

    // Env overrides file.
    let env: HashMap<String, String> = [("SUNO_CHARACTER_SET".into(), "unicode".into())]
        .into_iter()
        .collect();
    let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.character_set, CharacterSet::Unicode);

    // Flag overrides env.
    let flags = FlagOverrides {
        settings: Settings {
            character_set: Some(CharacterSet::Ascii),
            ..Default::default()
        },
        ..Default::default()
    };
    let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
    assert_eq!(eff.character_set, CharacterSet::Ascii);
}

#[test]
fn invalid_character_set_env_errors() {
    let toml = "[accounts.alice]\n";
    let cfg = Config::from_toml(toml).unwrap();
    let env: HashMap<String, String> = [("SUNO_CHARACTER_SET".into(), "utf8".into())]
        .into_iter()
        .collect();
    assert!(cfg.resolve("alice", None, &env, &no_flags()).is_err());
}

#[test]
fn album_overrides_parse_and_resolve() {
    let toml = r#"
        [accounts.alice]
        token = "t"
        [accounts.alice.albums]
        "root_abc123" = "Preferred Name"
        "root_def456" = "Another Album"
        "root_blank" = "   "
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    assert_eq!(
        cfg.accounts["alice"].albums["root_abc123"],
        "Preferred Name"
    );
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(eff.album_overrides["root_abc123"], "Preferred Name");
    assert_eq!(eff.album_overrides["root_def456"], "Another Album");
    // A blank value is dropped so it can never blank an album.
    assert!(!eff.album_overrides.contains_key("root_blank"));
}

#[test]
fn album_overrides_absent_by_default() {
    let cfg = Config::from_toml("[accounts.alice]\ntoken = \"t\"\n").unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert!(eff.album_overrides.is_empty());
}

#[test]
fn lead_tracks_parse_trim_and_dedupe() {
    let toml = r#"
        [accounts.alice]
        token = "t"
        lead_tracks = ["  b320f4cf  ", "c6f6a1a5", "b320f4cf", "   "]
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    // Trimmed, de-duplicated, blank dropped, deterministically ordered.
    assert_eq!(eff.lead_tracks, vec!["b320f4cf", "c6f6a1a5"]);
}

#[test]
fn lead_tracks_absent_by_default() {
    let cfg = Config::from_toml("[accounts.alice]\ntoken = \"t\"\n").unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert!(eff.lead_tracks.is_empty());
}

#[test]
fn number_singletons_defaults_true_and_can_be_disabled() {
    let cfg = Config::from_toml("[accounts.alice]\ntoken = \"t\"\n").unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert!(eff.number_singletons, "defaults to true");

    let off =
        Config::from_toml("[accounts.alice]\ntoken = \"t\"\nnumber_singletons = false\n").unwrap();
    let eff = off.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert!(!eff.number_singletons);
}
