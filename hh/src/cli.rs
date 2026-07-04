//! CLI definition (clap derive) for `hh` (FR-6.2).
//!
//! Every subcommand carries a one-line usage example in its `after_help`, so
//! `hh <sub> --help` shows an example (FR-6.2). Subcommand bodies are not
//! implemented in this skeleton — the dispatch in [`crate::main`] returns a
//! structured "not implemented" error for each, except `--version` and
//! `--help`, which clap handles directly.

use clap::{Args, Parser, Subcommand};

/// The full version string, with git sha, produced by `build.rs` (NFR-8).
use crate::HH_VERSION;

/// Halfhand — local-first CLI flight recorder for AI agents.
///
/// Records agent execution (prompts, tool calls, MCP traffic, file changes,
/// terminal output) to a local SQLite database and replays it faithfully.
/// Halfhand never makes network calls (SRS NFR-2).
#[derive(Parser, Debug)]
#[command(
    name = "hh",
    version = HH_VERSION,
    about = "Local-first CLI flight recorder for AI agents",
    long_about = "Halfhand records AI agent sessions to a local SQLite database and\n\
                  replays them faithfully. All data stays on your machine;\n\
                  Halfhand makes zero network calls (SRS NFR-2).\n\
                  \n\
                  Recorded sessions can contain secrets present in prompts and\n\
                  outputs — use `hh delete` to remove them.",
    after_help = "Examples:\n  hh run -- claude              record a Claude Code session\n  hh replay last                 replay the most recent session\n  hh inspect a1b2c3 --json       print session detail as JSON\n  hh list                        list recorded sessions"
)]
pub struct Cli {
    /// Subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands of `hh`.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Record an agent session inside a PTY (SRS FR-1).
    #[command(
        after_help = "Example:\n  hh run -- claude\n  hh run --record-input -- python3 my_agent.py"
    )]
    Run(RunArgs),

    /// Faithfully play back a recorded session in an interactive TUI (SRS FR-3).
    #[command(after_help = "Example:\n  hh replay last\n  hh replay a1b2c3")]
    Replay(ReplayArgs),

    /// Print non-interactive detail about a session (SRS FR-4).
    #[command(
        after_help = "Example:\n  hh inspect last\n  hh inspect a1b2c3 --step 7\n  hh inspect last --json | jq\n  hh inspect last --diff"
    )]
    Inspect(InspectArgs),

    /// List recorded sessions, newest first (SRS FR-5).
    #[command(after_help = "Example:\n  hh list\n  hh list --limit 50\n  hh list --json")]
    List(ListArgs),

    /// Delete a session and garbage-collect its blobs (SRS FR-6.1).
    #[command(after_help = "Example:\n  hh delete a1b2c3\n  hh delete last --yes")]
    Delete(DeleteArgs),

    /// Run an MCP server behind a recording stdio proxy (SRS FR-2).
    #[command(
        name = "mcp-proxy",
        after_help = "Example:\n  hh mcp-proxy -- uvx my-mcp-server\n  hh mcp-proxy --session-hint api-server -- node server.js"
    )]
    McpProxy(McpProxyArgs),
}

/// Arguments for `hh run`.
#[derive(Args, Debug)]
pub struct RunArgs {
    /// Record user keystrokes (off by default; keystrokes may contain secrets — SRS NFR-4).
    #[arg(long)]
    pub record_input: bool,

    /// Force an adapter rather than auto-detecting (`claude-code`).
    #[arg(long)]
    pub adapter: Option<String>,

    /// The command to run and its arguments, after `--`.
    #[arg(last = true, num_args = 1.., required = true)]
    pub command: Vec<String>,
}

/// Arguments for `hh replay`.
#[derive(Args, Debug)]
pub struct ReplayArgs {
    /// Session short id, full id, or `last` (most recent). Defaults to `last`.
    pub session: Option<String>,
}

/// Arguments for `hh inspect`.
#[derive(Args, Debug)]
pub struct InspectArgs {
    /// Session short id, full id, or `last`.
    pub session: Option<String>,

    /// Print the full detail of step N.
    #[arg(long)]
    pub step: Option<u32>,

    /// Emit stable, documented JSON (single object with --step, NDJSON stream otherwise).
    #[arg(long)]
    pub json: bool,

    /// Print the concatenated unified diff of file changes.
    #[arg(long)]
    pub diff: bool,

    /// Jump to the first error/failed step.
    #[arg(long)]
    pub failed: bool,
}

/// Arguments for `hh list`.
#[derive(Args, Debug)]
pub struct ListArgs {
    /// Maximum number of sessions to print (default 20).
    #[arg(long, default_value_t = 20)]
    pub limit: u32,

    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `hh delete`.
#[derive(Args, Debug)]
pub struct DeleteArgs {
    /// Session short id, full id, or `last`.
    pub session: String,

    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `hh mcp-proxy`.
#[derive(Args, Debug)]
pub struct McpProxyArgs {
    /// A human-readable hint stored as the session name for a standalone mcp-only session.
    #[arg(long)]
    pub session_hint: Option<String>,

    /// The MCP server command and its arguments, after `--`.
    #[arg(last = true, num_args = 1.., required = true)]
    pub command: Vec<String>,
}
