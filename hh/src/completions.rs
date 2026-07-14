//! `hh completions <shell>` — print a shell completion script (FR-6.2).
//!
//! Emits the completion script for the requested shell to stdout via
//! [`clap_complete`]. The output is plain text and pipe-safe (respects
//! non-TTY / `NO_COLOR` trivially — there are no colors in a completion
//! script).

use std::io::stdout;
use std::process::ExitCode;

use clap::CommandFactory;

use crate::cli::{Cli, CompletionsArgs};

/// Run `hh completions`. Always succeeds (`clap_complete::generate` writes to
/// stdout and reports no error itself), so it returns `ExitCode` directly and
/// the dispatch in [`crate::run`] wraps it in `Ok`.
pub fn completions_command(args: &CompletionsArgs) -> ExitCode {
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_owned();
    // Emit to stdout so the user can redirect (`hh completions bash > ...`).
    clap_complete::generate(args.shell, &mut cmd, bin_name, &mut stdout());
    ExitCode::SUCCESS
}
