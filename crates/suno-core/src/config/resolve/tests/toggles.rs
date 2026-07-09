use super::*;

#[test]
fn bool_toggles_default_off_and_follow_precedence() {
    // animated_covers, video_mp4 and download_stems share one precedence
    // ladder: compiled default off; file default on; per-source off; env on;
    // flag off wins. Only the key, env var and accessor differ per row.
    struct Row {
        label: &'static str,
        key: &'static str,
        env_var: &'static str,
        get: fn(&EffectiveSettings) -> bool,
        flag_off: FlagOverrides,
    }
    let rows = [
        Row {
            label: "animated_covers",
            key: "animated_covers",
            env_var: "SUNO_ANIMATED_COVERS",
            get: |e| e.animated_covers,
            flag_off: FlagOverrides {
                settings: Settings {
                    animated_covers: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        Row {
            label: "video_mp4",
            key: "video_mp4",
            env_var: "SUNO_VIDEO_MP4",
            get: |e| e.video_mp4,
            flag_off: FlagOverrides {
                settings: Settings {
                    video_mp4: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        Row {
            label: "download_stems",
            key: "download_stems",
            env_var: "SUNO_DOWNLOAD_STEMS",
            get: |e| e.download_stems,
            flag_off: FlagOverrides {
                settings: Settings {
                    download_stems: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
    ];
    for Row {
        label,
        key,
        env_var,
        get,
        flag_off,
    } in rows
    {
        // Compiled default is off.
        let cfg = Config::from_toml("[accounts.alice]\n").unwrap();
        let eff = cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap();
        assert!(!get(&eff), "{label}: compiled default off");

        // File default on; per-source off.
        let toml =
            format!("[defaults]\n{key} = true\n\n[accounts.alice.sources.liked]\n{key} = false\n");
        let cfg = Config::from_toml(&toml).unwrap();
        assert!(
            get(&cfg.resolve("alice", None, &no_env(), &no_flags()).unwrap()),
            "{label}: file default on (unscoped)"
        );
        assert!(
            !get(&cfg
                .resolve("alice", Some("liked"), &no_env(), &no_flags())
                .unwrap()),
            "{label}: per-source off overrides file default"
        );

        // Env on overrides file (even the per-source off).
        let env: HashMap<String, String> = [(env_var.to_string(), "true".to_string())]
            .into_iter()
            .collect();
        assert!(
            get(&cfg
                .resolve("alice", Some("liked"), &env, &no_flags())
                .unwrap()),
            "{label}: env on overrides file"
        );

        // Flag off overrides env.
        assert!(
            !get(&cfg
                .resolve("alice", Some("liked"), &env, &flag_off)
                .unwrap()),
            "{label}: flag off overrides env"
        );
    }
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
    assert_eq!("wav".parse::<StemFormat>().unwrap(), StemFormat::Wav);
    assert_eq!("mp3".parse::<StemFormat>().unwrap(), StemFormat::Mp3);
    // Case-sensitive to match serde (TOML) and the JSON schema.
    assert!("WAV".parse::<StemFormat>().is_err());
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
    // FromStr is case-sensitive (lowercase only) to match serde and the schema.
    assert_eq!(
        "neither".parse::<VideoCoverRetention>().unwrap(),
        VideoCoverRetention::Neither
    );
    assert_eq!(
        "webp".parse::<VideoCoverRetention>().unwrap(),
        VideoCoverRetention::Webp
    );
    assert_eq!(
        "mp4".parse::<VideoCoverRetention>().unwrap(),
        VideoCoverRetention::Mp4
    );
    assert_eq!(
        "both".parse::<VideoCoverRetention>().unwrap(),
        VideoCoverRetention::Both
    );
    // An unknown mode is a config error, not a silent fallback.
    assert!("mkv".parse::<VideoCoverRetention>().is_err());
    // A non-lowercase spelling is rejected, consistent with the file/schema.
    assert!("WebP".parse::<VideoCoverRetention>().is_err());

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
