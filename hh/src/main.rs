//! `hh` — the Halfhand binary entry point (SRS §1.2, FR-6.2).
//!
//! Wires the CLI surface (clap derive) to the data layer (`hh_core`) and the
//! recorder (`hh_record`). `hh run` is fully implemented (FR-1); the other
//! subcommands return a structured "not implemented" error for now.

include!(concat!(env!("OUT_DIR"), "/hh_version.rs"));

mod cli;
mod ui;

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
        Command::Replay(args) => Err(ui::not_implemented(
            "replay",
            &format!(
                "session hint: {}",
                args.session.as_deref().unwrap_or("last")
            ),
        )),
        Command::Inspect(args) => Err(ui::not_implemented(
            "inspect",
            &format!(
                "session hint: {}; use `hh list` to find a session id",
                args.session.as_deref().unwrap_or("last")
            ),
        )),
        Command::List(args) => list_command(&args),
        Command::Delete(args) => Err(ui::not_implemented(
            "delete",
            &format!("session hint: {}", args.session),
        )),
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
        .map_err(|e| anyhow::anyhow!("could not determine current directory: {e}"))?;

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
        .map_err(|e| anyhow::anyhow!("recording failed\n  why: {e}"))?;

    print_epilogue(&outcome);
    // Propagate the child's exit code so `hh run -- make` behaves like `make`.
    Ok(child_exit_code(outcome.exit_code))
}

/// `hh mcp-proxy` (FR-2): stdio JSON-RPC middleman. Forwards verbatim and
/// records each message as an event, attaching to `HH_SESSION_ID` if present or
/// creating a standalone `mcp-only` session.
fn mcp_proxy_command(args: cli::McpProxyArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, _config) = open_store()?;
    let cwd = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("could not determine current directory: {e}"))?;
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
    let color = std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
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
    let color = std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
    let check = if color {
        "✓".green().to_string()
    } else {
        "✓".to_string()
    };
    println!(
        "{check} Recorded session {sid} · {dur} · {steps} steps · {files} files changed · hh replay {sid}",
        sid = outcome.short_id,
        dur = humanize_ms(outcome.duration_ms),
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

/// Render the session list as an aligned plain-text table (FR-5.1). Respects
/// `NO_COLOR` and non-TTY output (plain, pipe-safe per CLAUDE.md): one accent
/// color (green) on the success glyph and id; error/interrupted use ✗/● in red
/// to match the existing epilogue/error convention.
fn print_session_table(rows: &[hh_core::SessionRow]) {
    use hh_core::SessionStatus as S;
    if rows.is_empty() {
        println!("no sessions recorded yet — run `hh run -- <command>` to record one");
        return;
    }
    let color = use_color();
    let now = now_unix_ms();

    // Build cell strings, then column-align them. Status carries a glyph.
    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    for r in rows {
        let glyph = match r.status {
            S::Ok => "✓",
            S::Error => "✗",
            S::Interrupted | S::Recording => "●",
        };
        let status_field = format!("{glyph} {status}", status = r.status);
        let status = if color {
            match r.status {
                S::Ok => status_field.green().to_string(),
                S::Error | S::Interrupted => status_field.red().to_string(),
                S::Recording => status_field.dimmed().to_string(),
            }
        } else {
            status_field
        };
        let id_field = r.short_id.clone();
        let id = if color {
            id_field.green().to_string()
        } else {
            id_field
        };
        let duration = match r.ended_at {
            Some(end) => humanize_ms((end - r.started_at).max(0)),
            None => "—".to_string(),
        };
        let command = truncate(&r.command.join(" "), 40);
        lines.push(Line {
            id,
            status,
            agent: r.agent_kind.to_string(),
            started: humanize_relative(r.started_at, now),
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

/// Humanize a `started_at` (unix-ms) relative to `now` (FR-5.1). Recent sessions
/// show "just now"/"5m ago"; older than a week show an absolute `YYYY-MM-DD`.
fn humanize_relative(started_at: i64, now: i64) -> String {
    let delta = now - started_at;
    if delta < 0 {
        return "just now".to_string();
    }
    let secs = delta / 1000;
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    // Absolute date for anything older than a week (avoids a `time`/chrono dep
    // in the binary; civil-calendar math on unix days is sufficient here).
    format_date_from_unix_days(started_at / 86_400_000)
}

/// Format a unix-day count as `YYYY-MM-DD` using the proleptic Gregorian
/// calendar. Good enough for the list view's "older than a week" column; we do
/// not need second-precision timestamps here.
fn format_date_from_unix_days(days: i64) -> String {
    // Algorithm from Howard Hinnant's `civil_from_days`.
    let z = days + 719_468; // days since 0000-03-01
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Truncate `s` to `max` chars, appending `…` if it was longer.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}…")
}

/// Whether to emit ANSI color on stdout: disabled by `NO_COLOR` or non-TTY
/// output (CLAUDE.md: plain, pipe-safe).
fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
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

/// Current unix-ms UTC timestamp (used for relative "started" times).
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
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

/// Humanize a millisecond duration as `4m32s` / `12s` / `350ms` (NFR-7).
fn humanize_ms(ms: i64) -> String {
    if ms < 0 {
        return "0ms".to_string();
    }
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{hours}h{minutes}m{secs}s")
    } else if minutes > 0 {
        format!("{minutes}m{secs}s")
    } else {
        format!("{secs}s")
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
    let color = std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal();
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
