//! The clap command surface: the top-level parser, global options, and every
//! subcommand's arguments.
//!
//! Values that feed the engine's precedence resolution (`token`, `format`,
//! `retries`, ...) deliberately carry no clap `env`: the per-account
//! environment tier (`SUNO_<LABEL>_TOKEN`) lives in
//! [`suno_core::Config::resolve`], so letting clap pre-read the global env here
//! would shadow it. Globals that the engine does not resolve (`--account`,
//! `--config`, `--dry-run`, `--yes`) keep their clap `env` for convenience.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use suno_core::{AudioFormat, CharacterSet};

/// A download-only tool for mirroring your Suno.ai library.
#[derive(Parser, Debug)]
#[command(name = "suno", version, about, long_about = None)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Command,
}

/// Options accepted by `suno` itself, valid before or after any subcommand.
#[derive(Args, Debug, Clone, Default)]
pub struct GlobalArgs {
    /// Run against one configured account.
    #[arg(long, global = true, env = "SUNO_ACCOUNT", value_name = "LABEL")]
    pub account: Option<String>,
    /// Run every configured account in isolation.
    #[arg(long, global = true, conflicts_with = "account")]
    pub all: bool,
    /// Path to the config file.
    #[arg(long, global = true, env = "SUNO_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Report changes without writing to disk.
    #[arg(short = 'n', long, global = true, env = "SUNO_DRY_RUN")]
    pub dry_run: bool,
    /// Increase verbosity (repeatable: -vv for debug).
    #[arg(short = 'v', long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Decrease verbosity (repeatable: -qq for errors only).
    #[arg(short = 'q', long, global = true, action = clap::ArgAction::Count)]
    pub quiet: u8,
    /// Skip confirmation prompts (e.g. destructive sync).
    #[arg(short = 'y', long, global = true, env = "SUNO_YES")]
    pub yes: bool,
    /// Suno `__client` token. Never printed. Overrides config and env.
    #[arg(long, global = true, hide_env_values = true, value_name = "TOKEN")]
    pub token: Option<String>,
}

impl GlobalArgs {
    /// The net verbosity level: `-v` adds, `-q` subtracts, default 0.
    pub fn verbosity(&self) -> i8 {
        i8::try_from(self.verbose).unwrap_or(i8::MAX) - i8::try_from(self.quiet).unwrap_or(i8::MAX)
    }
}

/// The subcommand to run.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Mirror a source: download, update, and remove local files.
    Sync(SyncArgs),
    /// Download and update, never delete.
    Copy(SyncArgs),
    /// Report what sync or copy would change without touching disk.
    Check(CheckArgs),
    /// List clips in your Suno library.
    Ls(LsArgs),
    /// List clips as newline-delimited JSON.
    Lsjson(LsArgs),
    /// Download a specific clip by ID or URL.
    Fetch(FetchArgs),
    /// Manage the configuration file.
    Config(ConfigArgs),
    /// Manage authentication.
    Auth(AuthArgs),
    /// Print version and environment information.
    Version,
    /// Emit a shell completion script.
    Completions(CompletionsArgs),
}

/// Audio format for downloaded clips, mapped onto [`AudioFormat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum AudioFmt {
    Mp3,
    Flac,
    Wav,
}

impl From<AudioFmt> for AudioFormat {
    fn from(value: AudioFmt) -> Self {
        match value {
            AudioFmt::Mp3 => AudioFormat::Mp3,
            AudioFmt::Flac => AudioFormat::Flac,
            AudioFmt::Wav => AudioFormat::Wav,
        }
    }
}

/// Character set for filename sanitisation, mapped onto [`CharacterSet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Charset {
    Unicode,
    Ascii,
}

impl From<Charset> for CharacterSet {
    fn from(value: Charset) -> Self {
        match value {
            Charset::Unicode => CharacterSet::Unicode,
            Charset::Ascii => CharacterSet::Ascii,
        }
    }
}

/// Output format for `ls`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

/// The per-run area mode selected by `--mode`, mapped onto [`SourceMode`].
///
/// `mirror` arms deletion for the selected areas; `copy` keeps them additive.
/// Absent, scoped runs default to copy and a plain library run keeps the verb's
/// mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ModeArg {
    Mirror,
    Copy,
}

impl From<ModeArg> for suno_core::SourceMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Mirror => suno_core::SourceMode::Mirror,
            ModeArg::Copy => suno_core::SourceMode::Copy,
        }
    }
}

/// Flags shared by `sync` and `copy`.
#[derive(Args, Debug, Clone, Default)]
pub struct SyncArgs {
    /// Local directory to mirror into (defaults to the account's configured root).
    #[arg(value_name = "DEST")]
    pub dest: Option<PathBuf>,
    /// Audio format: mp3, flac, wav.
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub format: Option<AudioFmt>,
    /// Mirror only the N most recent clips.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// Mirror clips newer than a relative time (e.g. 7d, 2w, last-run).
    #[arg(long, value_name = "SPEC")]
    pub since: Option<String>,
    /// Minimum newest clips kept when a recency filter applies.
    #[arg(long, value_name = "N")]
    pub min_newest: Option<u32>,
    /// Download retry attempts per clip.
    #[arg(long, value_name = "N")]
    pub retries: Option<u32>,
    /// Simultaneous downloads (default 4).
    #[arg(long, value_name = "N")]
    pub concurrency: Option<u32>,
    /// Also write an animated cover.webp from each clip's video preview.
    #[arg(long)]
    pub animated_covers: bool,
    /// Re-pin this library to the authenticated account (use only when you
    /// deliberately point it at a different Suno account).
    #[arg(long)]
    pub allow_account_change: bool,
    /// Also write a plain-text `.details.txt` sidecar next to each song.
    #[arg(long)]
    pub details_sidecar: bool,
    /// Also write a plain-text `.lyrics.txt` sidecar next to each song.
    #[arg(long)]
    pub lyrics_sidecar: bool,
    /// Select the mode for scoped areas: `mirror` arms deletion, `copy` stays
    /// additive. Only meaningful with `--liked`/`--playlist` or an `[areas]`
    /// config; without it a scoped run stays in copy (non-deleting) mode.
    #[arg(long, value_enum, value_name = "MODE")]
    pub mode: Option<ModeArg>,
    /// Also select your liked songs. In copy (the scoped default) it never
    /// deletes; pass `--mode mirror` to arm deletion of liked-exclusive files.
    /// When a plain library `sync` is not also running, only the selected areas'
    /// `.m3u8` playlists are maintained.
    #[arg(long)]
    pub liked: bool,
    /// Also select a playlist, by id or name (repeatable). Resolves against your
    /// own non-trashed playlists. In copy (the scoped default) it never deletes;
    /// pass `--mode mirror` to arm deletion of that playlist's exclusive files.
    /// A mirror playlist still runs the full library as a copy protector, so
    /// library-exclusive files are never deleted.
    #[arg(long, value_name = "ID_OR_NAME")]
    pub playlist: Vec<String>,
    /// Also write an untimed `.lrc` sidecar next to each song (plain lyrics, no
    /// per-line timestamps).
    #[arg(long)]
    pub lrc_sidecar: bool,
    /// Also download the standalone `.mp4` music video next to each song, when
    /// Suno provides one.
    #[arg(long)]
    pub video_mp4: bool,
    /// Relative path template for naming downloaded files.
    /// Placeholders: {creator}, {handle}, {album}, {title}, {id}, {id8}, {root_id8}.
    #[arg(long, value_name = "TEMPLATE")]
    pub naming_template: Option<String>,
    /// Character set for filename sanitisation: unicode or ascii.
    #[arg(long, value_enum, value_name = "SET")]
    pub character_set: Option<Charset>,
}

/// `check` accepts every `sync` flag plus `--exit-code`.
#[derive(Args, Debug, Clone, Default)]
pub struct CheckArgs {
    #[command(flatten)]
    pub sync: SyncArgs,
    /// Exit 1 when changes are pending, 0 when up to date (for CI).
    #[arg(long)]
    pub exit_code: bool,
}

/// Flags shared by `ls` and `lsjson`.
#[derive(Args, Debug, Clone, Default)]
pub struct LsArgs {
    /// List only liked clips.
    #[arg(long)]
    pub liked: bool,
    /// Stop after the first N clips.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// Show clips newer than a relative time (e.g. 7d, 2w, last-run).
    #[arg(long, value_name = "SPEC")]
    pub since: Option<String>,
    /// Output format: text or json.
    #[arg(long, value_enum, value_name = "FORMAT", default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

/// `fetch` arguments.
#[derive(Args, Debug, Clone)]
pub struct FetchArgs {
    /// The clip ID or a Suno URL containing it.
    #[arg(value_name = "ID_OR_URL")]
    pub id: String,
    /// Destination directory or file (defaults to the current directory).
    #[arg(value_name = "DEST")]
    pub dest: Option<PathBuf>,
    /// Audio format: mp3, flac, wav.
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub format: Option<AudioFmt>,
    /// Explicit output file path, overriding DEST and auto-naming.
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

/// `config` and its subcommands.
#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Interactively create a new config file.
    Init,
    /// Add a new account entry to an existing config file.
    AddAccount(ConfigAddAccountArgs),
    /// Print the current config with tokens redacted.
    Show,
}

#[derive(Args, Debug)]
pub struct ConfigAddAccountArgs {
    /// The account label to add.
    #[arg(value_name = "LABEL")]
    pub label: Option<String>,
    /// Token for the new account (hidden in help).
    #[arg(long, value_name = "TOKEN", hide = true)]
    pub token: Option<String>,
}

/// `auth` and its subcommands.
#[derive(Args, Debug)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    /// Re-authenticate one account by re-minting its JWT.
    Refresh(AuthRefreshArgs),
}

#[derive(Args, Debug)]
pub struct AuthRefreshArgs {
    /// The account label (falls back to --account / --all).
    #[arg(value_name = "ACCOUNT")]
    pub account: Option<String>,
}

/// `completions` arguments.
#[derive(Args, Debug)]
pub struct CompletionsArgs {
    /// The shell to emit a completion script for.
    #[arg(value_name = "SHELL")]
    pub shell: clap_complete::Shell,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn verbosity_combines_counts() {
        let g = GlobalArgs {
            verbose: 2,
            quiet: 0,
            ..Default::default()
        };
        assert_eq!(g.verbosity(), 2);
        let g = GlobalArgs {
            verbose: 0,
            quiet: 2,
            ..Default::default()
        };
        assert_eq!(g.verbosity(), -2);
        let g = GlobalArgs {
            verbose: 1,
            quiet: 1,
            ..Default::default()
        };
        assert_eq!(g.verbosity(), 0);
    }

    #[test]
    fn account_and_all_conflict() {
        let result = Cli::try_parse_from(["suno", "--account", "a", "--all", "ls"]);
        assert!(result.is_err());
    }

    #[test]
    fn sync_parses_dest_and_flags() {
        let cli = Cli::try_parse_from([
            "suno",
            "sync",
            "/music",
            "--format",
            "mp3",
            "--limit",
            "5",
            "--min-newest",
            "0",
        ])
        .unwrap();
        match cli.command {
            Command::Sync(args) => {
                assert_eq!(args.dest.as_deref(), Some(std::path::Path::new("/music")));
                assert_eq!(args.format, Some(AudioFmt::Mp3));
                assert_eq!(args.limit, Some(5));
                assert_eq!(args.min_newest, Some(0));
                assert!(!args.animated_covers);
            }
            _ => panic!("expected sync"),
        }
    }

    #[test]
    fn sync_parses_animated_covers_flag() {
        // Present enables it; absent leaves it off (default false).
        let cli = Cli::try_parse_from(["suno", "sync", "/music", "--animated-covers"]).unwrap();
        match cli.command {
            Command::Sync(args) => assert!(args.animated_covers),
            _ => panic!("expected sync"),
        }
        let cli = Cli::try_parse_from(["suno", "copy", "/music"]).unwrap();
        match cli.command {
            Command::Copy(args) => assert!(!args.animated_covers),
            _ => panic!("expected copy"),
        }
    }

    #[test]
    fn sync_parses_allow_account_change_flag() {
        // Off by default; the flag opts into re-pinning the library owner.
        let cli = Cli::try_parse_from(["suno", "sync", "/music"]).unwrap();
        match cli.command {
            Command::Sync(args) => assert!(!args.allow_account_change),
            _ => panic!("expected sync"),
        }
        let cli =
            Cli::try_parse_from(["suno", "sync", "/music", "--allow-account-change"]).unwrap();
        match cli.command {
            Command::Sync(args) => assert!(args.allow_account_change),
            _ => panic!("expected sync"),
        }
    }

    #[test]
    fn sync_parses_text_sidecar_flags() {
        // Each present flag enables its sidecar; absent leaves both off.
        let cli = Cli::try_parse_from([
            "suno",
            "sync",
            "/music",
            "--details-sidecar",
            "--lyrics-sidecar",
            "--lrc-sidecar",
            "--video-mp4",
        ])
        .unwrap();
        match cli.command {
            Command::Sync(args) => {
                assert!(args.details_sidecar);
                assert!(args.lyrics_sidecar);
                assert!(args.lrc_sidecar);
                assert!(args.video_mp4);
            }
            _ => panic!("expected sync"),
        }
        let cli = Cli::try_parse_from(["suno", "sync", "/music"]).unwrap();
        match cli.command {
            Command::Sync(args) => {
                assert!(!args.details_sidecar);
                assert!(!args.lyrics_sidecar);
                assert!(!args.lrc_sidecar);
                assert!(!args.video_mp4);
            }
            _ => panic!("expected sync"),
        }
    }

    #[test]
    fn sync_parses_scope_flags() {
        // --liked and repeatable --playlist both land on the sync args; absent
        // leaves the scope empty (a full-account run).
        let cli = Cli::try_parse_from([
            "suno",
            "sync",
            "/music",
            "--liked",
            "--playlist",
            "Chill",
            "--playlist",
            "id-42",
        ])
        .unwrap();
        match cli.command {
            Command::Sync(args) => {
                assert!(args.liked);
                assert_eq!(args.playlist, vec!["Chill".to_owned(), "id-42".to_owned()]);
            }
            _ => panic!("expected sync"),
        }
        let cli = Cli::try_parse_from(["suno", "copy", "/music"]).unwrap();
        match cli.command {
            Command::Copy(args) => {
                assert!(!args.liked);
                assert!(args.playlist.is_empty());
            }
            _ => panic!("expected copy"),
        }
    }

    #[test]
    fn check_flattens_scope_flags() {
        let cli =
            Cli::try_parse_from(["suno", "check", "/music", "--liked", "--playlist", "Focus"])
                .unwrap();
        match cli.command {
            Command::Check(args) => {
                assert!(args.sync.liked);
                assert_eq!(args.sync.playlist, vec!["Focus".to_owned()]);
            }
            _ => panic!("expected check"),
        }
    }

    #[test]
    fn global_flags_accepted_after_subcommand() {
        let cli =
            Cli::try_parse_from(["suno", "sync", "/music", "--dry-run", "-vv", "--yes"]).unwrap();
        assert!(cli.global.dry_run);
        assert!(cli.global.yes);
        assert_eq!(cli.global.verbosity(), 2);
    }

    #[test]
    fn check_has_exit_code_flag() {
        let cli = Cli::try_parse_from(["suno", "check", "/music", "--exit-code"]).unwrap();
        match cli.command {
            Command::Check(args) => assert!(args.exit_code),
            _ => panic!("expected check"),
        }
    }

    #[test]
    fn lsjson_and_ls_share_flags() {
        let cli = Cli::try_parse_from(["suno", "lsjson", "--liked", "--limit", "3"]).unwrap();
        match cli.command {
            Command::Lsjson(args) => {
                assert!(args.liked);
                assert_eq!(args.limit, Some(3));
            }
            _ => panic!("expected lsjson"),
        }
    }

    #[test]
    fn completions_parses_shell() {
        let cli = Cli::try_parse_from(["suno", "completions", "bash"]).unwrap();
        assert!(matches!(cli.command, Command::Completions(_)));
    }

    #[test]
    fn audio_fmt_maps_to_core() {
        assert_eq!(AudioFormat::from(AudioFmt::Flac), AudioFormat::Flac);
        assert_eq!(AudioFormat::from(AudioFmt::Mp3), AudioFormat::Mp3);
        assert_eq!(AudioFormat::from(AudioFmt::Wav), AudioFormat::Wav);
    }

    #[test]
    fn charset_maps_to_core() {
        assert_eq!(CharacterSet::from(Charset::Unicode), CharacterSet::Unicode);
        assert_eq!(CharacterSet::from(Charset::Ascii), CharacterSet::Ascii);
    }

    #[test]
    fn sync_parses_naming_template_and_character_set() {
        let cli = Cli::try_parse_from([
            "suno",
            "sync",
            "/music",
            "--naming-template",
            "{title}/{id8}",
            "--character-set",
            "ascii",
        ])
        .unwrap();
        match cli.command {
            Command::Sync(args) => {
                assert_eq!(args.naming_template.as_deref(), Some("{title}/{id8}"));
                assert_eq!(args.character_set, Some(Charset::Ascii));
            }
            _ => panic!("expected sync"),
        }
        // Absent flags leave both as None.
        let cli = Cli::try_parse_from(["suno", "sync", "/music"]).unwrap();
        match cli.command {
            Command::Sync(args) => {
                assert_eq!(args.naming_template, None);
                assert_eq!(args.character_set, None);
            }
            _ => panic!("expected sync"),
        }
    }
}
