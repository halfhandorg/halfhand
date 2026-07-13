//! `hh` — the Halfhand binary entry point (SRS §1.2, FR-6.2).
//!
//! Wires the CLI surface (clap derive) to the data layer (`hh_core`) and the
//! recorder (`hh_record`). `hh run` is fully implemented (FR-1); the other
//! subcommands return a structured "not implemented" error for now.

include!(concat!(env!("OUT_DIR"), "/hh_version.rs"));

mod cli;
mod export;
mod import;
mod inspect;
mod render;
mod replay;
mod secrets;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use cli::{Cli, Command};
use hh_core::config::{Config, Paths};
use hh_core::event::{AdapterStatus, AgentKind};
use hh_core::store::Store;
use owo_colors::OwoColorize;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            print_error(&e);
            // Exit-code contract (CLAUDE.md / README): 3 = session not
            // found, 1 = any other error. clap emits 2 for usage errors on
            // its own; `hh scan` returns 4 itself when findings exist.
            if e.chain()
                .any(|c| c.downcast_ref::<SessionNotFound>().is_some())
            {
                ExitCode::from(3)
            } else {
                ExitCode::from(1)
            }
        }
    }
}

/// Typed marker for "the session argument did not resolve", so [`main`] can
/// map it to exit code 3 without string-matching error text. Carries the
/// full, already-formatted actionable message.
#[derive(Debug)]
struct SessionNotFound(String);

impl std::fmt::Display for SessionNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SessionNotFound {}

/// Resolve a session id/`last` argument, mapping failure to the typed
/// [`SessionNotFound`] (exit code 3) with the same actionable message every
/// subcommand used before. Shared by every session-taking subcommand.
fn resolve_session_arg(store: &Store, hint: &str) -> anyhow::Result<String> {
    store.resolve_session(hint).map_err(|e| {
        anyhow::Error::new(SessionNotFound(format!(
            "could not resolve session `{hint}`\n  why: {e}"
        )))
    })
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Run(args) => run_command(args),
        Command::Replay(args) => replay_command(&args),
        Command::Inspect(args) => inspect::inspect_command(&args),
        Command::List(args) => list_command(&args),
        Command::Delete(args) => delete_command(&args),
        Command::McpProxy(args) => mcp_proxy_command(args),
        Command::Doctor(args) => doctor_command(&args),
        Command::Gc(args) => gc_command(&args),
        Command::Stats(args) => stats_command(&args),
        Command::Scan(args) => secrets::scan_command(&args),
        Command::Redact(args) => secrets::redact_command(&args),
        Command::Export(args) => export::export_command(&args),
        Command::Import(args) => import::import_command(&args),
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
    // Warn if the user wrote halfhand.toml / hh.toml *alongside* config.toml
    // (then it is silently ignored — only config.toml is read). When
    // config.toml is absent, Config::load above already fell back to the
    // legacy file and loaded it, so this is a no-op in that case. Catches the
    // "my ignore globs never applied" class of silent misconfiguration before
    // it causes a confusing recording.
    hh_core::config::warn_on_ignored_config_files(&paths0.config_path);
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

/// Build the record-time redactor when `[redaction] at_record = true`
/// (docs/redaction-design.md enforcement point 1), `None` otherwise. An
/// invalid user rule is an actionable error — recording *without* a detector
/// the user configured would be a silent redaction hole.
fn record_redactor(
    config: &hh_core::Config,
) -> anyhow::Result<Option<std::sync::Arc<hh_core::Detectors>>> {
    if !config.redaction.at_record {
        return Ok(None);
    }
    let detectors = hh_core::Detectors::new(&config.redaction)
        .map_err(|e| anyhow::anyhow!("could not compile redaction detectors\n  why: {e}"))?;
    Ok(Some(std::sync::Arc::new(detectors)))
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
        extra_ignore: config.record.ignore.clone(),
        internal_exclude,
        hh_version: HH_VERSION.to_string(),
        redactor: record_redactor(&config)?,
    };

    let outcome = hh_record::run(&store, &opts)
        .map_err(|e| anyhow::anyhow!("recording failed\n  why: {e}\n  hint: a session may still have been recorded — run `hh list` to check, then `hh inspect <id>` for details"))?;

    print_epilogue(&outcome);
    // Propagate the child's exit code so `hh run -- make` behaves like `make`.
    Ok(child_exit_code(outcome.exit_code))
}

/// `hh replay` (FR-3): open the interactive TUI on a recorded session, or
/// (`--web`) export a self-contained HTML replay page to a temp file and
/// print its path.
///
/// The TUI path requires a real terminal — it needs raw mode, which a
/// pipe/redirect cannot provide (CLAUDE.md: "Respect NO_COLOR and non-TTY
/// output"; for an inherently interactive command that means refusing
/// clearly rather than failing deep inside crossterm). `--web` needs no
/// terminal at all, so it is handled before that check.
fn replay_command(args: &cli::ReplayArgs) -> anyhow::Result<ExitCode> {
    if args.web {
        return replay_web_command(args);
    }
    if !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "`hh replay` needs an interactive terminal\n  \
             why: stdout is not a TTY (piped or redirected)\n  \
             hint: run `hh replay` directly in a terminal, or use `hh inspect` for non-interactive output"
        );
    }
    let (store, _paths, _config) = open_store()?;
    let hint = args.session.as_deref().unwrap_or("last");
    let session_id = resolve_session_arg(&store, hint)?;
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

/// `hh replay --web` (sugar): builds the same redacted HTML page `hh export
/// --html` would, writes it to a temp file, and prints the path — no
/// terminal, browser auto-open, or server, per v1 scope.
fn replay_web_command(args: &cli::ReplayArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, config) = open_store()?;
    let hint = args.session.as_deref().unwrap_or("last");
    let session_id = resolve_session_arg(&store, hint)?;
    let session = store
        .get_session(&session_id)
        .map_err(|e| anyhow::anyhow!("could not load session\n  why: {e}"))?;
    let detectors = secrets::detectors(&config)?;
    let html = export::build_redacted_html(&store, &session, Some(&detectors))?;

    let path = std::env::temp_dir().join(format!("hh-replay-{}.html", session.short_id));
    std::fs::write(&path, &html).map_err(|e| {
        anyhow::anyhow!(
            "could not write replay page to {}\n  why: {e}",
            path.display()
        )
    })?;

    let color = render::use_color();
    let check = if color {
        "✓".green().to_string()
    } else {
        "✓".to_string()
    };
    println!(
        "{check} Exported session {sid} → {path} (html, redacted) — open it in a browser",
        sid = session.short_id,
        path = path.display(),
    );
    Ok(ExitCode::SUCCESS)
}

/// `hh mcp-proxy` (FR-2): stdio JSON-RPC middleman. Forwards verbatim and
/// records each message as an event, attaching to `HH_SESSION_ID` if present or
/// creating a standalone `mcp-only` session.
fn mcp_proxy_command(args: cli::McpProxyArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, config) = open_store()?;
    let cwd = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("could not determine current directory\n  why: {e}\n  hint: run `hh` from a directory that exists and is accessible"))?;
    let opts = hh_record::McpProxyOptions {
        command: args.command,
        cwd,
        session_hint: args.session_hint,
        hh_version: HH_VERSION.to_string(),
        redactor: record_redactor(&config)?,
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

/// `hh doctor` (v1.0.0 addendum: a feature ships with a docs page + snapshot).
/// A read-only diagnostic that runs a fixed set of health checks against the
/// recording stack and prints one `✓`/`✗` line per check, exiting nonzero if any
/// fail. The checks target the failure modes behind the "silently recorded 0
/// steps" class of bug:
/// - data dir writability — the recorder cannot persist without it;
/// - DB integrity (`PRAGMA integrity_check`) — a corrupt DB explains missing
///   sessions/steps;
/// - config resolution + non-canonical config detection — `halfhand.toml` is
///   silently ignored (only `config.toml` is read), so ignore globs / a custom
///   data dir quietly never apply;
/// - Claude Code JSONL discoverability + a parse-test of the newest transcript
///   for the current cwd — the adapter tailer reads these, and a missing /
///   unparseable transcript is the direct cause of 0 recorded steps;
/// - a watcher smoke test — confirms `notify` delivers file-change events on
///   this platform (a watcher that silently never fires explains 0 files
///   changed).
///
/// `--json` emits a stable object with a per-check array for scripting.
fn doctor_command(args: &cli::DoctorArgs) -> anyhow::Result<ExitCode> {
    let (store, paths, _config) = open_store()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let checks = [
        check_data_dir_writable(&paths.data_dir),
        check_db_integrity(&store),
        check_config_resolution(&paths.config_path),
        check_claude_jsonl_discoverable(&cwd),
        check_watcher_smoke(),
    ];
    let any_failed = checks.iter().any(|c| !c.passed);

    if args.json {
        let arr: Vec<serde_json::Value> = checks.iter().map(doctor_check_to_json).collect();
        let obj = serde_json::json!({
            "schema": inspect::SCHEMA_VERSION,
            "status": if any_failed { "fail" } else { "ok" },
            "checks": arr,
        });
        println!("{obj}");
    } else {
        print!("{}", render_doctor_plain(&checks, render::use_color()));
    }

    Ok(if any_failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// One `hh doctor` check result: a stable name, pass/fail, and a human detail.
struct DoctorCheck {
    /// Stable, machine-readable check name (used as the JSON `name` field).
    name: &'static str,
    /// `true` when the check passed.
    passed: bool,
    /// Human-readable outcome / suggested fix.
    detail: String,
}

impl DoctorCheck {
    /// A passing check with `detail`.
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: true,
            detail: detail.into(),
        }
    }

    /// A failing check with `detail` (which should include a suggested fix).
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: false,
            detail: detail.into(),
        }
    }
}

/// Build the JSON object for one doctor check (`hh doctor --json`).
fn doctor_check_to_json(c: &DoctorCheck) -> serde_json::Value {
    serde_json::json!({
        "name": c.name,
        "status": if c.passed { "ok" } else { "fail" },
        "detail": c.detail,
    })
}

/// Render the doctor checks as plain `✓`/`✗ name — detail` lines (one accent
/// color: green pass, red fail). Factored out so a snapshot test can lock the
/// human-readable output without depending on a TTY, the real data dir, or the
/// real Claude transcript.
fn render_doctor_plain(checks: &[DoctorCheck], color: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for c in checks {
        let glyph = if c.passed {
            if color {
                "✓".green().to_string()
            } else {
                "✓".to_string()
            }
        } else if color {
            "✗".red().to_string()
        } else {
            "✗".to_string()
        };
        let _ = writeln!(
            out,
            "{glyph} {name} — {detail}",
            name = c.name,
            detail = c.detail
        );
    }
    out
}

/// Check 1: the data dir is writable. Writes and removes a self-cleaning probe
/// file so the test exercises real create+write+delete permissions, not just
/// `metadata`.
fn check_data_dir_writable(data_dir: &std::path::Path) -> DoctorCheck {
    const NAME: &str = "data dir writable";
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        return DoctorCheck::fail(
            NAME,
            format!(
                "could not create {}: {e} — set HH_DATA_DIR to a writable directory",
                data_dir.display()
            ),
        );
    }
    let probe = data_dir.join(format!(".hh-doctor-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"hh doctor") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            DoctorCheck::pass(NAME, format!("{} is writable", data_dir.display()))
        }
        Err(e) => DoctorCheck::fail(
            NAME,
            format!(
                "cannot write to {}: {e} — check permissions on the data directory",
                data_dir.display()
            ),
        ),
    }
}

/// Check 2: SQLite integrity via `PRAGMA integrity_check` (read-only probe).
fn check_db_integrity(store: &Store) -> DoctorCheck {
    const NAME: &str = "database integrity";
    match store.integrity_check() {
        Ok(s) if s == "ok" => DoctorCheck::pass(NAME, "PRAGMA integrity_check: ok"),
        Ok(s) => DoctorCheck::fail(
            NAME,
            format!("integrity_check reports `{s}` — back up the data dir and re-record"),
        ),
        Err(e) => DoctorCheck::fail(NAME, format!("could not run integrity_check: {e}")),
    }
}

/// Check 3: config resolution. Reports the canonical config path and fails if a
/// non-canonical file (`halfhand.toml` / `hh.toml`) is present *alongside* a
/// present `config.toml` — then it is silently ignored, so its settings (ignore
/// globs, data dir) never apply. When `config.toml` is absent, a legacy file is
/// loaded as a fallback (see `Config::load`), so this check passes instead.
fn check_config_resolution(config_path: &std::path::Path) -> DoctorCheck {
    const NAME: &str = "config resolution";
    let ignored = hh_core::config::ignored_noncanonical_config_files(config_path);
    let mut detail = format!("config: {}", config_path.display());
    if !config_path.exists() {
        detail.push_str(" (absent — defaults in effect)");
    }
    if ignored.is_empty() {
        return DoctorCheck::pass(NAME, detail);
    }
    let names: Vec<String> = ignored.iter().map(|p| p.display().to_string()).collect();
    DoctorCheck::fail(
        NAME,
        format!(
            "{detail}; ignored non-canonical file(s): {} — move their contents into \
             config.toml so they take effect",
            names.join(", ")
        ),
    )
}

/// Check 4: Claude Code transcript discoverability for the current cwd, plus a
/// parse-test of the newest transcript's first record. The adapter tailer reads
/// these `~/.claude/projects/<slug>/*.jsonl` files; a missing or unparseable
/// transcript is the direct cause of a session that finalizes `ok` with 0
/// steps. Read-only with respect to the user's data (opens the transcript, does
/// not write anything).
fn check_claude_jsonl_discoverable(cwd: &std::path::Path) -> DoctorCheck {
    const NAME: &str = "claude jsonl discoverable";
    use std::io::BufRead;
    let Some(projects) = hh_core::adapter::claude_projects_dir() else {
        return DoctorCheck::fail(
            NAME,
            "HOME/USERPROFILE not set — cannot locate ~/.claude/projects",
        );
    };
    if !projects.is_dir() {
        return DoctorCheck::fail(
            NAME,
            format!(
                "{} not found — no Claude Code transcripts on this machine",
                projects.display()
            ),
        );
    }
    let Some(transcript) = hh_core::adapter::newest_jsonl_for_cwd(cwd) else {
        return DoctorCheck::fail(
            NAME,
            format!(
                "no transcript found for cwd {} under {} — run a Claude Code session \
                 from this directory first",
                cwd.display(),
                projects.display()
            ),
        );
    };
    let file = match std::fs::File::open(&transcript) {
        Ok(f) => f,
        Err(e) => {
            return DoctorCheck::fail(
                NAME,
                format!("found {} but could not open it: {e}", transcript.display()),
            )
        }
    };
    let mut first_line = String::new();
    if let Err(e) = std::io::BufReader::new(file).read_line(&mut first_line) {
        return DoctorCheck::fail(
            NAME,
            format!("could not read {}: {e}", transcript.display()),
        );
    }
    match serde_json::from_str::<serde_json::Value>(first_line.trim()) {
        Ok(v) => match v.get("type").and_then(|t| t.as_str()) {
            Some(ty) => DoctorCheck::pass(
                NAME,
                format!("{} (newest record type: {ty})", transcript.display()),
            ),
            None => DoctorCheck::fail(
                NAME,
                format!(
                    "{} first line parsed as JSON but has no `type` field — \
                     unexpected transcript format",
                    transcript.display()
                ),
            ),
        },
        Err(e) => DoctorCheck::fail(
            NAME,
            format!("{} first line is not valid JSON: {e}", transcript.display()),
        ),
    }
}

/// Check 5: watcher smoke test — confirms `notify` delivers a file-change event
/// on this platform. Delegated to the recorder layer so the binary needs no new
/// `notify`/`tempfile` dependency (CLAUDE.md: state why a dependency is added;
/// here none is, by reusing `hh_record`).
fn check_watcher_smoke() -> DoctorCheck {
    const NAME: &str = "watcher smoke test";
    match hh_record::watcher_smoke_test() {
        Ok(()) => DoctorCheck::pass(NAME, "file-change event delivered"),
        Err(reason) => DoctorCheck::fail(NAME, reason),
    }
}

/// `hh gc` (Area 3): reclaim space. Prunes orphaned blob files and stale
/// `blobs` rows (files leaked by a crash between `BlobStore::put` and the
/// referencing event's commit, or `blobs` rows whose backing file was deleted
/// out of band), then `VACUUM`s `hh.db` to shrink it on disk (skippable with
/// `--no-vacuum`). Safe: it only removes blobs no live event references, never
/// referenced data. Reports what was reclaimed.
fn gc_command(args: &cli::GcArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, _config) = open_store()?;
    let prune = store
        .prune_orphan_blobs()
        .map_err(|e| anyhow::anyhow!("could not prune orphan blobs\n  why: {e}"))?;
    let vacuumed = if args.no_vacuum {
        false
    } else {
        store.vacuum().map_err(|e| {
            anyhow::anyhow!(
                "could not vacuum the database\n  why: {e}\n  hint: ensure no other `hh` process is using the data dir, or re-run with `hh gc --no-vacuum`"
            )
        })?;
        true
    };
    if args.json {
        let obj = serde_json::json!({
            "schema": inspect::SCHEMA_VERSION,
            "orphan_files_removed": prune.orphan_files_removed,
            "orphan_bytes_reclaimed": prune.orphan_bytes_reclaimed,
            "orphan_rows_removed": prune.orphan_rows_removed,
            "vacuumed": vacuumed,
        });
        println!("{obj}");
    } else {
        print!("{}", render_gc_plain(&prune, vacuumed, render::use_color()));
    }
    Ok(ExitCode::SUCCESS)
}

/// Render the `hh gc` outcome as plain lines (Area 3). Factored out so a
/// snapshot test can lock the human-readable output without a TTY or real data
/// dir. `color=false` keeps the snapshot deterministic.
fn render_gc_plain(prune: &hh_core::PruneStats, vacuumed: bool, color: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let check = if color {
        "✓".green().to_string()
    } else {
        "✓".to_string()
    };
    let dot = if color {
        "●".dimmed().to_string()
    } else {
        "●".to_string()
    };
    let _ = writeln!(
        out,
        "{check} Pruned {files} orphan blob file(s) · {bytes} reclaimed · {rows} stale row(s) removed",
        files = prune.orphan_files_removed,
        bytes = render::humanize_bytes(prune.orphan_bytes_reclaimed),
        rows = prune.orphan_rows_removed,
    );
    if vacuumed {
        let _ = writeln!(
            out,
            "{check} Vacuumed the database (hh.db compacted on disk)"
        );
    } else {
        let _ = writeln!(
            out,
            "{dot} Skipped vacuum (--no-vacuum; run `hh gc` without it to compact)"
        );
    }
    out
}

/// `hh stats` (Area 3): a read-only store inventory — session/event/blob
/// counts, disk usage (the `hh.db` file plus its `-wal`/`-shm` sidecars and the
/// compressed blob directory), and the largest sessions by event count.
fn stats_command(args: &cli::StatsArgs) -> anyhow::Result<ExitCode> {
    let (store, paths, _config) = open_store()?;
    let stats = store
        .store_stats(args.top)
        .map_err(|e| anyhow::anyhow!("could not gather store stats\n  why: {e}"))?;
    if args.json {
        let largest: Vec<serde_json::Value> = stats
            .largest_sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "short_id": s.short_id,
                    "events": s.event_count,
                })
            })
            .collect();
        let obj = serde_json::json!({
            "schema": inspect::SCHEMA_VERSION,
            "sessions": stats.sessions,
            "events": stats.events,
            "blobs": {
                "count": stats.blobs_count,
                "uncompressed_bytes": stats.blobs_uncompressed_bytes,
                "on_disk_bytes": stats.blobs_dir_bytes,
            },
            "disk": {
                "db_bytes": stats.db_bytes,
                "wal_bytes": stats.wal_bytes,
                "shm_bytes": stats.shm_bytes,
                "blobs_dir_bytes": stats.blobs_dir_bytes,
                "total_bytes": stats.db_bytes + stats.wal_bytes + stats.shm_bytes + stats.blobs_dir_bytes,
            },
            "largest_sessions": largest,
        });
        println!("{obj}");
    } else {
        print!(
            "{}",
            render_stats_plain(&stats, &paths.data_dir, render::use_color())
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Render the `hh stats` inventory as a plain key/value summary (Area 3).
/// Factored out for a snapshot test. Only the data-dir path and the
/// largest-sessions short ids (fixed 6-char width) carry the accent, matching
/// `hh list`'s "color the id, not the numbers" convention so column alignment
/// stays stable under color.
fn render_stats_plain(
    stats: &hh_core::StoreStats,
    data_dir: &std::path::Path,
    color: bool,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let dir = if color {
        data_dir.display().to_string().cyan().to_string()
    } else {
        data_dir.display().to_string()
    };
    let _ = writeln!(out, "hh stats — {dir}");
    let _ = writeln!(out);

    let total_disk = stats.db_bytes + stats.wal_bytes + stats.shm_bytes + stats.blobs_dir_bytes;
    let rows: [(&str, String); 4] = [
        ("sessions", stats.sessions.to_string()),
        ("events", stats.events.to_string()),
        (
            "blobs",
            format!(
                "{} ({} on disk, {} uncompressed)",
                stats.blobs_count,
                render::humanize_bytes(stats.blobs_dir_bytes),
                render::humanize_bytes(stats.blobs_uncompressed_bytes),
            ),
        ),
        (
            "disk",
            format!(
                "hh.db {} · WAL {} · blobs {} · total {}",
                render::humanize_bytes(stats.db_bytes),
                render::humanize_bytes(stats.wal_bytes),
                render::humanize_bytes(stats.blobs_dir_bytes),
                render::humanize_bytes(total_disk),
            ),
        ),
    ];
    for (label, value) in &rows {
        let _ = writeln!(out, "{label:<8}  {value}");
    }

    if !stats.largest_sessions.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "largest sessions");
        let w = stats
            .largest_sessions
            .iter()
            .map(|s| s.event_count.to_string().len())
            .max()
            .unwrap_or(0);
        for s in &stats.largest_sessions {
            let sid = if color {
                s.short_id.cyan().to_string()
            } else {
                s.short_id.clone()
            };
            let _ = writeln!(out, "  {sid}  {:>w$} events", s.event_count, w = w);
        }
    }
    out
}

/// Print the one-line epilogue (FR-1.6):
/// `✓ Recorded session a1b2c3 · 4m32s · 42 steps · 7 files changed · hh replay a1b2c3`
///
/// Then, on stderr (so it never corrupts the agent's captured stdout), surface
/// adapter degradation (FR-1.5): the recorder carries a `degrade_reason` from the
/// tailer and prints it here — *after* the child exits and the terminal is
/// restored — rather than from the tailer thread mid-session, where the agent's
/// alternate-screen TUI would bury it. Also warns when an adapter-active
/// claude-code session recorded 0 steps over a nontrivial duration, the symptom
/// of a broken adapter (the tailer never found a transcript).
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

    // Adapter degradation: surface the carried reason on stderr (FR-1.5).
    if outcome.adapter_status == AdapterStatus::Degraded {
        if let Some(reason) = outcome.degrade_reason.as_deref() {
            eprintln!("hh: warning: adapter degraded: {reason}");
        } else {
            eprintln!("hh: warning: adapter degraded; run `hh doctor` to diagnose");
        }
    }

    // 0-steps guard: a claude-code session that ran an adapter but captured no
    // steps over >60 s almost certainly means the adapter never found the
    // transcript (the original silent-breakage symptom). Tell the user plainly.
    if outcome.agent_kind == AgentKind::ClaudeCode
        && outcome.adapter_status != AdapterStatus::None
        && outcome.steps == 0
        && outcome.duration_ms > 60_000
    {
        eprintln!(
            "hh: warning: recorded 0 steps for a claude-code session over {} \
             — the adapter may be broken; run `hh doctor`",
            render::humanize_ms(outcome.duration_ms)
        );
    }
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
    let id = resolve_session_arg(&store, &args.session)?;
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
/// state (ok=green ✓, error=red ✗, interrupted=yellow ●, recording=cyan ●) plus
/// a `⚠` marker when the adapter ended degraded, via the shared
/// [`render::render_status_with_adapter`] so `hh list` and `hh inspect` speak
/// the same visual language.
fn print_session_table(rows: &[hh_core::SessionRow]) {
    if rows.is_empty() {
        println!("no sessions recorded yet — run `hh run -- <command>` to record one");
        return;
    }
    print!(
        "{}",
        format_session_table(rows, render::use_color(), render::now_unix_ms())
    );
}

/// Pure formatter behind [`print_session_table`], factored out so a snapshot
/// test can lock the alignment (headers lined up with data, degraded marker
/// present) without depending on a TTY or wall-clock.
fn format_session_table(rows: &[hh_core::SessionRow], color: bool, now: i64) -> String {
    // Build cell strings, then column-align by *visible* width. Status carries
    // a glyph (+ ⚠ when the adapter degraded, so the row is visibly not clean).
    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    for r in rows {
        let status = render::render_status_with_adapter(r.status, r.adapter_status, color);
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
    let last = headers.len() - 1;

    let mut out = String::new();
    for (i, h) in headers.iter().enumerate() {
        if i == last {
            out.push_str(h);
        } else {
            pad_into(&mut out, h, widths[i]);
        }
    }
    out.push('\n');
    for (i, &w) in widths.iter().enumerate() {
        for _ in 0..w {
            out.push('-');
        }
        if i != last {
            out.push(' ');
        }
    }
    out.push('\n');
    for l in &lines {
        pad_into(&mut out, &l.id, widths[0]);
        pad_into(&mut out, &l.status, widths[1]);
        pad_into(&mut out, &l.agent, widths[2]);
        pad_into(&mut out, &l.started, widths[3]);
        pad_into(&mut out, &l.duration, widths[4]);
        pad_into(&mut out, &l.steps, widths[5]);
        pad_into(&mut out, &l.files, widths[6]);
        out.push_str(&l.command);
        out.push('\n');
    }
    out
}

/// A row's rendered cells for the list table.
struct Line {
    /// Short id (colored if enabled).
    id: String,
    /// `glyph status [⚠]` (colored if enabled).
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

/// Return the column widths for the list table: `max(header width, max cell
/// visible width)` per column. Widths are measured by [`render::visible_width`]
/// (ANSI escapes stripped) so colored cells do not push columns out of
/// alignment. The COMMAND column is left unbounded (last column, no padding).
fn compute_widths(headers: &[&str; 8], lines: &[Line]) -> [usize; 8] {
    let cell = |l: &Line, i: usize| -> usize {
        match i {
            0 => render::visible_width(&l.id),
            1 => render::visible_width(&l.status),
            2 => render::visible_width(&l.agent),
            3 => render::visible_width(&l.started),
            4 => render::visible_width(&l.duration),
            5 => render::visible_width(&l.steps),
            6 => render::visible_width(&l.files),
            _ => render::visible_width(&l.command),
        }
    };
    let mut widths = [0usize; 8];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.chars().count();
        for l in lines {
            widths[i] = widths[i].max(cell(l, i));
        }
    }
    widths
}

/// Append `s` to `out` left-justified in `width` plus one trailing space, padding
/// by *visible* width so embedded ANSI escapes do not eat the padding.
fn pad_into(out: &mut String, s: &str, width: usize) {
    out.push_str(s);
    let pad = width.saturating_sub(render::visible_width(s));
    for _ in 0..pad {
        out.push(' ');
    }
    out.push(' ');
}

/// Build the JSON object for one session row (`hh list --json`, FR-5.1).
fn session_to_json(r: &hh_core::SessionRow) -> serde_json::Value {
    let duration_ms = r.ended_at.map(|end| (end - r.started_at).max(0));
    serde_json::json!({
        "schema": inspect::SCHEMA_VERSION,
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
        "imported_from": r.imported_from,
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

#[cfg(test)]
mod tests {
    use super::*;
    use hh_core::{AdapterStatus, AgentKind, SessionRow, SessionStatus};
    use std::path::PathBuf;

    fn row(short: &str, status: SessionStatus, adapter: AdapterStatus, steps: i64) -> SessionRow {
        SessionRow {
            id: format!("00000000-0000-7000-8000-{short}"),
            short_id: short.to_string(),
            // 2026-07-10T12:00:00Z → a stable "started" so relative time is
            // deterministic only if `now` is pinned; we pass a fixed `now`.
            started_at: 1_782_052_800_000,
            ended_at: Some(1_782_053_120_000),
            exit_code: Some(0),
            status,
            agent_kind: AgentKind::ClaudeCode,
            adapter_status: adapter,
            command: vec!["claude".into()],
            cwd: PathBuf::from("/tmp/work"),
            step_count: steps,
            imported_from: None,
            files_changed: 7,
        }
    }

    #[test]
    fn visible_width_strips_ansi_escapes() {
        // Plain text counts one per char.
        assert_eq!(render::visible_width("hello"), 5);
        assert_eq!(render::visible_width("✓ ok"), 4);
        // ANSI-colored text measures its visible width, not byte length.
        let colored = "\x1b[32m✓ ok\x1b[0m";
        assert_eq!(render::visible_width(colored), 4, "escapes must not count");
        assert_ne!(colored.len(), 4);
    }

    #[test]
    fn session_table_aligns_headers_and_marks_degraded() {
        // Two claude-code sessions: one clean, one whose adapter degraded. The
        // snapshot locks (a) headers lining up with row data and (b) the ⚠
        // marker on the degraded row. color=false → no ANSI escapes, so the
        // snapshot is deterministic and also exercises the pipe-safe path.
        let rows = [
            row("a1b2c3", SessionStatus::Ok, AdapterStatus::Active, 42),
            row("d4e5f6", SessionStatus::Ok, AdapterStatus::Degraded, 0),
        ];
        let table = format_session_table(&rows, false, 1_782_052_810_000);
        insta::assert_snapshot!(table);
        // Sanity: the degraded row carries the warning glyph; the clean row does not.
        let lines: Vec<&str> = table.lines().collect();
        assert!(
            lines.iter().any(|l| l.contains("⚠")),
            "degraded row must show ⚠"
        );
        assert!(
            lines.iter().any(|l| l.contains("✓ ok") && !l.contains("⚠")),
            "clean row must show ✓ ok without ⚠"
        );
    }

    #[test]
    fn session_table_aligns_under_color() {
        // The alignment bug: with byte-length padding, ANSI escapes in colored
        // cells inflated their "width" and shifted every column right of them out
        // of line with the (plain) headers. visible-width padding fixes it. This
        // snapshot (color=true, deterministic ANSI from owo_colors) locks that the
        // header row and the data rows align column-for-column despite escapes.
        let rows = [
            row("a1b2c3", SessionStatus::Ok, AdapterStatus::Active, 42),
            row("d4e5f6", SessionStatus::Ok, AdapterStatus::Degraded, 0),
        ];
        let table = format_session_table(&rows, true, 1_782_052_810_000);
        insta::assert_snapshot!(table);
        // Column alignment under color: the STATUS header and each row's status
        // glyph start at the same byte offset once escapes are stripped. Measure
        // visible offsets of "STATUS"/"✓" in each line.
        let vis_offset = |line: &str, needle: &str| -> Option<usize> {
            Some(render::visible_width(line.split(needle).next()?))
        };
        let lines: Vec<&str> = table.lines().collect();
        let hdr = vis_offset(lines[0], "STATUS");
        let r1 = vis_offset(lines[2], "✓");
        let r2 = vis_offset(lines[3], "✓");
        assert_eq!(hdr, r1, "header STATUS column must align with row status");
        assert_eq!(hdr, r2, "header STATUS column must align with degraded row");
    }

    /// `hh doctor` renders one `✓`/`✗ name — detail` line per check, color=false
    /// so the snapshot is deterministic and pipe-safe. Locks both glyphs and the
    /// "name — detail" shape across the five checks.
    #[test]
    fn doctor_plain_renders_pass_and_fail_lines() {
        let checks = [
            DoctorCheck::pass(
                "data dir writable",
                "/home/me/.local/share/halfhand is writable",
            ),
            DoctorCheck::pass("database integrity", "PRAGMA integrity_check: ok"),
            DoctorCheck::fail(
                "config resolution",
                "config: /home/me/.config/halfhand/config.toml (absent — defaults in effect); \
                 ignored non-canonical file(s): /home/me/.config/halfhand/halfhand.toml \
                 — move their contents into config.toml so they take effect",
            ),
            DoctorCheck::pass(
                "claude jsonl discoverable",
                "/home/me/.claude/projects/-home-me-work/abc.jsonl (newest record type: user)",
            ),
            DoctorCheck::pass("watcher smoke test", "file-change event delivered"),
        ];
        let out = render_doctor_plain(&checks, false);
        insta::assert_snapshot!(out);
        // Sanity: four passes and one fail glyph, no ANSI escapes.
        assert_eq!(out.matches('✓').count(), 4, "four passing checks");
        assert_eq!(out.matches('✗').count(), 1, "one failing check");
        assert!(!out.contains('\x1b'), "color=false must emit no escapes");
    }

    /// `hh doctor --json` builds one object per check with stable fields.
    #[test]
    fn doctor_check_to_json_shape() {
        let pass = DoctorCheck::pass("watcher smoke test", "file-change event delivered");
        let fail = DoctorCheck::fail("database integrity", "corrupt");
        assert_eq!(doctor_check_to_json(&pass)["status"], "ok");
        assert_eq!(doctor_check_to_json(&fail)["status"], "fail");
        assert_eq!(doctor_check_to_json(&fail)["name"], "database integrity");
    }

    /// `hh gc` plain output: a pruned line + a vacuum line (color=false for a
    /// deterministic snapshot), and the `--no-vacuum` skip line on the second
    /// variant. Locks the glyphs and the humanized bytes the user sees.
    #[test]
    fn gc_plain_renders_prune_and_vacuum() {
        let prune = hh_core::PruneStats {
            orphan_files_removed: 3,
            orphan_bytes_reclaimed: 1_572_864, // 1.5 MiB
            orphan_rows_removed: 2,
        };
        let out = render_gc_plain(&prune, true, false);
        insta::assert_snapshot!(out);
        assert!(out.contains("Pruned 3 orphan blob file(s)"));
        assert!(
            out.contains("1.5 MiB"),
            "bytes must humanize to 1.5 MiB: {out}"
        );
        assert!(out.contains("Vacuumed"));
        // --no-vacuum path shows the skip line instead.
        let out2 = render_gc_plain(&prune, false, false);
        assert!(out2.contains("Skipped vacuum"));
    }

    /// `hh stats` plain output: counts, disk usage, and the largest-sessions
    /// list (color=false for a deterministic snapshot). Locks the layout and the
    /// humanized bytes the user sees.
    #[test]
    fn stats_plain_renders_counts_and_largest() {
        let stats = hh_core::StoreStats {
            sessions: 2,
            events: 5,
            blobs_count: 1,
            blobs_uncompressed_bytes: 14,
            db_bytes: 3_145_728, // 3.0 MiB
            wal_bytes: 0,
            shm_bytes: 32_768,      // 32.0 KiB
            blobs_dir_bytes: 1_536, // 1.5 KiB
            largest_sessions: vec![
                hh_core::LargestSession {
                    id: "00000000-0000-7000-8000-000000a1b2c3".into(),
                    short_id: "a1b2c3".into(),
                    event_count: 4,
                },
                hh_core::LargestSession {
                    id: "00000000-0000-7000-8000-0000000d4e5f6".into(),
                    short_id: "d4e5f6".into(),
                    event_count: 1,
                },
            ],
        };
        let out = render_stats_plain(
            &stats,
            std::path::Path::new("/home/me/.local/share/halfhand"),
            false,
        );
        insta::assert_snapshot!(out);
        assert!(
            out.contains("a1b2c3"),
            "largest session short id appears: {out}"
        );
        assert!(out.contains("largest sessions"));
        assert!(out.contains("4 events"));
        assert!(
            out.contains("3.0 MiB"),
            "db bytes humanize to 3.0 MiB: {out}"
        );
        assert!(!out.contains('\x1b'), "color=false must emit no escapes");
    }

    /// `humanize_bytes` covers plain bytes and each binary unit without f64.
    #[test]
    fn humanize_bytes_units() {
        assert_eq!(render::humanize_bytes(0), "0 B");
        assert_eq!(render::humanize_bytes(1023), "1023 B");
        assert_eq!(render::humanize_bytes(1024), "1.0 KiB");
        assert_eq!(render::humanize_bytes(1_572_864), "1.5 MiB");
        assert_eq!(render::humanize_bytes(3_145_728), "3.0 MiB");
        assert_eq!(render::humanize_bytes(1u64 << 30), "1.0 GiB");
        assert_eq!(render::humanize_bytes(1u64 << 40), "1.0 TiB");
    }
}
