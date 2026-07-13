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
    #[command(
        after_help = "Example:\n  hh replay last\n  hh replay a1b2c3\n  hh replay last --web"
    )]
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

    /// Diagnose the recording stack and report pass/fail per check.
    ///
    /// `hh doctor` is a read-only health probe: it does not mutate the database
    /// or write to the data dir beyond a self-cleaning watcher probe. Each check
    /// prints a `✓`/`✗` line and the command exits nonzero if any check fails,
    /// so it is safe to run from CI or before a session you suspect is broken.
    #[command(after_help = "Example:\n  hh doctor\n  hh doctor --json | jq")]
    Doctor(DoctorArgs),

    /// Reclaim disk space: prune orphaned blobs and vacuum the database (Area 3).
    #[command(after_help = "Example:\n  hh gc\n  hh gc --json\n  hh gc --no-vacuum")]
    Gc(GcArgs),

    /// Summarize the store: sessions, events, disk usage, largest sessions (Area 3).
    #[command(after_help = "Example:\n  hh stats\n  hh stats --json\n  hh stats --top 10")]
    Stats(StatsArgs),

    /// Scan recorded sessions for secrets (docs/redaction.md).
    ///
    /// Reports what was detected (type, step, location, correlation tag) —
    /// never the secret itself. Exits 4 when findings exist, so CI can gate
    /// on a clean scan; 0 when clean.
    #[command(after_help = "Example:\n  hh scan last\n  hh scan a1b2c3 --json\n  hh scan --all")]
    Scan(ScanArgs),

    /// Irreversibly redact secrets from a recorded session in place.
    ///
    /// Rewrites events and affected blobs, replacing each secret with
    /// {{REDACTED:<type>:<hash8>}}; originals are securely deleted and the
    /// database is compacted so no plaintext copy survives. See
    /// docs/redaction.md for the guarantees and their limits.
    #[command(after_help = "Example:\n  hh redact a1b2c3\n  hh redact last --yes")]
    Redact(RedactArgs),

    /// Export a session as a JSON bundle, a portable `.hh` archive, or a
    /// self-contained HTML page.
    ///
    /// Exports are ALWAYS redacted by default — nothing leaves the machine
    /// unredacted by accident. Opting out (--no-redact) requires an
    /// interactive confirmation.
    #[command(
        after_help = "Example:\n  hh export last --out session.json\n  hh export last --bundle -o session.hh\n  hh export a1b2c3 --html --out session.html\n  hh export last | jq .session"
    )]
    Export(ExportArgs),

    /// Import a portable session bundle produced by `hh export --bundle`.
    ///
    /// Validates the bundle's manifest and blob hashes, then imports it
    /// under a brand-new local session id — the original id is preserved in
    /// the new session's `imported_from` field. Refuses a corrupt or
    /// tampered bundle with a precise error.
    #[command(after_help = "Example:\n  hh import session.hh")]
    Import(ImportArgs),

    /// Search recorded sessions for text matches (FTS5 full-text search).
    ///
    /// Searches event summaries and message bodies across all sessions.
    /// Results show the session, step, event kind, and a highlighted snippet.
    /// Supports FTS5 query syntax: words, phrases in quotes, prefix with `*`,
    /// AND/OR/NOT operators.
    #[command(
        after_help = "Example:\n  hh search \"error\"\n  hh search \"create file\" --kind tool_call\n  hh search \"api key\" --agent claude-code --json"
    )]
    Search(SearchArgs),
}

/// Arguments for `hh scan`.
#[derive(Args, Debug)]
pub struct ScanArgs {
    /// Session short id, full id, or `last`. Defaults to `last` (or use --all).
    pub session: Option<String>,

    /// Scan every recorded session.
    #[arg(long, conflicts_with = "session")]
    pub all: bool,

    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `hh redact`.
#[derive(Args, Debug)]
pub struct RedactArgs {
    /// Session short id, full id, or `last`.
    pub session: String,

    /// Skip the confirmation prompt (redaction is irreversible).
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `hh export`.
#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Session short id, full id, or `last`. Defaults to `last`.
    pub session: Option<String>,

    /// Write to this file instead of stdout.
    #[arg(long, short = 'o')]
    pub out: Option<std::path::PathBuf>,

    /// Export a self-contained HTML page instead of the JSON bundle.
    #[arg(long, conflicts_with = "bundle")]
    pub html: bool,

    /// Export a portable `.hh` archive (manifest + events + referenced
    /// blobs, zstd-compressed) instead of the plain JSON bundle. Import it
    /// elsewhere with `hh import`.
    #[arg(long, conflicts_with = "html")]
    pub bundle: bool,

    /// Skip redaction (requires interactive confirmation; refused when stdin
    /// is not a TTY, so a script can never exfiltrate raw sessions).
    #[arg(long)]
    pub no_redact: bool,
}

/// Arguments for `hh import`.
#[derive(Args, Debug)]
pub struct ImportArgs {
    /// Path to a bundle produced by `hh export --bundle`.
    pub file: std::path::PathBuf,
}

/// Arguments for `hh search`.
#[derive(Args, Debug)]
pub struct SearchArgs {
    /// The FTS5 search query (words, phrases, prefix with `*`, AND/OR/NOT).
    pub query: String,

    /// Filter by agent kind (claude-code, codex-cli, gemini-cli, generic, mcp-only).
    #[arg(long)]
    pub agent: Option<String>,

    /// Filter by event kind (user_message, agent_message, tool_call, etc.).
    #[arg(long)]
    pub kind: Option<String>,

    /// Only sessions started after this unix-ms timestamp.
    #[arg(long)]
    pub since: Option<i64>,

    /// Only sessions whose cwd contains this path fragment.
    #[arg(long)]
    pub path: Option<String>,

    /// Maximum results (default 50).
    #[arg(long, default_value_t = 50)]
    pub limit: u32,

    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `hh doctor`.
#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Emit machine-readable JSON (one object with a per-check array) instead
    /// of plain pass/fail lines.
    #[arg(long)]
    pub json: bool,
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

    /// Export a self-contained HTML replay page to a temp file and print its
    /// path, instead of opening the interactive TUI. Does not need a
    /// terminal, does not open a browser, and does not start a server.
    #[arg(long)]
    pub web: bool,
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

/// Arguments for `hh gc`.
#[derive(Args, Debug)]
pub struct GcArgs {
    /// Skip the (slow, disk-hungry) VACUUM; only prune orphaned blob files and
    /// stale rows. VACUUM rebuilds `hh.db` and needs temporary free space, so
    /// use this on a nearly-full disk.
    #[arg(long)]
    pub no_vacuum: bool,

    /// Emit machine-readable JSON instead of plain lines.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `hh stats`.
#[derive(Args, Debug)]
pub struct StatsArgs {
    /// Number of largest sessions (by event count) to list.
    #[arg(long, default_value_t = 5)]
    pub top: u32,

    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}
