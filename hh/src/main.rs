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
        Command::List(_args) => Err(ui::not_implemented(
            "list",
            "list reads the SQLite store, which is ready in hh-core; wire it up next",
        )),
        Command::Delete(args) => Err(ui::not_implemented(
            "delete",
            &format!("session hint: {}", args.session),
        )),
        Command::McpProxy(args) => Err(ui::not_implemented(
            "mcp-proxy",
            &format!(
                "wrap `{}`; this is the next task: stdio JSON-RPC middleman",
                args.command.join(" ")
            ),
        )),
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
