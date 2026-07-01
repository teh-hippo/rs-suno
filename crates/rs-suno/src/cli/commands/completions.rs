//! `completions`: emit a shell completion script via `clap_complete`.

use clap::CommandFactory;

use crate::cli::args::{Cli, CompletionsArgs};
use crate::cli::desired::ExitCode;

/// Run `completions`, writing the script for the chosen shell to stdout.
pub fn run_completions(args: &CompletionsArgs) -> ExitCode {
    let mut command = Cli::command();
    let name = command.get_name().to_owned();
    clap_complete::generate(args.shell, &mut command, name, &mut std::io::stdout());
    ExitCode::Ok
}
