use super::*;
use crate::config::fixtures::{no_env, no_flags};
use crate::config::{AccountConfig, Defaults, Settings};
use crate::vocab::StemFormat;
use std::collections::BTreeMap;

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
}

#[test]
fn file_defaults_override_compiled() {
    let toml = r#"
        [defaults]
        format = "mp3"
        concurrency = 8

        [accounts.alice]
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(eff.format, AudioFormat::Mp3);
    assert_eq!(eff.concurrency, 8);
    assert_eq!(eff.retries, 3); // compiled default
}

#[test]
fn account_settings_override_defaults() {
    let toml = r#"
        [defaults]
        format = "mp3"

        [accounts.alice]
        format = "wav"
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(eff.format, AudioFormat::Wav);
}

#[test]
fn per_source_overrides_account() {
    let toml = r#"
        [accounts.alice]
        format = "flac"

        [accounts.alice.sources.liked]
        format = "mp3"
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    let eff = cfg
        .resolve("alice", Some("liked"), &no_env(), &no_flags())
        .unwrap();
    assert_eq!(eff.format, AudioFormat::Mp3);
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
fn global_env_overrides_file() {
    let toml = r#"
        [accounts.alice]
        format = "flac"
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    let env: HashMap<String, String> = [("SUNO_FORMAT".into(), "mp3".into())].into_iter().collect();
    let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.format, AudioFormat::Mp3);
}

#[test]
fn per_account_env_overrides_global_env() {
    let toml = "[accounts.alice]\n";
    let cfg = Config::from_toml(toml).unwrap();
    let env: HashMap<String, String> = [
        ("SUNO_FORMAT".into(), "mp3".into()),
        ("SUNO_ALICE_FORMAT".into(), "wav".into()),
    ]
    .into_iter()
    .collect();
    let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.format, AudioFormat::Wav);
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
fn flag_overrides_env_and_file() {
    let toml = r#"
        [accounts.alice]
        format = "flac"
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    let env: HashMap<String, String> = [("SUNO_FORMAT".into(), "mp3".into())].into_iter().collect();
    let flags = FlagOverrides {
        settings: Settings {
            format: Some(AudioFormat::Wav),
            ..Default::default()
        },
        ..Default::default()
    };
    let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
    assert_eq!(eff.format, AudioFormat::Wav);
}

#[test]
fn token_precedence() {
    let toml = r#"
        [accounts.alice]
        token = "file_tok"
    "#;
    let cfg = Config::from_toml(toml).unwrap();

    // env overrides file
    let env: HashMap<String, String> = [("SUNO_TOKEN".into(), "env_tok".into())]
        .into_iter()
        .collect();
    let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.token.as_deref(), Some("env_tok"));
    assert_eq!(eff.stored_token.as_deref(), Some("file_tok"));

    // flag overrides env
    let flags = FlagOverrides {
        token: Some("flag_tok".into()),
        ..Default::default()
    };
    let eff = cfg.resolve("alice", None, &env, &flags).unwrap();
    assert_eq!(eff.token.as_deref(), Some("flag_tok"));
    assert_eq!(eff.stored_token.as_deref(), Some("file_tok"));
}

#[test]
fn stored_token_is_populated_from_config_when_no_override_exists() {
    let toml = r#"
        [accounts.alice]
        token = "file_tok"
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(eff.token, None);
    assert_eq!(eff.stored_token.as_deref(), Some("file_tok"));
    assert_eq!(eff.token_command, None);
}

#[test]
fn per_account_token_env_overrides_global() {
    let toml = "[accounts.alice]\n";
    let cfg = Config::from_toml(toml).unwrap();
    let env: HashMap<String, String> = [
        ("SUNO_TOKEN".into(), "global".into()),
        ("SUNO_ALICE_TOKEN".into(), "per_account".into()),
    ]
    .into_iter()
    .collect();
    let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.token.as_deref(), Some("per_account"));
}

#[test]
fn token_command_resolves_from_defaults_account_source_and_env() {
    let toml = r#"
        [defaults]
        token_command = "defaults"

        [accounts.alice]
        token_command = "account"

        [accounts.alice.sources.liked]
        token_command = "source"
    "#;
    let cfg = Config::from_toml(toml).unwrap();

    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(eff.token_command.as_deref(), Some("account"));

    let eff = cfg
        .resolve("alice", Some("liked"), &no_env(), &no_flags())
        .unwrap();
    assert_eq!(eff.token_command.as_deref(), Some("source"));

    let env: HashMap<String, String> = [("SUNO_TOKEN_COMMAND".into(), "global".into())]
        .into_iter()
        .collect();
    let eff = cfg
        .resolve("alice", Some("liked"), &env, &no_flags())
        .unwrap();
    assert_eq!(eff.token_command.as_deref(), Some("global"));

    let env: HashMap<String, String> = [
        ("SUNO_TOKEN_COMMAND".into(), "global".into()),
        ("SUNO_ALICE_TOKEN_COMMAND".into(), "per_account".into()),
    ]
    .into_iter()
    .collect();
    let eff = cfg
        .resolve("alice", Some("liked"), &env, &no_flags())
        .unwrap();
    assert_eq!(eff.token_command.as_deref(), Some("per_account"));
}

#[test]
fn per_account_token_command_env_label_uppersnakedcase() {
    let cfg = Config::from_toml("[accounts.my-lib]\n").unwrap();
    let env: HashMap<String, String> = [("SUNO_MY_LIB_TOKEN_COMMAND".into(), "command".into())]
        .into_iter()
        .collect();
    let eff = cfg.resolve("my-lib", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.token_command.as_deref(), Some("command"));
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
fn animated_covers_defaults_off_and_follows_precedence() {
    // Compiled default is off.
    let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert!(!eff.animated_covers);

    // File default on; per-source off; env on; flag off — flag wins.
    let toml = r#"
        [defaults]
        animated_covers = true

        [accounts.alice.sources.liked]
        animated_covers = false
    "#;
    let cfg = Config::from_toml(toml).unwrap();

    // File default (defaults) turns it on for an unscoped resolve.
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert!(eff.animated_covers);

    // Per-source file setting overrides the file default.
    let eff = cfg
        .resolve("alice", Some("liked"), &no_env(), &no_flags())
        .unwrap();
    assert!(!eff.animated_covers);

    // Env overrides file (even the per-source off).
    let env: HashMap<String, String> = [("SUNO_ANIMATED_COVERS".into(), "true".into())]
        .into_iter()
        .collect();
    let eff = cfg
        .resolve("alice", Some("liked"), &env, &no_flags())
        .unwrap();
    assert!(eff.animated_covers);

    // Flag overrides env.
    let flags = FlagOverrides {
        settings: Settings {
            animated_covers: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    let eff = cfg.resolve("alice", Some("liked"), &env, &flags).unwrap();
    assert!(!eff.animated_covers);
}

#[test]
fn video_mp4_defaults_off_and_follows_precedence() {
    // Compiled default is off.
    let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert!(!eff.video_mp4);

    // File default on; per-source off; env on; flag off — flag wins.
    let toml = r#"
        [defaults]
        video_mp4 = true

        [accounts.alice.sources.liked]
        video_mp4 = false
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    assert!(
        cfg.resolve("alice", None, &no_env(), &no_flags())
            .unwrap()
            .video_mp4
    );
    assert!(
        !cfg.resolve("alice", Some("liked"), &no_env(), &no_flags())
            .unwrap()
            .video_mp4
    );

    let env: HashMap<String, String> = [("SUNO_VIDEO_MP4".into(), "true".into())]
        .into_iter()
        .collect();
    assert!(
        cfg.resolve("alice", Some("liked"), &env, &no_flags())
            .unwrap()
            .video_mp4
    );

    let flags = FlagOverrides {
        settings: Settings {
            video_mp4: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(
        !cfg.resolve("alice", Some("liked"), &env, &flags)
            .unwrap()
            .video_mp4
    );
}

#[test]
fn download_stems_defaults_off_and_follows_precedence() {
    // Compiled default is off (bulk stem mirroring never spends, but it is
    // opt-in so it never runs unless asked).
    let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
    assert!(
        !cfg.resolve("alice", None, &no_env(), &no_flags())
            .unwrap()
            .download_stems
    );

    // File default on; per-source off; env on; flag off — flag wins.
    let toml = r#"
        [defaults]
        download_stems = true

        [accounts.alice.sources.liked]
        download_stems = false
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    assert!(
        cfg.resolve("alice", None, &no_env(), &no_flags())
            .unwrap()
            .download_stems
    );
    assert!(
        !cfg.resolve("alice", Some("liked"), &no_env(), &no_flags())
            .unwrap()
            .download_stems
    );

    let env: HashMap<String, String> = [("SUNO_DOWNLOAD_STEMS".into(), "true".into())]
        .into_iter()
        .collect();
    assert!(
        cfg.resolve("alice", Some("liked"), &env, &no_flags())
            .unwrap()
            .download_stems
    );

    let flags = FlagOverrides {
        settings: Settings {
            download_stems: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(
        !cfg.resolve("alice", Some("liked"), &env, &flags)
            .unwrap()
            .download_stems
    );
}

#[test]
fn stem_format_defaults_to_wav_and_follows_precedence() {
    // Compiled default is WAV (lossless, the safe default for stems).
    let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
    assert_eq!(
        cfg.resolve("alice", None, &no_env(), &no_flags())
            .unwrap()
            .stem_format,
        StemFormat::Wav
    );

    // File default mp3; per-source wav; env mp3; flag wav — flag wins.
    let toml = r#"
        [defaults]
        stem_format = "mp3"

        [accounts.alice.sources.liked]
        stem_format = "wav"
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    assert_eq!(
        cfg.resolve("alice", None, &no_env(), &no_flags())
            .unwrap()
            .stem_format,
        StemFormat::Mp3
    );
    assert_eq!(
        cfg.resolve("alice", Some("liked"), &no_env(), &no_flags())
            .unwrap()
            .stem_format,
        StemFormat::Wav
    );

    let env: HashMap<String, String> = [("SUNO_STEM_FORMAT".into(), "mp3".into())]
        .into_iter()
        .collect();
    assert_eq!(
        cfg.resolve("alice", Some("liked"), &env, &no_flags())
            .unwrap()
            .stem_format,
        StemFormat::Mp3
    );

    let flags = FlagOverrides {
        settings: Settings {
            stem_format: Some(StemFormat::Wav),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        cfg.resolve("alice", Some("liked"), &env, &flags)
            .unwrap()
            .stem_format,
        StemFormat::Wav
    );
}

#[test]
fn stem_format_rejects_flac_and_unknown() {
    // FLAC is deliberately unrepresentable for stems: parsing it is an error,
    // so a config or flag can never ask for a FLAC stem.
    assert!("flac".parse::<StemFormat>().is_err());
    assert!("aac".parse::<StemFormat>().is_err());
    assert_eq!("WAV".parse::<StemFormat>().unwrap(), StemFormat::Wav);
    assert_eq!("Mp3".parse::<StemFormat>().unwrap(), StemFormat::Mp3);
    // A FLAC stem_format in config is a config error, not a silent fallback.
    assert!(Config::from_toml("[defaults]\nstem_format = \"flac\"\n").is_err());
}

#[test]
fn video_cover_retention_drives_cover_artifacts_not_the_music_video() {
    let resolve = |retention: &str| {
        let toml = format!("[accounts.alice]\nvideo_cover_retention = \"{retention}\"\n");
        Config::from_toml(&toml)
            .unwrap()
            .resolve("alice", None, &no_env(), &no_flags())
            .unwrap()
    };

    let neither = resolve("neither");
    assert!(!neither.animated_covers && !neither.raw_animated_cover);
    assert_eq!(neither.video_cover_retention, VideoCoverRetention::Neither);

    let webp = resolve("webp");
    assert!(webp.animated_covers && !webp.raw_animated_cover);
    assert_eq!(webp.video_cover_retention, VideoCoverRetention::Webp);

    // `mp4` keeps the raw album cover (`video_cover_url` verbatim); it does
    // NOT switch on the standalone music-video toggle (`video_url`).
    let mp4 = resolve("mp4");
    assert!(!mp4.animated_covers && mp4.raw_animated_cover);
    assert!(!mp4.video_mp4);
    assert_eq!(mp4.video_cover_retention, VideoCoverRetention::Mp4);

    let both = resolve("both");
    assert!(both.animated_covers && both.raw_animated_cover);
    assert!(!both.video_mp4);
    assert_eq!(both.video_cover_retention, VideoCoverRetention::Both);
}

#[test]
fn video_mp4_is_independent_of_cover_retention() {
    // The standalone music video (`video_url`) has its own toggle and is
    // never implied by a `video_cover_retention` mode, nor vice versa.
    let toml = "[accounts.alice]\nvideo_mp4 = true\nvideo_cover_retention = \"webp\"\n";
    let cfg = Config::from_toml(toml).unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert!(eff.video_mp4);
    assert!(eff.animated_covers);
    assert!(!eff.raw_animated_cover);
    assert_eq!(eff.video_cover_retention, VideoCoverRetention::Webp);
}

#[test]
fn animated_cover_webp_knobs_follow_precedence_and_validate_ranges() {
    let toml = r#"
        [defaults]
        animated_cover_quality = 80
        animated_cover_max_fps = 20
        animated_cover_max_width = 640
        animated_cover_compression_level = 3

        [accounts.alice.sources.liked]
        animated_cover_quality = 75
    "#;
    let cfg = Config::from_toml(toml).unwrap();
    let eff = cfg
        .resolve("alice", Some("liked"), &no_env(), &no_flags())
        .unwrap();
    assert_eq!(eff.animated_cover_webp.quality, 75);
    assert_eq!(eff.animated_cover_webp.max_fps, 20);
    assert_eq!(eff.animated_cover_webp.max_width, Some(640));
    assert_eq!(eff.animated_cover_webp.compression_level, 3);

    let env: HashMap<String, String> = [("SUNO_ANIMATED_COVER_QUALITY".into(), "90".into())]
        .into_iter()
        .collect();
    let eff = cfg
        .resolve("alice", Some("liked"), &env, &no_flags())
        .unwrap();
    assert_eq!(eff.animated_cover_webp.quality, 90);

    let flags = FlagOverrides {
        settings: Settings {
            animated_cover_quality: Some(95),
            animated_cover_max_width: Some(512),
            animated_cover_compression_level: Some(4),
            ..Default::default()
        },
        ..Default::default()
    };
    let eff = cfg.resolve("alice", Some("liked"), &env, &flags).unwrap();
    assert_eq!(eff.animated_cover_webp.quality, 95);
    assert_eq!(eff.animated_cover_webp.max_width, Some(512));
    assert_eq!(eff.animated_cover_webp.compression_level, 4);

    let bad_env: HashMap<String, String> = [("SUNO_ANIMATED_COVER_QUALITY".into(), "101".into())]
        .into_iter()
        .collect();
    assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());
}

#[test]
fn video_cover_retention_parses_formats_and_reports_kept_artifacts() {
    // FromStr is case-insensitive across every variant.
    assert_eq!(
        "NEITHER".parse::<VideoCoverRetention>().unwrap(),
        VideoCoverRetention::Neither
    );
    assert_eq!(
        "WebP".parse::<VideoCoverRetention>().unwrap(),
        VideoCoverRetention::Webp
    );
    assert_eq!(
        "mp4".parse::<VideoCoverRetention>().unwrap(),
        VideoCoverRetention::Mp4
    );
    assert_eq!(
        "Both".parse::<VideoCoverRetention>().unwrap(),
        VideoCoverRetention::Both
    );
    // An unknown mode is a config error, not a silent fallback.
    assert!("mkv".parse::<VideoCoverRetention>().is_err());

    // Display round-trips back to a token FromStr accepts.
    for mode in [
        VideoCoverRetention::Neither,
        VideoCoverRetention::Webp,
        VideoCoverRetention::Mp4,
        VideoCoverRetention::Both,
    ] {
        assert_eq!(
            mode.to_string().parse::<VideoCoverRetention>().unwrap(),
            mode
        );
    }

    // keeps_webp / keeps_mp4 truth table.
    assert!(!VideoCoverRetention::Neither.keeps_webp());
    assert!(!VideoCoverRetention::Neither.keeps_mp4());
    assert!(VideoCoverRetention::Webp.keeps_webp());
    assert!(!VideoCoverRetention::Webp.keeps_mp4());
    assert!(!VideoCoverRetention::Mp4.keeps_webp());
    assert!(VideoCoverRetention::Mp4.keeps_mp4());
    assert!(VideoCoverRetention::Both.keeps_webp());
    assert!(VideoCoverRetention::Both.keeps_mp4());
}

#[test]
fn video_cover_retention_resolves_from_env_and_rejects_unknown() {
    let cfg = Config::from_toml("[accounts.alice]\n").unwrap();

    // A valid env value overrides the legacy toggles, just like a flag.
    let env: HashMap<String, String> = [("SUNO_VIDEO_COVER_RETENTION".into(), "both".into())]
        .into_iter()
        .collect();
    let eff = cfg.resolve("alice", None, &env, &no_flags()).unwrap();
    assert_eq!(eff.video_cover_retention, VideoCoverRetention::Both);
    assert!(eff.animated_covers);
    assert!(eff.raw_animated_cover);

    // An unknown env value is a config error rather than a silent default.
    let bad_env: HashMap<String, String> = [("SUNO_VIDEO_COVER_RETENTION".into(), "mkv".into())]
        .into_iter()
        .collect();
    assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());
}

#[test]
fn animated_cover_compression_level_enforces_zero_to_four() {
    // The top of the valid range is accepted from the config file. Effort is
    // capped at 4 because level 6 costs many times the time for no size gain.
    let cfg =
        Config::from_toml("[defaults]\nanimated_cover_compression_level = 4\n[accounts.alice]\n")
            .unwrap();
    assert_eq!(
        cfg.resolve("alice", None, &no_env(), &no_flags())
            .unwrap()
            .animated_cover_webp
            .compression_level,
        4
    );

    // One past the top is rejected.
    let cfg =
        Config::from_toml("[defaults]\nanimated_cover_compression_level = 5\n[accounts.alice]\n")
            .unwrap();
    assert!(cfg.resolve("alice", None, &no_env(), &no_flags()).is_err());

    // The same ceiling is enforced for an env override.
    let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
    let bad_env: HashMap<String, String> =
        [("SUNO_ANIMATED_COVER_COMPRESSION_LEVEL".into(), "5".into())]
            .into_iter()
            .collect();
    assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());

    // A non-integer env value is a config error, not a panic.
    let junk_env: HashMap<String, String> = [("SUNO_ANIMATED_COVER_MAX_FPS".into(), "abc".into())]
        .into_iter()
        .collect();
    assert!(cfg.resolve("alice", None, &junk_env, &no_flags()).is_err());
}

#[test]
fn animated_cover_lossless_defaults_off_and_follows_precedence() {
    // The compiled default is a bounded lossy encode that fits the FLAC
    // picture cap: quality 90, effort 4, lossy (not lossless).
    let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert_eq!(eff.animated_cover_webp.quality, 90);
    assert_eq!(eff.animated_cover_webp.compression_level, 4);
    assert!(!eff.animated_cover_webp.lossless);

    // Config opts in; an env value and a flag each override in turn.
    let cfg = Config::from_toml("[defaults]\nanimated_cover_lossless = true\n[accounts.alice]\n")
        .unwrap();
    assert!(
        cfg.resolve("alice", None, &no_env(), &no_flags())
            .unwrap()
            .animated_cover_webp
            .lossless
    );
    let env: HashMap<String, String> = [("SUNO_ANIMATED_COVER_LOSSLESS".into(), "false".into())]
        .into_iter()
        .collect();
    assert!(
        !cfg.resolve("alice", None, &env, &no_flags())
            .unwrap()
            .animated_cover_webp
            .lossless
    );
    let flags = FlagOverrides {
        settings: Settings {
            animated_cover_lossless: Some(true),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(
        cfg.resolve("alice", None, &env, &flags)
            .unwrap()
            .animated_cover_webp
            .lossless
    );

    // A non-boolean value is a config error, not a silent default.
    let bad_env: HashMap<String, String> =
        [("SUNO_ANIMATED_COVER_LOSSLESS".into(), "maybe".into())]
            .into_iter()
            .collect();
    assert!(cfg.resolve("alice", None, &bad_env, &no_flags()).is_err());
}

#[test]
fn animated_cover_max_width_defaults_to_bounded() {
    // With nothing configured, the width cap is the bounded default (640 px)
    // so the embedded animated cover reliably fits the FLAC picture cap.
    let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
    assert_eq!(
        cfg.resolve("alice", None, &no_env(), &no_flags())
            .unwrap()
            .animated_cover_webp
            .max_width,
        Some(640)
    );
}

#[test]
fn text_sidecars_default_off_and_follow_precedence() {
    // Both compiled defaults are off.
    let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert!(!eff.details_sidecar);
    assert!(!eff.lyrics_sidecar);

    let toml = r#"
        [defaults]
        details_sidecar = true

        [accounts.alice.sources.liked]
        details_sidecar = false
    "#;
    let cfg = Config::from_toml(toml).unwrap();

    // File default turns details on for an unscoped resolve; lyrics stays off.
    let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
    assert!(eff.details_sidecar);
    assert!(!eff.lyrics_sidecar);

    // Per-source file setting overrides the file default.
    let eff = cfg
        .resolve("alice", Some("liked"), &no_env(), &no_flags())
        .unwrap();
    assert!(!eff.details_sidecar);

    // Env overrides file (both flags), and the flag overrides env.
    let env: HashMap<String, String> = [
        ("SUNO_DETAILS_SIDECAR".into(), "true".into()),
        ("SUNO_LYRICS_SIDECAR".into(), "true".into()),
    ]
    .into_iter()
    .collect();
    let eff = cfg
        .resolve("alice", Some("liked"), &env, &no_flags())
        .unwrap();
    assert!(eff.details_sidecar);
    assert!(eff.lyrics_sidecar);

    let flags = FlagOverrides {
        settings: Settings {
            lyrics_sidecar: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    let eff = cfg.resolve("alice", Some("liked"), &env, &flags).unwrap();
    assert!(eff.details_sidecar);
    assert!(!eff.lyrics_sidecar);
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
