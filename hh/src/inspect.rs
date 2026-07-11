//! `hh inspect` — non-interactive session detail (SRS FR-4).
//!
//! `hh inspect <session>` prints a summary view (header + one line per step).
//! Flags select other views, all of which honor `NO_COLOR` / non-TTY (plain,
//! pipe-safe per CLAUDE.md):
//!
//! - `--step N` — full detail of step N (TTY-colored, pipe-plain).
//! - `--json` — stable, documented JSON (`schema: 1`). Without `--step`:
//!   NDJSON, one object per event for the whole session. With `--step N`: a
//!   single object for that step. See `docs/json.md` for every field.
//! - `--diff` — concatenated unified diff of the session's file changes.
//! - `--diff --step N` — diff for just step N.
//! - `--failed` — jump to the first error/failed step (acts like `--step N`
//!   once found).
//!
//! `--json` and `--diff` are mutually exclusive output formats.

use crate::cli;
use crate::render;
use hh_core::{
    build_timeline, ChangeKind, EventDetail, EventIndexRow, EventKind, FileChange, SessionRow,
    StepEntry, Store, TimelineRow,
};
use owo_colors::OwoColorize;
use similar::{ChangeTag, TextDiff};
use std::fmt::Write as _;

/// JSON schema version emitted by every `hh` JSON object — both the session
/// objects in `hh list --json` and the event/step objects in `hh inspect
/// --json` (FR-4 / FR-5.1). Consumers should gate on this before reading
/// fields; see `docs/json.md`.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// Internal bookkeeping fields stripped from `body` before any human or JSON
/// rendering (FR-1.5's `correlate_key` is resolved to `events.correlates`
/// before storage and is not meaningful to a reader). Mirrors the replay
/// detail pane's `HIDDEN_KEYS`.
const HIDDEN_BODY_KEYS: &[&str] = &["correlate_key"];

/// `hh inspect` (FR-4). Resolves the session, builds the step timeline, and
/// dispatches to the requested view.
pub(crate) fn inspect_command(args: &cli::InspectArgs) -> anyhow::Result<std::process::ExitCode> {
    if args.json && args.diff {
        anyhow::bail!(
            "`hh inspect --json` and `--diff` are mutually exclusive\n  \
             why: they are different output formats\n  \
             hint: pick one — `--json` for machine consumption, `--diff` for a human-readable diff"
        );
    }
    let (store, _paths, _config) = open_store_for_inspect()?;
    let hint = args.session.as_deref().unwrap_or("last");
    let id = store
        .resolve_session(hint)
        .map_err(|e| anyhow::anyhow!("could not resolve session `{hint}`\n  why: {e}"))?;
    let session = store
        .get_session(&id)
        .map_err(|e| anyhow::anyhow!("could not load session\n  why: {e}"))?;

    // Fast path (Area 2 big-session hardening): `hh inspect --json` with no
    // `--step` and no `--failed` streams the whole session as NDJSON straight
    // from the DB cursor in constant memory — it never materializes the event
    // index or builds the step timeline, which for a 100k-event session would
    // hold every row in RAM at once. The stream is byte-identical to the
    // index-based path (same `(ts_ms, id)` order, same `event_to_json`), just
    // O(1) memory instead of O(n).
    if args.json && args.step.is_none() && !args.failed {
        print_session_ndjson_stream(&store, &session)?;
        return Ok(std::process::ExitCode::SUCCESS);
    }

    let index = store
        .list_event_index(&id)
        .map_err(|e| anyhow::anyhow!("could not load session events\n  why: {e}"))?;
    let timeline = build_timeline(&index, false);

    // `--failed` resolves to a step ordinal, then behaves like `--step N`.
    let step = if args.failed {
        let Some(n) = find_failed_step(&store, &timeline)? else {
            let color = render::use_color();
            let prefix = if color {
                "●".cyan().to_string()
            } else {
                "●".to_string()
            };
            println!(
                "{prefix} no failed steps in session {sid} ({steps} steps)",
                sid = session.short_id,
                steps = session.step_count,
            );
            return Ok(std::process::ExitCode::SUCCESS);
        };
        Some(n)
    } else {
        args.step.map(u64::from)
    };

    let color = render::use_color();
    if args.json {
        // The no-step `--json` case is streamed by the fast path above;
        // reaching here means `--step N` or `--failed` set a step.
        let n = step.ok_or_else(|| {
            anyhow::anyhow!(
                "`hh inspect --json` reached the step path with no step (this is a bug)"
            )
        })?;
        print_step_json(&store, &session, &timeline, n)?;
    } else if args.diff {
        if let Some(n) = step {
            let entry = require_step(&timeline, n, &session)?;
            print_diff(&store, &entry, color);
        } else {
            print_diff_session(&store, &index, color);
        }
    } else if let Some(n) = step {
        let entry = require_step(&timeline, n, &session)?;
        print_step_detail(&store, &entry, color);
    } else {
        print_summary(&session, &timeline, color);
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Open the store with the same config/FR-1.7 reconciliation as every other
/// subcommand. Kept local so `inspect` does not depend on a private main-rs
/// helper.
fn open_store_for_inspect() -> anyhow::Result<(Store, hh_core::config::Paths, hh_core::Config)> {
    use hh_core::config::{Config, Paths};
    let paths0 = Paths::resolve(&Config::default())
        .map_err(|e| anyhow::anyhow!("could not resolve data directory\n  why: {e}\n  hint: set HH_DATA_DIR to a writable directory"))?;
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
    if let Err(e) = store.mark_stale_interrupted() {
        eprintln!("hh: warning: could not reconcile stale sessions: {e}");
    }
    Ok((store, paths, config))
}

/// Look up a step entry by its 1-based ordinal, or error with an actionable
/// message naming the valid range.
fn require_step(
    timeline: &[TimelineRow],
    n: u64,
    session: &SessionRow,
) -> anyhow::Result<StepEntry> {
    for row in timeline {
        if let TimelineRow::Step(s) = row {
            if u64::try_from(s.step).unwrap_or(0) == n {
                return Ok(s.clone());
            }
        }
    }
    let max = timeline
        .iter()
        .filter_map(|r| match r {
            TimelineRow::Step(s) => Some(s.step),
            TimelineRow::Terminal(_) => None,
        })
        .max()
        .unwrap_or(0);
    anyhow::bail!(
        "step {n} not found in session {sid}\n  \
         why: this session has {count} step(s) (1..={max})\n  \
         hint: run `hh inspect {sid}` to list every step",
        sid = session.short_id,
        count = session.step_count,
        max = max,
    );
}

/// Find the first failed step (FR-4 `--failed`): a step containing an `error`
/// event, or a `tool_result` whose body marks it `is_error: true`. Returns the
/// step ordinal, or `None` if the session has no failed step.
fn find_failed_step(store: &Store, timeline: &[TimelineRow]) -> anyhow::Result<Option<u64>> {
    for row in timeline {
        let TimelineRow::Step(s) = row else { continue };
        for &eid in &s.event_ids {
            let detail = store.get_event_detail(eid)?;
            if detail.kind == EventKind::Error {
                return Ok(Some(u64::try_from(s.step).unwrap_or(0)));
            }
            if detail.kind == EventKind::ToolResult && tool_result_is_error(&detail) {
                return Ok(Some(u64::try_from(s.step).unwrap_or(0)));
            }
        }
    }
    Ok(None)
}

/// Whether a `tool_result` event's body marks the result as an error. The
/// Claude Code adapter emits `{"is_error": true, ...}` for failed tool calls.
fn tool_result_is_error(detail: &EventDetail) -> bool {
    detail
        .body_json
        .as_ref()
        .and_then(|v| v.get("is_error"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Human output (summary + step detail)
// ---------------------------------------------------------------------------

/// The short badge label for an event kind, shared with the replay TUI's
/// `kind::badge_label` (FR-3.2: six accent categories + two quiet ones).
fn badge_label(kind: EventKind) -> &'static str {
    match kind {
        EventKind::AgentMessage | EventKind::Thinking => "AGENT",
        EventKind::UserMessage => "USER",
        EventKind::ToolCall | EventKind::ToolResult => "TOOL",
        EventKind::McpRequest | EventKind::McpResponse | EventKind::McpNotification => "MCP",
        EventKind::FileChange => "FILE",
        EventKind::Error => "ERR",
        EventKind::TerminalOutput => "TERM",
        EventKind::Lifecycle => "LIFE",
    }
}

/// Color the badge string for stdout (owo-colors), matching the replay
/// palette: AGENT cyan, USER green, TOOL yellow, MCP magenta, FILE blue, ERR
/// red; TERM/LIFE are dimmed (not "steps").
fn color_badge(label: &str, kind: EventKind) -> String {
    match kind {
        EventKind::AgentMessage | EventKind::Thinking => label.cyan().to_string(),
        EventKind::UserMessage => label.green().to_string(),
        EventKind::ToolCall | EventKind::ToolResult => label.yellow().to_string(),
        EventKind::McpRequest | EventKind::McpResponse | EventKind::McpNotification => {
            label.magenta().to_string()
        }
        EventKind::FileChange => label.blue().to_string(),
        EventKind::Error => label.red().to_string(),
        EventKind::TerminalOutput | EventKind::Lifecycle => label.dimmed().to_string(),
    }
}

/// Render and print the summary view (FR-4): a header line (matching the
/// replay header) followed by one aligned row per step.
fn print_summary(session: &SessionRow, timeline: &[TimelineRow], color: bool) {
    print!("{}", render_summary_header(session, color));
    print!("{}", render_step_table(timeline, color));
}

/// The summary header: ` a1b2c3 ✓ ok · claude-code · 4m32s · 42 steps · 7 files changed`
/// then a dimmed command line. Matches the replay TUI header (NFR-7).
fn render_summary_header(session: &SessionRow, color: bool) -> String {
    let duration = match session.ended_at {
        Some(end) => render::humanize_ms((end - session.started_at).max(0)),
        None => "—".to_string(),
    };
    let status = render::render_status(session.status, color);
    let id = if color {
        session.short_id.green().to_string()
    } else {
        session.short_id.clone()
    };
    let line1 = format!(
        " {id} {status} · {agent} · {dur} · {steps} steps · {files} files changed\n",
        agent = session.agent_kind,
        dur = duration,
        steps = session.step_count,
        files = session.files_changed,
    );
    let command = if color {
        session.command.join(" ").dimmed().to_string()
    } else {
        session.command.join(" ")
    };
    format!("{line1} {command}\n\n")
}

/// The per-step table: a header row, a separator, and one row per step with a
/// right-aligned step number, a padded badge, the summary, and an `HH:MM:SS`
/// timestamp. Terminal/lifecycle rows are omitted (this view is steps-only).
fn render_step_table(timeline: &[TimelineRow], color: bool) -> String {
    let mut out = String::new();
    let header = format!(
        " {step:<5} {badge:<5} {summary:<40} {time}\n",
        step = "STEP",
        badge = "KIND",
        summary = "SUMMARY",
        time = "TIME",
    );
    out.push_str(&header);
    let _ = writeln!(
        out,
        " ----- ----- ---------------------------------------- --------",
    );
    for row in timeline {
        let TimelineRow::Step(s) = row else { continue };
        out.push_str(&render_step_row(s, color));
    }
    out
}

/// One row of the step table.
fn render_step_row(s: &StepEntry, color: bool) -> String {
    let label = badge_label(s.kind);
    let badge = if color {
        color_badge(&format!("{label:<5}"), s.kind)
    } else {
        format!("{label:<5}")
    };
    // The badge carries ANSI escapes when colored; pad the *visible* label to
    // 5 so the SUMMARY column lines up in both modes. The leading step number
    // is plain (no color) so it stays a stable, greppable anchor.
    let summary = render::truncate(&s.summary, 40);
    format!(
        " {step:<5} {badge} {summary:<40} {time}\n",
        step = s.step,
        badge = badge,
        time = render::format_hms(s.ts_ms),
    )
}

/// Print the full human-readable detail of one step (FR-4 `--step N`).
fn print_step_detail(store: &Store, entry: &StepEntry, color: bool) {
    print!("{}", render_step_detail(store, entry, color));
}

/// Render the detail of one step as plain/colored text: a header line, then a
/// section per event in the step (call before its result, in id order).
fn render_step_detail(store: &Store, entry: &StepEntry, color: bool) -> String {
    let details = match entry
        .event_ids
        .iter()
        .map(|&id| store.get_event_detail(id))
        .collect::<hh_core::Result<Vec<_>>>()
    {
        Ok(d) => d,
        Err(e) => return format!("error: {e}\n"),
    };
    let mut out = String::new();
    let label = badge_label(entry.kind);
    let badge = if color {
        color_badge(label, entry.kind)
    } else {
        label.to_string()
    };
    let _ = writeln!(
        out,
        "Step {n} · {badge} · {summary} · {time}",
        n = entry.step,
        badge = badge,
        summary = entry.summary,
        time = render::format_hms(entry.ts_ms),
    );
    out.push('\n');
    for d in &details {
        out.push_str(&render_event_block(d, color));
        out.push('\n');
    }
    out
}

/// Render one event as a labeled block within a step. Text events print their
/// `text` body; structured events print pretty JSON (hidden keys stripped);
/// file changes print a one-line summary (the diff is shown only via `--diff`);
/// errors print the summary in red plus the body.
fn render_event_block(d: &EventDetail, color: bool) -> String {
    let mut out = String::new();
    let label = badge_label(d.kind);
    let badge = if color {
        color_badge(label, d.kind)
    } else {
        label.to_string()
    };
    let _ = writeln!(out, "{badge} {summary}", summary = d.summary);
    match d.kind {
        // Text-bearing events share the indented-text rendering.
        EventKind::UserMessage
        | EventKind::AgentMessage
        | EventKind::Thinking
        | EventKind::Lifecycle
        | EventKind::TerminalOutput => {
            if let Some(text) = text_body(d) {
                out.push_str(&indent(&text));
                if !text.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        // Structured events print pretty JSON; `Error` shares this path (its
        // body is the error payload, rendered verbatim).
        EventKind::ToolCall
        | EventKind::McpRequest
        | EventKind::McpNotification
        | EventKind::Error => {
            if let Some(body) = &d.body_json {
                out.push_str(&pretty_json(body, color));
            }
        }
        EventKind::ToolResult | EventKind::McpResponse => {
            if let Some(body) = &d.body_json {
                let is_err = d.kind == EventKind::ToolResult && tool_result_is_error(d);
                let prefix = if is_err { "error: " } else { "" };
                let _ = write!(out, "{prefix}{}", pretty_json(body, color));
            }
        }
        EventKind::FileChange => {
            if let Some(fc) = &d.file_change {
                let binary = if fc.is_binary { " · binary" } else { "" };
                let _ = writeln!(
                    out,
                    "  {path} ({kind}){binary}",
                    path = fc.path,
                    kind = fc.change_kind
                );
            }
        }
    }
    out
}

/// Extract the `text` field from an event body (text-bearing events store
/// `{"text": "..."}`).
fn text_body(d: &EventDetail) -> Option<String> {
    d.body_json
        .as_ref()
        .and_then(|v| v.get("text"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Pretty-print a JSON value (2-space indent) with `correlate_key` stripped.
/// Indented two spaces; the plain form is unstyled, the colored form tints
/// keys/strings/punctuation (mirroring the replay detail pane).
fn pretty_json(value: &serde_json::Value, color: bool) -> String {
    let cleaned = strip_hidden_keys(value);
    let text = serde_json::to_string_pretty(&cleaned).unwrap_or_else(|_| cleaned.to_string());
    if !color {
        return indent(&text);
    }
    tint_json(&text)
}

/// Apply a simple ANSI tint to pretty-printed JSON text.
fn tint_json(text: &str) -> String {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let nl = if line.ends_with('\n') { "\n" } else { "" };
        out.push_str("  ");
        out.push_str(&tint_json_line(body));
        out.push_str(nl);
    }
    out
}

/// Tint one JSON line: keys cyan (a quoted string followed by `:`), other
/// strings green, punctuation dimmed. Numbers/booleans/null are left plain.
fn tint_json_line(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut s = String::from('"');
                while let Some(ch) = chars.next() {
                    s.push(ch);
                    if ch == '\\' {
                        if let Some(esc) = chars.next() {
                            s.push(esc);
                        }
                        continue;
                    }
                    if ch == '"' {
                        break;
                    }
                }
                let is_key = matches!(chars.peek(), Some(':'));
                if is_key {
                    out.push_str(&s.cyan().to_string());
                } else {
                    out.push_str(&s.green().to_string());
                }
            }
            ':' | '{' | '}' | '[' | ']' | ',' => {
                out.push_str(&c.dimmed().to_string());
            }
            _ => {
                out.push(c);
            }
        }
    }
    out
}

/// Indent every line of `s` by two spaces.
fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

// ---------------------------------------------------------------------------
// Diff output (`--diff`, `--diff --step N`)
// ---------------------------------------------------------------------------

/// Print the concatenated unified diff of every file change in the session,
/// in chronological order.
fn print_diff_session(store: &Store, index: &[EventIndexRow], color: bool) {
    let mut first = true;
    for e in index {
        if e.kind != EventKind::FileChange {
            continue;
        }
        if !first {
            println!();
        }
        first = false;
        match store.get_event_detail(e.id) {
            Ok(detail) => {
                if let Some(fc) = &detail.file_change {
                    print!("{}", render_file_change_diff(fc, store, color));
                }
            }
            Err(err) => {
                eprintln!("hh: warning: could not load file change: {err}");
            }
        }
    }
    if first {
        println!("no file changes in this session");
    }
}

/// Print the diff for the file changes within one step.
fn print_diff(store: &Store, entry: &StepEntry, color: bool) {
    let mut any = false;
    for &eid in &entry.event_ids {
        let detail = match store.get_event_detail(eid) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("hh: warning: could not load step event: {e}");
                continue;
            }
        };
        if let Some(fc) = &detail.file_change {
            if any {
                println!();
            }
            any = true;
            print!("{}", render_file_change_diff(fc, store, color));
        }
    }
    if !any {
        println!("step {n} has no file changes", n = entry.step);
    }
}

/// Render one file change as a git-style unified diff. Binary files and
/// creates/deletes with a missing side render a descriptive line instead of a
/// byte diff. `+`/`-` lines are tinted green/red when `color` is enabled.
fn render_file_change_diff(fc: &FileChange, store: &Store, color: bool) -> String {
    let mut out = String::new();
    let header = format!("diff -- a/{path} b/{path}\n", path = fc.path);
    out.push_str(&if color {
        header.cyan().to_string()
    } else {
        header
    });
    let (a_label, b_label) = match fc.change_kind {
        ChangeKind::Created => ("/dev/null".to_string(), format!("b/{path}", path = fc.path)),
        ChangeKind::Deleted => (format!("a/{path}", path = fc.path), "/dev/null".to_string()),
        ChangeKind::Modified => (
            format!("a/{path}", path = fc.path),
            format!("b/{path}", path = fc.path),
        ),
    };
    let sep = format!("--- {a_label}\n+++ {b_label}\n");
    out.push_str(&if color { sep.dimmed().to_string() } else { sep });

    if fc.is_binary {
        out.push_str("Binary file — diff not shown.\n");
        return out;
    }

    let before = fc
        .before_hash
        .as_deref()
        .and_then(|h| store.blobs().get(h).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    let after = fc
        .after_hash
        .as_deref()
        .and_then(|h| store.blobs().get(h).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned());

    if let (Some(a), Some(b)) = (before.as_deref(), after.as_deref()) {
        let diff = TextDiff::from_lines(a, b);
        let unified = diff.unified_diff();
        let mut any_hunk = false;
        for hunk in unified.iter_hunks() {
            any_hunk = true;
            let hdr = format!("{}\n", hunk.header());
            out.push_str(&if color {
                hdr.magenta().to_string()
            } else {
                hdr
            });
            for change in hunk.iter_changes() {
                let (prefix, line) = match change.tag() {
                    ChangeTag::Delete => ("-", change.to_string_lossy()),
                    ChangeTag::Insert => ("+", change.to_string_lossy()),
                    ChangeTag::Equal => (" ", change.to_string_lossy()),
                };
                let line = line.strip_suffix('\n').unwrap_or(&line);
                let rendered = format!("{prefix}{line}\n");
                out.push_str(&if color {
                    match change.tag() {
                        ChangeTag::Insert => rendered.green().to_string(),
                        ChangeTag::Delete => rendered.red().to_string(),
                        ChangeTag::Equal => rendered,
                    }
                } else {
                    rendered
                });
            }
        }
        if !any_hunk {
            out.push_str("(no changes)\n");
        }
    } else {
        // A create/delete has only one side, or the referenced blob was
        // never captured — show whichever side exists in full.
        let only = before.as_deref().or(after.as_deref());
        match only {
            Some(text) => {
                for line in text.lines() {
                    let prefix = if after.is_none() { "-" } else { "+" };
                    let rendered = format!("{prefix}{line}\n");
                    out.push_str(&if color {
                        if after.is_none() {
                            rendered.red().to_string()
                        } else {
                            rendered.green().to_string()
                        }
                    } else {
                        rendered
                    });
                }
            }
            None => out.push_str("(content not captured)\n"),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// JSON output (`--json`, `--json --step N`)
// ---------------------------------------------------------------------------

/// Stream the whole session as NDJSON (FR-4) to stdout: one JSON object per
/// event, in `(ts_ms, id)` order, on its own line, each carrying `schema: 1`.
/// Streams straight from the DB cursor via [`Store::for_each_event_detail`] —
/// peak memory is O(1) regardless of session size (Area 2), unlike the old
/// index-based path which materialized every event row.
fn print_session_ndjson_stream(store: &Store, session: &SessionRow) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    write_session_ndjson(store, session, &mut out)
}

/// Write the session's NDJSON stream to `out` (FR-4): one JSON object per event
/// in `(ts_ms, id)` order, one per line. Factored over a generic writer so a
/// snapshot test can capture the exact bytes the streaming path produces
/// without touching stdout, and so the stdout path and the test share one
/// implementation.
///
/// The emit closure returns `hh_core::Result`, but a write failure is an
/// `io::Error`. We stash it and stop iteration with a sentinel (`WriterClosed`
/// is never surfaced — it is just a way to break out of a callback whose error
/// type is not `anyhow`), then report the real cause here. A `BrokenPipe` (a
/// downstream consumer like `hh inspect --json | head` closed the pipe) stops
/// cleanly like a well-behaved CLI rather than printing a crash.
fn write_session_ndjson<W: std::io::Write>(
    store: &Store,
    session: &SessionRow,
    out: &mut W,
) -> anyhow::Result<()> {
    let mut io_err: Option<std::io::Error> = None;
    let stream_result = store.for_each_event_detail(&session.id, |detail| {
        let obj = event_to_json(&detail, session);
        if let Err(e) = writeln!(out, "{obj}") {
            io_err = Some(e);
            // Stop iteration; the stashed io error is reported below. The
            // sentinel is never surfaced to the user.
            return Err(hh_core::StorageError::WriterClosed.into());
        }
        Ok(())
    });
    if let Some(e) = io_err {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(anyhow::anyhow!("could not write inspect JSON\n  why: {e}"));
    }
    stream_result.map_err(|e| anyhow::anyhow!("could not stream session events\n  why: {e}"))?;
    Ok(())
}

/// Emit a single JSON object for one step (FR-4 `--json --step N`).
fn print_step_json(
    store: &Store,
    session: &SessionRow,
    timeline: &[TimelineRow],
    n: u64,
) -> anyhow::Result<()> {
    let obj = step_json_value(store, session, timeline, n)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&obj)
            .map_err(|e| anyhow::anyhow!("could not serialize step JSON\n  why: {e}\n  hint: this is an internal error — the recorded payload was not serializable; please report it"))?
    );
    Ok(())
}

/// Build the single JSON object for one step. Factored out for snapshot tests.
fn step_json_value(
    store: &Store,
    session: &SessionRow,
    timeline: &[TimelineRow],
    n: u64,
) -> anyhow::Result<serde_json::Value> {
    let entry = require_step(timeline, n, session)?;
    let events: Vec<serde_json::Value> = entry
        .event_ids
        .iter()
        .map(|&id| store.get_event_detail(id))
        .collect::<hh_core::Result<Vec<_>>>()
        .map_err(|e| anyhow::anyhow!("could not load step events\n  why: {e}"))?
        .iter()
        .map(|d| event_to_json(d, session))
        .collect();
    Ok(serde_json::json!({
        "schema": SCHEMA_VERSION,
        "session": session_ref(session),
        "step": entry.step,
        "kind": entry.kind.to_string(),
        "summary": entry.summary,
        "ts_ms": entry.ts_ms,
        "events": events,
    }))
}

/// The `session` reference embedded in every inspect JSON object.
fn session_ref(session: &SessionRow) -> serde_json::Value {
    serde_json::json!({
        "id": session.id,
        "short_id": session.short_id,
    })
}

/// Build the stable JSON object for one event (the unit of both the NDJSON
/// stream and a step object's `events` array). See `docs/json.md`.
fn event_to_json(d: &EventDetail, session: &SessionRow) -> serde_json::Value {
    let body = d.body_json.as_ref().map(strip_hidden_keys);
    let file_change = d.file_change.as_ref().map(file_change_to_json);
    serde_json::json!({
        "schema": SCHEMA_VERSION,
        "session": session_ref(session),
        "id": d.id,
        "ts_ms": d.ts_ms,
        "kind": d.kind.to_string(),
        "step": d.step,
        "correlates": d.correlates,
        "summary": d.summary,
        "body": body,
        "file_change": file_change,
    })
}

/// The JSON object for a `file_changes` row.
fn file_change_to_json(fc: &FileChange) -> serde_json::Value {
    serde_json::json!({
        "path": fc.path,
        "change_kind": fc.change_kind.to_string(),
        "before_hash": fc.before_hash,
        "after_hash": fc.after_hash,
        "is_binary": fc.is_binary,
    })
}

/// Recursively remove [`HIDDEN_BODY_KEYS`] from any object in `value` (mirrors
/// the replay detail pane's `strip_hidden_keys`).
fn strip_hidden_keys(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if HIDDEN_BODY_KEYS.contains(&k.as_str()) {
                    continue;
                }
                out.insert(k.clone(), strip_hidden_keys(v));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(strip_hidden_keys).collect())
        }
        other => other.clone(),
    }
}
#[cfg(test)]
mod tests {
    //! Snapshot tests for `hh inspect`'s human + JSON output (FR-4; CLAUDE.md:
    //! "Snapshot tests with insta for rendered output"). All rendering is
    //! driven with `color = false` so snapshots are deterministic and
    //! pipe-safe (the colored path is the same strings plus ANSI escapes).

    use super::*;
    use hh_core::{
        AdapterStatus, AgentKind, ChangeKind, Event, EventKind, FileChange, NewSession, Store,
    };
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A fixture session with a representative mix of events: a user prompt,
    /// a correlated tool_call + tool_result (one step), a created file change,
    /// an agent message, an error, and a terminal_output chunk (non-step).
    struct Fixture {
        _tmp: TempDir,
        store: Store,
        session: SessionRow,
        timeline: Vec<TimelineRow>,
        index: Vec<EventIndexRow>,
    }

    // A fixed UUIDv7 so the derived `short_id` (the random tail, see
    // `NewSession::short_id`) is stable across runs — insta snapshots would
    // otherwise drift every run on a fresh `now_v7()`. `simple[26..32]` of
    // this uuid is `a1b2c3`, which is what the snapshots assert.
    fn new_session() -> NewSession {
        let id = hh_core::Uuid::parse_str("018e2c5a-4a00-7000-8000-000000a1b2c3")
            .expect("hardcoded fixture uuid parses");
        NewSession {
            id,
            started_at: 1_700_000_000_000,
            agent_kind: AgentKind::ClaudeCode,
            adapter_status: AdapterStatus::Active,
            command: vec!["claude".into(), "--chat".into()],
            cwd: PathBuf::from("/tmp/proj"),
            hostname: Some("devbox".into()),
            hh_version: "0.1.0-beta.1".into(),
            model: Some("glm-5.2".into()),
            git_branch: Some("main".into()),
            git_sha: Some("deadbeef".into()),
            git_dirty: Some(false),
        }
    }

    impl Fixture {
        #[allow(clippy::too_many_lines)] // linear fixture script; splitting it would obscure the scenario
        fn build() -> Self {
            let tmp = TempDir::new().unwrap();
            let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs"))
                .expect("open store");
            let created = store.create_session(&new_session()).unwrap();
            let sid = created.id.clone();

            let writer = store.event_writer().unwrap();
            // User prompt → step 1.
            writer
                .append_event(Event {
                    session_id: sid.clone(),
                    ts_ms: 0,
                    kind: EventKind::UserMessage,
                    step: None,
                    summary: "list the files".into(),
                    body_json: Some(serde_json::json!({"text": "please list the files"})),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            // Tool call → step 2.
            let call_id = writer
                .append_event(Event {
                    session_id: sid.clone(),
                    ts_ms: 1_000,
                    kind: EventKind::ToolCall,
                    step: None,
                    summary: "tool_call: Bash".into(),
                    body_json: Some(serde_json::json!({
                        "name": "Bash",
                        "input": {"command": "ls -la"},
                        "correlate_key": "call_1",
                    })),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            // Tool result (correlated) → shares step 2.
            writer
                .append_event(Event {
                    session_id: sid.clone(),
                    ts_ms: 1_500,
                    kind: EventKind::ToolResult,
                    step: None,
                    summary: "tool_result: ok".into(),
                    body_json: Some(serde_json::json!({
                        "content": "file.txt",
                        "is_error": false,
                    })),
                    blob_hash: None,
                    blob_size: None,
                    correlates: Some(call_id),
                })
                .unwrap();
            // File change (created) → step 3. Store the after-content blob and
            // reference it via event.blob_hash so the writer bumps its refcount.
            let after = store.blobs().put(b"created-content\n").unwrap();
            writer
                .append_file_change(
                    Event {
                        session_id: sid.clone(),
                        ts_ms: 2_000,
                        kind: EventKind::FileChange,
                        step: None,
                        summary: "created.txt created".into(),
                        body_json: Some(serde_json::json!({
                            "path": "created.txt",
                            "change_kind": "created",
                        })),
                        blob_hash: Some(after.hash.clone()),
                        blob_size: Some(after.size),
                        correlates: None,
                    },
                    FileChange {
                        event_id: 0,
                        path: "created.txt".into(),
                        change_kind: ChangeKind::Created,
                        before_hash: None,
                        after_hash: Some(after.hash.clone()),
                        is_binary: false,
                    },
                )
                .unwrap();
            // Agent message → step 4.
            writer
                .append_event(Event {
                    session_id: sid.clone(),
                    ts_ms: 2_500,
                    kind: EventKind::AgentMessage,
                    step: None,
                    summary: "done listing".into(),
                    body_json: Some(serde_json::json!({"text": "here are the files"})),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            // Error → step 5.
            writer
                .append_event(Event {
                    session_id: sid.clone(),
                    ts_ms: 3_000,
                    kind: EventKind::Error,
                    step: None,
                    summary: "boom".into(),
                    body_json: Some(serde_json::json!({"message": "permission denied"})),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            // Terminal output → non-step.
            writer
                .append_event(Event {
                    session_id: sid.clone(),
                    ts_ms: 500,
                    kind: EventKind::TerminalOutput,
                    step: None,
                    summary: "terminal chunk".into(),
                    body_json: Some(serde_json::json!({"text": "$ ls\nfile.txt\n"})),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            writer.finish().unwrap();

            store.assign_steps(&sid).unwrap();
            // Finalize the session so the summary header has a duration.
            store
                .finalize_session(&sid, 1_700_000_003_200, Some(1), hh_core::SessionStatus::Ok)
                .unwrap();
            let session = store.get_session(&sid).unwrap();
            let index = store.list_event_index(&sid).unwrap();
            let timeline = build_timeline(&index, false);
            Self {
                _tmp: tmp,
                store,
                session,
                timeline,
                index,
            }
        }
    }

    #[test]
    fn snapshot_summary() {
        let fx = Fixture::build();
        let out = format!(
            "{}{}",
            render_summary_header(&fx.session, false),
            render_step_table(&fx.timeline, false)
        );
        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_step_detail_tool_pair() {
        let fx = Fixture::build();
        // Step 2 is the correlated tool_call + tool_result.
        let entry = require_step(&fx.timeline, 2, &fx.session).unwrap();
        let out = render_step_detail(&fx.store, &entry, false);
        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_step_detail_error() {
        let fx = Fixture::build();
        let entry = require_step(&fx.timeline, 5, &fx.session).unwrap();
        let out = render_step_detail(&fx.store, &entry, false);
        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_diff_created_file() {
        let fx = Fixture::build();
        // Find the file_change event and render its diff.
        let detail = fx
            .index
            .iter()
            .find(|e| e.kind == EventKind::FileChange)
            .and_then(|e| fx.store.get_event_detail(e.id).ok())
            .expect("file change event");
        let fc = detail.file_change.as_ref().expect("file_change row");
        let out = render_file_change_diff(fc, &fx.store, false);
        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_event_json_tool_call() {
        let fx = Fixture::build();
        let detail = fx
            .index
            .iter()
            .find(|e| e.kind == EventKind::ToolCall)
            .and_then(|e| fx.store.get_event_detail(e.id).ok())
            .expect("tool_call event");
        let obj = event_to_json(&detail, &fx.session);
        let pretty = serde_json::to_string_pretty(&obj).unwrap();
        insta::assert_snapshot!(pretty);
    }

    #[test]
    fn snapshot_step_json() {
        let fx = Fixture::build();
        let obj = step_json_value(&fx.store, &fx.session, &fx.timeline, 2).unwrap();
        let pretty = serde_json::to_string_pretty(&obj).unwrap();
        insta::assert_snapshot!(pretty);
    }

    #[test]
    fn snapshot_ndjson_stream() {
        let fx = Fixture::build();
        // Exercise the actual streaming path (`write_session_ndjson`) into a
        // buffer rather than the removed in-memory collector, so the snapshot
        // locks exactly what `hh inspect --json` emits — one object per line,
        // each terminated by a newline, in `(ts_ms, id)` order.
        let mut buf = Vec::new();
        write_session_ndjson(&fx.store, &fx.session, &mut buf).unwrap();
        let ndjson = String::from_utf8(buf).expect("NDJSON is UTF-8");
        insta::assert_snapshot!(ndjson);
        // Sanity: every line is a JSON object carrying schema:1, in order.
        for line in ndjson.lines() {
            let v: serde_json::Value =
                serde_json::from_str(line).expect("each NDJSON line is valid JSON");
            assert_eq!(v["schema"], 1, "every event object carries schema:1");
            assert!(v["kind"].is_string(), "every event object has a kind");
        }
    }

    #[test]
    fn failed_step_finds_the_error_event() {
        let fx = Fixture::build();
        let n = find_failed_step(&fx.store, &fx.timeline).unwrap();
        assert_eq!(n, Some(5));
    }

    #[test]
    fn failed_step_finds_an_error_tool_result() {
        // A tool_result with is_error: true is also a "failed step".
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let created = store.create_session(&new_session()).unwrap();
        let sid = created.id.clone();
        let writer = store.event_writer().unwrap();
        let call = writer
            .append_event(Event {
                session_id: sid.clone(),
                ts_ms: 0,
                kind: EventKind::ToolCall,
                step: None,
                summary: "tool_call: Bash".into(),
                body_json: Some(serde_json::json!({"name": "Bash", "input": {}})),
                blob_hash: None,
                blob_size: None,
                correlates: None,
            })
            .unwrap();
        writer
            .append_event(Event {
                session_id: sid.clone(),
                ts_ms: 10,
                kind: EventKind::ToolResult,
                step: None,
                summary: "tool_result: failed".into(),
                body_json: Some(serde_json::json!({"content": "boom", "is_error": true})),
                blob_hash: None,
                blob_size: None,
                correlates: Some(call),
            })
            .unwrap();
        writer.finish().unwrap();
        store.assign_steps(&sid).unwrap();
        let index = store.list_event_index(&sid).unwrap();
        let timeline = build_timeline(&index, false);
        let n = find_failed_step(&store, &timeline).unwrap();
        assert_eq!(n, Some(1));
    }

    #[test]
    fn require_step_errors_for_out_of_range() {
        let fx = Fixture::build();
        let err = require_step(&fx.timeline, 99, &fx.session).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("step 99 not found"), "{msg}");
        assert!(
            msg.contains("hint:"),
            "out-of-range error needs a hint: {msg}"
        );
    }
}
