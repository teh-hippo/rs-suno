use super::*;

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
