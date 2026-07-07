//! `hh` — the Halfhand binary entry point (SRS §1.2, FR-6.2).
//!
//! Wires the CLI surface (clap derive) to the data layer (`hh_core`) and the
//! recorder (`hh_record`). `hh run` is fully implemented (FR-1); the other
//! subcommands return a structured "not implemented" error for now.

include!(concat!(env!("OUT_DIR"), "/hh_version.rs"));

mod cli;
mod inspect;
mod render;
mod replay;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use cli::{Cli, Command};
use hh_core::config::{Config, Paths};
use hh_core::store::Store;
use owo_colors::OwoColorize;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            print_error(&e);
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Run(args) => run_command(args),
        Command::Replay(args) => replay_command(&args),
        Command::Inspect(args) => inspect::inspect_command(&args),
        Command::List(args) => list_command(&args),
        Command::Delete(args) => delete_command(&args),
        Command::McpProxy(args) => mcp_proxy_command(args),
    }
}

/// Load config + open the store, applying FR-1.7 (mark stale `recording`
/// sessions `interrupted`) on every open. Returns the store, resolved paths,
/// and loaded config for the subcommand.
///
/// Config precedence (SRS §2.3): `HH_DATA_DIR` env > `[storage] data_dir`
/// file > platform default. We resolve once with defaults to find the config
/// file's platform path, load it, then resolve again so `[storage] data_dir`
/// is honored.
fn open_store() -> anyhow::Result<(Store, Paths, Config)> {
    let paths0 = Paths::resolve(&Config::default())
        .map_err(|e| anyhow::anyhow!("could not resolve data directory\n  why: {e}\n  hint: set HH_DATA_DIR to a writable directory"))?;
    // Best-effort config load: unknown keys warn on stderr, never fail
    // (SRS §4.2). If the config file is unreadable for another reason, fall
    // back to defaults rather than blocking the run.
    let config = Config::load(&paths0.config_path).unwrap_or_else(|e| {
        eprintln!(
            "hh: warning: could not load {}: {e}",
            paths0.config_path.display()
        );
        Config::default()
    });
    let paths = Paths::resolve(&config).map_err(|e| {
        anyhow::anyhow!("could not resolve data directory\n  why: {e}\n  hint: set HH_DATA_DIR to a writable directory")
    })?;
    let store = Store::open(&paths.db_path, &paths.blobs_dir).map_err(|e| {
        anyhow::anyhow!(
            "could not open store at {}\n  why: {e}\n  hint: check permissions on the data directory",
            paths.db_path.display()
        )
    })?;
    // FR-1.7: mark sessions left `recording` by a crashed `hh run` as
    // `interrupted`. Best-effort; a failure here does not block the run.
    if let Err(e) = store.mark_stale_interrupted() {
        eprintln!("hh: warning: could not reconcile stale sessions: {e}");
    }
    Ok((store, paths, config))
}

/// `hh run` (FR-1): record an agent session.
fn run_command(args: cli::RunArgs) -> anyhow::Result<ExitCode> {
    let (store, paths, config) = open_store()?;

    let cwd = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("could not determine current directory\n  why: {e}\n  hint: run `hh` from a directory that exists and is accessible"))?;

    // Exclude the halfhand data dir / db / blobs from the FS watcher when they
    // live under cwd, so the recorder does not record itself writing the DB.
    let internal_exclude = paths_under(&cwd, &[&paths.db_path, &paths.blobs_dir, &paths.data_dir]);

    let opts = hh_record::RunOptions {
        command: args.command,
        adapter: args.adapter,
        cwd,
        max_file_size: config.record.max_file_size,
        record_input: args.record_input || config.record.record_input,
        record_binary: config.record.record_binary,
        extra_ignore: config.record.ignore,
        internal_exclude,
        hh_version: HH_VERSION.to_string(),
    };

    let outcome = hh_record::run(&store, &opts)
        .map_err(|e| anyhow::anyhow!("recording failed\n  why: {e}\n  hint: a session may still have been recorded — run `hh list` to check, then `hh inspect <id>` for details"))?;

    print_epilogue(&outcome);
    // Propagate the child's exit code so `hh run -- make` behaves like `make`.
    Ok(child_exit_code(outcome.exit_code))
}

/// `hh replay` (FR-3): open the interactive TUI on a recorded session.
/// Requires a real terminal — the TUI needs raw mode, which a pipe/redirect
/// cannot provide (CLAUDE.md: "Respect NO_COLOR and non-TTY output"; for an
/// inherently interactive command that means refusing clearly rather than
/// failing deep inside crossterm).
fn replay_command(args: &cli::ReplayArgs) -> anyhow::Result<ExitCode> {
    if !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "`hh replay` needs an interactive terminal\n  \
             why: stdout is not a TTY (piped or redirected)\n  \
             hint: run `hh replay` directly in a terminal, or use `hh inspect` for non-interactive output"
        );
    }
    let (store, _paths, _config) = open_store()?;
    let hint = args.session.as_deref().unwrap_or("last");
    let session_id = store
        .resolve_session(hint)
        .map_err(|e| anyhow::anyhow!("could not resolve session `{hint}`\n  why: {e}"))?;
    let session = store
        .get_session(&session_id)
        .map_err(|e| anyhow::anyhow!("could not load session\n  why: {e}"))?;
    let index = store
        .list_event_index(&session_id)
        .map_err(|e| anyhow::anyhow!("could not load session events\n  why: {e}"))?;
    let no_color = std::env::var_os("NO_COLOR").is_some();
    replay::run(store, session, index, no_color)?;
    Ok(ExitCode::SUCCESS)
}

/// `hh mcp-proxy` (FR-2): stdio JSON-RPC middleman. Forwards verbatim and
/// records each message as an event, attaching to `HH_SESSION_ID` if present or
/// creating a standalone `mcp-only` session.
fn mcp_proxy_command(args: cli::McpProxyArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, _config) = open_store()?;
    let cwd = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("could not determine current directory\n  why: {e}\n  hint: run `hh` from a directory that exists and is accessible"))?;
    let opts = hh_record::McpProxyOptions {
        command: args.command,
        cwd,
        session_hint: args.session_hint,
        hh_version: HH_VERSION.to_string(),
    };
    let outcome = hh_record::run_mcp_proxy(&store, &opts)
        .map_err(|e| anyhow::anyhow!("mcp proxy failed\n  why: {e}"))?;
    print_mcp_epilogue(&outcome);
    Ok(child_exit_code(outcome.exit_code))
}

/// Print the one-line MCP proxy epilogue (FR-2): the session it recorded into
/// and the server's exit outcome. Attached mode notes the parent session.
fn print_mcp_epilogue(outcome: &hh_record::McpProxyOutcome) {
    let color = render::use_color();
    let check = if color {
        "✓".green().to_string()
    } else {
        "✓".to_string()
    };
    let id = if color {
        outcome.short_id.green().to_string()
    } else {
        outcome.short_id.clone()
    };
    let exit = outcome
        .exit_code
        .map_or_else(|| "unknown".into(), |c| c.to_string());
    if outcome.attached {
        println!(
            "{check} MCP proxy attached to session {sid} · exit {exit}",
            sid = outcome.session_id
        );
    } else {
        println!("{check} Recorded mcp-only session {id} · exit {exit} · hh replay {id}");
    }
}

/// Print the one-line epilogue (FR-1.6):
/// `✓ Recorded session a1b2c3 · 4m32s · 42 steps · 7 files changed · hh replay a1b2c3`
fn print_epilogue(outcome: &hh_record::RunOutcome) {
    let color = render::use_color();
    let check = if color {
        "✓".green().to_string()
    } else {
        "✓".to_string()
    };
    println!(
        "{check} Recorded session {sid} · {dur} · {steps} steps · {files} files changed · hh replay {sid}",
        sid = outcome.short_id,
        dur = render::humanize_ms(outcome.duration_ms),
        steps = outcome.steps,
        files = outcome.files_changed,
    );
}

/// `hh list` (FR-5.1): list recorded sessions newest-first as an aligned table
/// (default) or a JSON array (`--json`), bounded by `--limit`.
fn list_command(args: &cli::ListArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, _config) = open_store()?;
    let rows = store
        .list_sessions(args.limit)
        .map_err(|e| anyhow::anyhow!("could not list sessions\n  why: {e}"))?;
    if args.json {
        // Stable, documented JSON shape (FR-5.1): an array of one object per
        // session. Timestamps are unix-ms integers (no timezone ambiguity);
        // `duration_ms` is `null` while a session is still recording or was
        // interrupted before finalizing.
        let arr: Vec<serde_json::Value> = rows.iter().map(session_to_json).collect();
        println!("{}", serde_json::Value::Array(arr));
    } else {
        print_session_table(&rows);
    }
    Ok(ExitCode::SUCCESS)
}

/// `hh delete` (FR-6.1): delete a session and garbage-collect its blobs.
/// Prompts for confirmation unless `--yes`; refuses non-interactively (piped
/// stdin) without `--yes` so a stray `hh delete last` in a script never
/// silently destroys data (NFR-4 / actionable errors).
fn delete_command(args: &cli::DeleteArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, _config) = open_store()?;
    let id = store
        .resolve_session(&args.session)
        .map_err(|e| anyhow::anyhow!("could not resolve session `{}`\n  why: {e}", args.session))?;
    let session = store
        .get_session(&id)
        .map_err(|e| anyhow::anyhow!("could not load session\n  why: {e}"))?;

    if !args.yes {
        confirm_delete(&session)?;
    }

    let removed = store.delete_session(&id).map_err(|e| {
        anyhow::anyhow!(
            "could not delete session `{}`\n  why: {e}",
            session.short_id
        )
    })?;

    let color = render::use_color();
    let check = if color {
        "✓".green().to_string()
    } else {
        "✓".to_string()
    };
    println!(
        "{check} Deleted session {sid} · {n} blob(s) garbage-collected · {steps} steps removed",
        sid = session.short_id,
        n = removed,
        steps = session.step_count,
    );
    Ok(ExitCode::SUCCESS)
}

/// Prompt the user to confirm deleting `session` on an interactive stdin. A
/// non-TTY stdin (piped/redirected) is refused with an actionable error
/// pointing at `--yes`, so deletion can never happen by accident in a script.
fn confirm_delete(session: &hh_core::SessionRow) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "refusing to delete without confirmation\n  \
             why: stdin is not a TTY (piped or redirected), so the confirmation prompt cannot be read\n  \
             hint: re-run with `hh delete {} --yes` to skip the prompt",
            session.short_id
        );
    }
    let color = render::use_color_stderr();
    let prefix = if color {
        "●".yellow().to_string()
    } else {
        "●".to_string()
    };
    let command = render::truncate(&session.command.join(" "), 60);
    eprint!(
        "{prefix} Delete session {sid} ({status}, {command})? [y/N] ",
        sid = session.short_id,
        status = session.status,
    );
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| anyhow::anyhow!("could not read confirmation\n  why: {e}\n  hint: re-run with `hh delete {sid} --yes` to skip the prompt", sid = session.short_id))?;
    let answer = line.trim().to_lowercase();
    if answer != "y" && answer != "yes" {
        anyhow::bail!("delete cancelled");
    }
    Ok(())
}

/// Render the session list as an aligned plain-text table (FR-5.1). Respects
/// `NO_COLOR` and non-TTY output (plain, pipe-safe per CLAUDE.md): one accent
/// color (green) on the id; the STATUS column carries a colored glyph per
/// state (ok=green ✓, error=red ✗, interrupted=yellow ●, recording=cyan ●),
/// via the shared [`render::render_status`] so `hh list` and `hh inspect`
/// speak the same visual language.
fn print_session_table(rows: &[hh_core::SessionRow]) {
    if rows.is_empty() {
        println!("no sessions recorded yet — run `hh run -- <command>` to record one");
        return;
    }
    let color = render::use_color();
    let now = render::now_unix_ms();

    // Build cell strings, then column-align them. Status carries a glyph.
    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    for r in rows {
        let status = render::render_status(r.status, color);
        let id_field = r.short_id.clone();
        let id = if color {
            id_field.green().to_string()
        } else {
            id_field
        };
        let duration = match r.ended_at {
            Some(end) => render::humanize_ms((end - r.started_at).max(0)),
            None => "—".to_string(),
        };
        let command = render::truncate(&r.command.join(" "), 40);
        lines.push(Line {
            id,
            status,
            agent: r.agent_kind.to_string(),
            started: render::humanize_relative(r.started_at, now),
            duration,
            steps: r.step_count.to_string(),
            files: r.files_changed.to_string(),
            command,
        });
    }

    let headers = [
        "ID", "STATUS", "AGENT", "STARTED", "DURATION", "STEPS", "FILES", "COMMAND",
    ];
    let widths = compute_widths(&headers, &lines);

    let mut header_line = String::new();
    for (i, h) in headers.iter().enumerate() {
        pad_into(&mut header_line, h, widths[i]);
    }
    println!("{header_line}");
    let mut sep = String::new();
    for &w in &widths {
        for _ in 0..w {
            sep.push('-');
        }
        sep.push(' ');
    }
    println!("{sep}");
    for l in &lines {
        let mut row = String::new();
        pad_into(&mut row, &l.id, widths[0]);
        pad_into(&mut row, &l.status, widths[1]);
        pad_into(&mut row, &l.agent, widths[2]);
        pad_into(&mut row, &l.started, widths[3]);
        pad_into(&mut row, &l.duration, widths[4]);
        pad_into(&mut row, &l.steps, widths[5]);
        pad_into(&mut row, &l.files, widths[6]);
        row.push_str(&l.command);
        println!("{row}");
    }
}

/// A row's rendered cells for the list table.
struct Line {
    /// Short id (colored if enabled).
    id: String,
    /// `glyph status` (colored if enabled).
    status: String,
    /// Agent kind.
    agent: String,
    /// Relative "started" time.
    started: String,
    /// Humanized duration, or `—` while recording/interrupted.
    duration: String,
    /// Step count.
    steps: String,
    /// Files-changed count.
    files: String,
    /// Truncated command line.
    command: String,
}

/// Return the column widths for the list table: `max(header.len, max cell.len)`
/// per column. The COMMAND column is left unbounded (last column, no padding).
fn compute_widths(headers: &[&str; 8], lines: &[Line]) -> [usize; 8] {
    let cell = |l: &Line, i: usize| -> usize {
        match i {
            0 => l.id.len(),
            1 => l.status.len(),
            2 => l.agent.len(),
            3 => l.started.len(),
            4 => l.duration.len(),
            5 => l.steps.len(),
            6 => l.files.len(),
            _ => l.command.len(),
        }
    };
    let mut widths = [0usize; 8];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
        for l in lines {
            widths[i] = widths[i].max(cell(l, i));
        }
    }
    widths
}

/// Append `s` to `out` left-justified in `width` plus one trailing space.
fn pad_into(out: &mut String, s: &str, width: usize) {
    out.push_str(s);
    let pad = width.saturating_sub(s.len());
    for _ in 0..pad {
        out.push(' ');
    }
    out.push(' ');
}

/// Build the JSON object for one session row (`hh list --json`, FR-5.1).
fn session_to_json(r: &hh_core::SessionRow) -> serde_json::Value {
    let duration_ms = r.ended_at.map(|end| (end - r.started_at).max(0));
    serde_json::json!({
        "id": r.id,
        "short_id": r.short_id,
        "status": r.status.to_string(),
        "agent_kind": r.agent_kind.to_string(),
        "adapter_status": r.adapter_status.to_string(),
        "started_at": r.started_at,
        "ended_at": r.ended_at,
        "exit_code": r.exit_code,
        "duration_ms": duration_ms,
        "steps": r.step_count,
        "files_changed": r.files_changed,
        "command": r.command,
        "cwd": r.cwd.to_string_lossy(),
    })
}

/// Map an `Option<i32>` child exit code to an [`ExitCode`]. `None` (code could
/// not be determined, e.g. killed by an unreported signal) becomes 1.
fn child_exit_code(code: Option<i32>) -> ExitCode {
    match code {
        Some(c) => {
            // Process exit codes are 0–255; clamp into that range. `try_from`
            // avoids a sign-loss cast (clippy::cast_sign_loss) and is infallible
            // after the clamp (0..=255).
            let c = c.clamp(0, 255);
            ExitCode::from(u8::try_from(c).unwrap_or(0))
        }
        None => ExitCode::from(1),
    }
}

/// Return the subset of `candidates` that are equal to or nested under `root`.
fn paths_under(root: &std::path::Path, candidates: &[&PathBuf]) -> Vec<PathBuf> {
    candidates
        .iter()
        .filter_map(|c| {
            if c.starts_with(root) {
                Some((*c).clone())
            } else {
                None
            }
        })
        .collect()
}

fn print_error(e: &anyhow::Error) {
    let color = render::use_color_stderr();
    let prefix = if color {
        "✗".red().to_string()
    } else {
        "✗".to_string()
    };
    eprintln!("{prefix} {e:#}");
    for cause in e.chain().skip(1) {
        eprintln!("  caused by: {cause}");
    }
}
