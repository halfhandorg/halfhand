//! CLI definition (clap derive) for `hh` (FR-6.2).
//!
//! Every subcommand carries usage examples in its `after_help`, built by
//! [`examples`] so the description column lines up, and `hh <sub> --help`
//! always shows an example (FR-6.2).
//!
//! Requirement IDs (SRS FR-/NFR-/DR-, ENH-/BUG-, "Area n") are for
//! maintainers, not users: they live in plain `//` comments next to the item
//! they trace, never in a doc comment or attribute string clap renders. The
//! `rendered_help_has_no_requirement_ids` test enforces that. Subcommand dispatch lives in
//! the binary's `main.rs` (`crate::run`); `--version` and `--help` are handled
//! directly by clap.

use clap::{Args, Parser, Subcommand};

/// The full version string, with git sha, produced by `build.rs` (NFR-8).
use crate::HH_VERSION;

// SRS NFR-2: zero network calls.
/// Halfhand — local-first CLI flight recorder for AI agents.
///
/// Records agent execution (prompts, tool calls, MCP traffic, file changes,
/// terminal output) to a local SQLite database and replays it faithfully.
/// Halfhand never makes network calls.
#[derive(Parser, Debug)]
#[command(
    name = "hh",
    version = HH_VERSION,
    about = "Local-first CLI flight recorder for AI agents",
    long_about = "Halfhand records AI agent sessions to a local SQLite database and\n\
                  replays them faithfully. All data stays on your machine;\n\
                  Halfhand makes zero network calls.\n\
                  \n\
                  Recorded sessions can contain secrets present in prompts and\n\
                  outputs — use `hh delete` to remove them.",
    after_help = with_docs(
        &examples(&[
            ("hh run -- claude", "record a Claude Code session"),
            ("hh replay last", "replay the most recent session"),
            ("hh inspect a1b2c3 --json", "print session detail as JSON"),
            ("hh list", "list recorded sessions"),
        ]),
        "",
    )
)]
pub struct Cli {
    /// Subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands of `hh`.
#[derive(Subcommand, Debug)]
pub enum Command {
    // SRS FR-1.
    /// Record an agent session inside a PTY.
    #[command(after_help = examples(&[
        ("hh run -- claude", "record a Claude Code session"),
        ("hh run --record-input -- python3 my_agent.py", "also record your keystrokes"),
        ("hh run --name \"auth refactor\" -- claude", "record and name the session"),
        (
            "hh run --adapter claude-desktop -- $SHELL",
            "record the Claude Desktop session open in this dir; exit to finish",
        ),
    ]))]
    Run(RunArgs),

    // SRS FR-3.
    /// Faithfully play back a recorded session in an interactive TUI.
    #[command(after_help = examples(&[
        ("hh replay last", "replay the most recent session"),
        ("hh replay a1b2c3", "replay a session by short id"),
        ("hh replay last --web", "write a self-contained HTML replay instead"),
        ("hh replay last --web --open", "…and open it in your browser"),
    ]))]
    Replay(ReplayArgs),

    // SRS FR-4.
    /// Print non-interactive detail about a session.
    #[command(after_help = examples(&[
        ("hh inspect last", "summarize the most recent session"),
        ("hh inspect a1b2c3 --step 7", "show the full detail of step 7"),
        ("hh inspect last --json | jq", "stream the session as JSON"),
        ("hh inspect last --diff", "print the file changes as a unified diff"),
        ("hh inspect last --context", "model, provider, token usage, cost, durations"),
        ("hh inspect last --context --json", "the same panel as one JSON object"),
    ]))]
    Inspect(InspectArgs),

    // SRS FR-5.
    /// List recorded sessions, newest first.
    #[command(after_help = examples(&[
        ("hh list", "the 20 most recent sessions"),
        ("hh list --limit 50", "the 50 most recent sessions"),
        ("hh list --json", "machine-readable output"),
    ]))]
    List(ListArgs),

    // Migration 0007 (session context).
    /// Name or rename a recorded session.
    ///
    /// The name shows in `hh inspect` and in `--json` output. Names are
    /// trimmed, must not be empty, and are at most 100 characters.
    #[command(after_help = examples(&[
        ("hh rename last \"auth refactor\"", "name the most recent session"),
        ("hh rename a1b2c3 \"retry with opus\"", "rename a session by short id"),
        ("hh rename a1b2c3 --clear", "remove a session's name"),
    ]))]
    Rename(RenameArgs),

    // SRS FR-6.1.
    /// Delete a session and garbage-collect its blobs.
    #[command(after_help = examples(&[
        ("hh delete a1b2c3", "delete a session (asks first)"),
        ("hh delete last --yes", "delete the most recent session without asking"),
    ]))]
    Delete(DeleteArgs),

    // SRS FR-2. Advanced: listed last in `hh --help` (ADVANCED_DISPLAY_ORDER)
    // so the daily commands come first and `hh` does not read as an MCP tool.
    /// Advanced: run an MCP server behind a recording stdio proxy.
    ///
    /// Use it in an agent's MCP server configuration in place of the server
    /// command, to record the JSON-RPC traffic between the agent and that
    /// server.
    #[command(
        name = "mcp-proxy",
        display_order = ADVANCED_DISPLAY_ORDER,
        after_help = with_docs(
            &examples(&[
                ("hh mcp-proxy -- uvx my-mcp-server", "record a server's stdio traffic"),
                (
                    "hh mcp-proxy --session-hint api-server -- node server.js",
                    "same, with a session hint",
                ),
            ]),
            "mcp-proxy/",
        )
    )]
    McpProxy(McpProxyArgs),

    /// Diagnose the recording stack and report pass/fail per check.
    ///
    /// `hh doctor` is a read-only health probe: it does not mutate the database
    /// or write to the data dir beyond a self-cleaning watcher probe. Each check
    /// prints a `✓`/`✗` line and the command exits nonzero if any check fails,
    /// so it is safe to run from CI or before a session you suspect is broken.
    #[command(after_help = examples(&[
        ("hh doctor", "run every check"),
        ("hh doctor --json | jq", "machine-readable results"),
    ]))]
    Doctor(DoctorArgs),

    // Area 3 (storage maintenance).
    /// Reclaim disk space: prune orphaned blobs and vacuum the database.
    #[command(after_help = examples(&[
        ("hh gc", "prune orphaned blobs, then vacuum"),
        ("hh gc --json", "machine-readable report"),
        ("hh gc --no-vacuum", "prune only; skip the slow VACUUM"),
    ]))]
    Gc(GcArgs),

    // Area 3 (storage maintenance).
    /// Summarize the store: sessions, events, disk usage, largest sessions.
    #[command(after_help = examples(&[
        ("hh stats", "store summary"),
        ("hh stats --json", "machine-readable summary"),
        ("hh stats --top 10", "list the 10 largest sessions"),
    ]))]
    Stats(StatsArgs),

    // Design: docs/redaction.md, docs/redaction-design.md.
    /// Scan recorded sessions for secrets and report what was found.
    ///
    /// Reports what was detected (type, step, location, correlation tag) —
    /// never the secret itself. Exits 4 when findings exist, so CI can gate
    /// on a clean scan; 0 when clean.
    #[command(after_help = with_docs(
        &examples(&[
            ("hh scan last", "scan the most recent session"),
            ("hh scan a1b2c3 --json", "machine-readable findings"),
            ("hh scan --all", "scan every session (exits 4 on any finding)"),
        ]),
        "redaction/",
    ))]
    Scan(ScanArgs),

    /// Irreversibly redact secrets from a recorded session in place.
    ///
    /// Rewrites events and affected blobs, replacing each secret with
    /// `{{REDACTED:<type>:<hash8>}}`; originals are securely deleted and the
    /// database is compacted so no plaintext copy survives. The docs page
    /// below lists the guarantees and their limits.
    #[command(after_help = with_docs(
        &examples(&[
            ("hh redact a1b2c3", "redact a session (asks first)"),
            ("hh redact last --yes", "redact the most recent session without asking"),
        ]),
        "redaction/",
    ))]
    Redact(RedactArgs),

    /// Export a session as a JSON bundle, a portable `.hh` archive, or a
    /// self-contained HTML page.
    ///
    /// Exports are ALWAYS redacted by default — nothing leaves the machine
    /// unredacted by accident. Opting out (--no-redact) requires an
    /// interactive confirmation.
    #[command(after_help = examples(&[
        ("hh export last --out session.json", "redacted JSON bundle to a file"),
        ("hh export last --bundle -o session.hh", "portable archive for `hh import`"),
        ("hh export a1b2c3 --html --out session.html", "self-contained HTML replay page"),
        ("hh export last --html --open", "HTML page under the data dir, opened in your browser"),
        ("hh export last | jq .session", "JSON bundle to stdout"),
    ]))]
    Export(ExportArgs),

    /// Import a portable session bundle produced by `hh export --bundle`.
    ///
    /// Validates the bundle's manifest and blob hashes, then imports it
    /// under a brand-new local session id — the original id is preserved in
    /// the new session's `imported_from` field. Refuses a corrupt or
    /// tampered bundle with a precise error.
    #[command(after_help = examples(&[
        ("hh import session.hh", "import a bundle as a new session"),
    ]))]
    Import(ImportArgs),

    /// Search recorded sessions for text matches (FTS5 full-text search).
    ///
    /// Searches event summaries and message bodies across all sessions.
    /// Results show the session, step, event kind, and a highlighted snippet.
    /// Supports FTS5 query syntax: words, phrases in quotes, prefix with `*`,
    /// AND/OR/NOT operators.
    #[command(after_help = examples(&[
        ("hh search \"error\"", "every event mentioning error"),
        ("hh search \"create file\" --kind tool_call", "only tool calls"),
        ("hh search \"api key\" --agent claude-code --json", "Claude Code sessions, as JSON"),
    ]))]
    Search(SearchArgs),

    // SRS FR-6.2 (ergonomics).
    /// Print a shell completion script for `hh`.
    ///
    /// Writes the completion script for the given shell to stdout. Install it
    /// per your shell's conventions (the docs page below has the steps). The
    /// script is plain text and pipe-safe.
    #[command(after_help = with_docs(
        &examples(&[
            ("hh completions bash > /etc/bash_completion.d/hh", "bash"),
            ("hh completions zsh > ~/.zsh/_hh", "zsh"),
            ("hh completions fish > ~/.config/fish/completions/hh.fish", "fish"),
            ("hh completions powershell | Out-String | Invoke-Expression", "PowerShell"),
        ]),
        "completions/",
    ))]
    Completions(CompletionsArgs),
}

/// `display_order` for advanced, less-common subcommands: larger than any
/// order clap assigns by declaration, so they list after the daily commands
/// in `hh --help`.
const ADVANCED_DISPLAY_ORDER: usize = 1000;

/// Base URL of the docs site that `after_help` pointers resolve against.
const DOCS_URL: &str = "https://halfhand.org/docs/";

/// Render an `Example:`/`Examples:` block, one `(command, description)` row
/// per line, with every description starting in the same column: two spaces
/// past the longest command in the block. Padding is computed, never
/// hand-counted, so adding an example cannot misalign the block.
fn examples(rows: &[(&str, &str)]) -> String {
    let width = rows
        .iter()
        .map(|(cmd, _)| cmd.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::from(if rows.len() == 1 {
        "Example:"
    } else {
        "Examples:"
    });
    for (cmd, desc) in rows {
        out.push_str("\n  ");
        out.push_str(cmd);
        if !desc.is_empty() {
            let pad = width - cmd.chars().count() + 2;
            out.extend(std::iter::repeat_n(' ', pad));
            out.push_str(desc);
        }
    }
    out
}

/// Append a `Docs:` line pointing at `page` under [`DOCS_URL`] (an empty
/// `page` links the docs index).
fn with_docs(examples: &str, page: &str) -> String {
    format!("{examples}\n\nDocs: {DOCS_URL}{page}")
}

/// Arguments for `hh completions`.
#[derive(Args, Debug)]
pub struct CompletionsArgs {
    /// Target shell. One of: bash, zsh, fish, powershell, elvish.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}
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
// Each bool is an independent clap flag (`--html`, `--bundle`, `--open`,
// `--no-open`, `--no-redact`); a state enum would not map to the CLI surface.
#[allow(clippy::struct_excessive_bools)]
pub struct ExportArgs {
    /// Session short id, full id, or `last`. Defaults to `last`.
    pub session: Option<String>,

    /// Write to this file instead of stdout (`-o -` is stdout explicitly).
    #[arg(long, short = 'o')]
    pub out: Option<std::path::PathBuf>,

    /// Export a self-contained HTML page instead of the JSON bundle.
    #[arg(long, conflicts_with = "bundle")]
    pub html: bool,

    /// With `--html`: open the page in your default browser. Without `-o`,
    /// the page is written to `<data_dir>/exports/`. Only honoured when
    /// stdout is a terminal and `CI` is unset; never fails the command.
    #[arg(long, requires = "html", overrides_with = "no_open")]
    pub open: bool,

    /// Do not open a browser (overrides an earlier `--open`, e.g. from an
    /// alias).
    #[arg(long, overrides_with = "open")]
    pub no_open: bool,

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

    /// Filter by agent kind (claude-code, claude-desktop, codex-cli, gemini-cli, generic, mcp-only).
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
    // SRS NFR-4: keystrokes are not recorded unless asked for.
    /// Record user keystrokes (off by default; keystrokes may contain secrets).
    #[arg(long)]
    pub record_input: bool,

    /// Force an adapter rather than auto-detecting (`claude-code`,
    /// `claude-desktop`, `codex-cli`, `gemini-cli`).
    #[arg(long)]
    pub adapter: Option<String>,

    /// Name the recorded session (shown by `hh inspect`; change it later
    /// with `hh rename`).
    #[arg(long, value_parser = parse_session_name)]
    pub name: Option<String>,

    /// The command to run and its arguments, after `--`.
    #[arg(last = true, num_args = 1.., required = true)]
    pub command: Vec<String>,
}

/// Arguments for `hh replay`.
#[derive(Args, Debug)]
pub struct ReplayArgs {
    /// Session short id, full id, or `last` (most recent). Defaults to `last`.
    pub session: Option<String>,

    /// Export a self-contained HTML replay page and print its path, instead
    /// of opening the interactive TUI. Written to
    /// `<data_dir>/exports/hh-session-<id>.html` unless `-o` says otherwise.
    /// Does not need a terminal and does not start a server.
    #[arg(long)]
    pub web: bool,

    /// With `--web`: write the page here instead (`-o -` writes the HTML to
    /// stdout).
    #[arg(long, short = 'o', requires = "web")]
    pub out: Option<std::path::PathBuf>,

    /// With `--web`: open the page in your default browser. Only honoured
    /// when stdout is a terminal and `CI` is unset; never fails the command.
    #[arg(long, requires = "web", overrides_with = "no_open")]
    pub open: bool,

    /// Do not open a browser (the default; overrides an earlier `--open`).
    #[arg(long, overrides_with = "open")]
    pub no_open: bool,
}

/// Arguments for `hh inspect`.
#[derive(Args, Debug)]
// Each bool is an independent clap flag (`--json`, `--diff`, `--failed`,
// `--context`); a state enum would not map to the CLI surface.
#[allow(clippy::struct_excessive_bools)]
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

    /// Show the context panel: model, tokens, durations, estimated cost.
    ///
    /// Name, model, provider, context limit and usage, self-reported token
    /// counts (and a separate size-based estimate), message counts, created
    /// and last-activity times, wall and active duration, and estimated
    /// cost. Unknown values print as `—`. With --json, one JSON object.
    #[arg(long, conflicts_with_all = ["step", "diff", "failed"])]
    pub context: bool,
}

/// Arguments for `hh rename`.
#[derive(Args, Debug)]
pub struct RenameArgs {
    /// Session short id, full id, or `last`.
    pub session: String,

    /// The new name.
    #[arg(value_parser = parse_session_name, required_unless_present = "clear")]
    pub name: Option<String>,

    /// Remove the session's name instead of setting one.
    #[arg(long, conflicts_with = "name")]
    pub clear: bool,
}

/// Longest accepted session name, in characters.
const MAX_SESSION_NAME_CHARS: usize = 100;

/// clap value parser for a session name: trimmed, non-empty, at most
/// [`MAX_SESSION_NAME_CHARS`] characters, no control characters (a name is
/// printed to the terminal, so an escape sequence must not get through).
/// A rejection is a usage error (exit 2).
pub(crate) fn parse_session_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(
            "a session name cannot be empty; use `hh rename <id> --clear` to remove one".into(),
        );
    }
    if name.chars().count() > MAX_SESSION_NAME_CHARS {
        return Err(format!(
            "a session name is at most {MAX_SESSION_NAME_CHARS} characters; shorten it"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(
            "a session name cannot contain control characters (newlines, tabs, escapes)".into(),
        );
    }
    Ok(name.to_string())
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
    /// A human-readable hint for a standalone mcp-only session. Accepted for
    /// forward compatibility but not yet persisted anywhere (it does not set
    /// the session name; use `hh rename`) — it has no visible effect.
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Requirement-ID tokens found in `text`: `SRS`, `FR-<n>`, `NFR-<n>`,
    /// `DR-<n>`, `ENH-<n>`, `BUG-<n>` and `Area <n>`, matched as whole words
    /// (so `ADDR-1` or `Areas` do not count).
    fn requirement_ids(text: &str) -> Vec<String> {
        const PREFIXES: [&str; 5] = ["FR-", "NFR-", "DR-", "ENH-", "BUG-"];
        let words: Vec<&str> = text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
            .filter(|w| !w.is_empty())
            .collect();
        let mut found = Vec::new();
        for (i, word) in words.iter().enumerate() {
            let is_id = *word == "SRS"
                || PREFIXES.iter().any(|p| {
                    word.strip_prefix(p)
                        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
                })
                || (*word == "Area"
                    && words
                        .get(i + 1)
                        .is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit())));
            if is_id {
                found.push((*word).to_string());
            }
        }
        found
    }

    #[test]
    fn requirement_id_matcher_catches_every_form_and_only_those() {
        for leaky in [
            "Record a session (SRS FR-1)",
            "zero network calls (NFR-2).",
            "vacuum the database (Area 3)",
            "see DR-1",
            "fixes ENH-12",
            "regression for BUG-7",
            "completions (FR-6.2 ergonomics)",
        ] {
            assert!(
                !requirement_ids(leaky).is_empty(),
                "matcher missed a requirement id in {leaky:?}"
            );
        }
        for clean in [
            "ADDR-1 is not an id",
            "Areas of the store",
            "FRAME-1 and BUGS-2",
            "Area code",
            "https://halfhand.org/docs/redaction/",
        ] {
            assert_eq!(
                requirement_ids(clean),
                Vec::<String>::new(),
                "false positive in {clean:?}"
            );
        }
    }

    /// Every help text clap can render for `cmd` and its subcommands,
    /// recursively, labelled by command path.
    fn rendered_help(cmd: &clap::Command, path: &str, out: &mut Vec<(String, String)>) {
        let mut c = cmd.clone();
        out.push((format!("{path} --help"), c.render_long_help().to_string()));
        out.push((format!("{path} -h"), c.render_help().to_string()));
        out.push((format!("{path} usage"), c.render_usage().to_string()));
        for sub in cmd.get_subcommands() {
            rendered_help(sub, &format!("{path} {}", sub.get_name()), out);
        }
    }

    /// Error text clap renders for a bad flag and for missing arguments on
    /// every subcommand, plus an invalid value.
    fn rendered_errors(cmd: &clap::Command) -> Vec<(String, String)> {
        let mut argvs: Vec<Vec<String>> = vec![
            vec!["hh".into()],
            vec!["hh".into(), "--no-such-flag".into()],
            vec!["hh".into(), "no-such-command".into()],
            vec!["hh".into(), "completions".into(), "no-such-shell".into()],
        ];
        for sub in cmd.get_subcommands() {
            let name = sub.get_name().to_string();
            argvs.push(vec!["hh".into(), name.clone()]);
            argvs.push(vec!["hh".into(), name, "--no-such-flag".into()]);
        }
        argvs
            .into_iter()
            .filter_map(|argv| {
                let label = argv.join(" ");
                Cli::try_parse_from(argv)
                    .err()
                    .map(|e| (label, e.render().to_string()))
            })
            .collect()
    }

    /// Guard: no requirement ID reaches any string clap renders — about,
    /// long_about, after_help, argument help, usage, or error text — for `hh`
    /// or any subcommand at any depth. Tags belong in `//` comments.
    #[test]
    fn rendered_help_has_no_requirement_ids() {
        let mut root = Cli::command();
        root.build();
        let mut texts = Vec::new();
        rendered_help(&root, "hh", &mut texts);
        texts.extend(rendered_errors(&root));
        assert!(
            texts.len() > 40,
            "walked too few help texts ({}); is the walk broken?",
            texts.len()
        );
        let leaks: Vec<String> = texts
            .iter()
            .filter_map(|(label, text)| {
                let ids = requirement_ids(text);
                (!ids.is_empty()).then(|| format!("{label}: {ids:?}"))
            })
            .collect();
        assert!(
            leaks.is_empty(),
            "requirement IDs leaked into rendered help/errors:\n{}",
            leaks.join("\n")
        );
    }

    /// Every subcommand (except clap's own `help`) shows a usage example.
    #[test]
    fn every_subcommand_has_an_example_block() {
        let mut root = Cli::command();
        root.build();
        for sub in root.get_subcommands().filter(|s| s.get_name() != "help") {
            let after = sub
                .get_after_help()
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                after.starts_with("Example:") || after.starts_with("Examples:"),
                "`hh {}` has no example block: {after:?}",
                sub.get_name()
            );
        }
    }

    /// `mcp-proxy` is listed after the daily commands, but is still there.
    #[test]
    fn mcp_proxy_is_listed_last() {
        let mut root = Cli::command();
        root.build();
        let help = root.render_long_help().to_string();
        let commands: Vec<&str> = help
            .split("Commands:\n")
            .nth(1)
            .and_then(|s| s.split("\n\n").next())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        let pos = |name: &str| commands.iter().position(|c| *c == name);
        let mcp = pos("mcp-proxy").expect("mcp-proxy must still be listed");
        for daily in ["run", "replay", "inspect", "list", "delete", "completions"] {
            assert!(
                pos(daily).expect(daily) < mcp,
                "`{daily}` should list before mcp-proxy: {commands:?}"
            );
        }
    }

    #[test]
    fn examples_pad_descriptions_to_the_longest_command() {
        let block = examples(&[
            ("hh run -- claude", "record"),
            ("hh replay last", "replay"),
            ("hh x", ""),
        ]);
        assert_eq!(
            block,
            "Examples:\n  hh run -- claude  record\n  hh replay last    replay\n  hh x"
        );
        assert_eq!(examples(&[("hh a", "b")]), "Example:\n  hh a  b");
    }

    /// Every multi-row example block in rendered help has its descriptions
    /// starting in a single column.
    #[test]
    fn rendered_example_blocks_align() {
        let mut root = Cli::command();
        root.build();
        let mut texts = Vec::new();
        rendered_help(&root, "hh", &mut texts);
        for (label, text) in texts.iter().filter(|(l, _)| l.ends_with("--help")) {
            // The block runs from its header to the next blank line.
            let Some(block) = text
                .split("Examples:\n")
                .nth(1)
                .map(|s| s.split("\n\n").next().unwrap_or_default())
            else {
                continue;
            };
            let starts: std::collections::BTreeSet<usize> = block
                .lines()
                .filter_map(|l| {
                    let body = l.strip_prefix("  ")?;
                    let gap = body.find("  ")?;
                    Some(2 + gap + body[gap..].len() - body[gap..].trim_start().len())
                })
                .collect();
            assert!(
                starts.len() <= 1,
                "{label}: example descriptions start in different columns {starts:?}:\n{block}"
            );
        }
    }
}
