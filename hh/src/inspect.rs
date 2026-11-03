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
//! - `--context` — the session context panel (model, provider, token usage,
//!   message counts, times, durations, estimated cost); with `--json`, one
//!   JSON object. See `docs/context.md`.
//!
//! `--json` and `--diff` are mutually exclusive output formats.

use crate::cli;
use crate::render;
use hh_core::{
    build_timeline, ChangeKind, EventDetail, EventIndexRow, EventKind, FileChange, ModelSpan,
    SessionContext, SessionRow, StepEntry, Store, TimelineRow,
};
use owo_colors::OwoColorize;
use similar::{ChangeTag, TextDiff};
use std::fmt::Write as _;

/// JSON schema version emitted by every `hh` JSON object — both the session
/// objects in `hh list --json` and the event/step objects in `hh inspect
/// --json` (FR-4 / FR-5.1). Consumers should gate on this before reading
/// fields; see `docs/json.md`. Aliases [`hh_core::JSON_SCHEMA_VERSION`], the
/// single counter shared by every JSON-emitting command (including
/// `hh-core::bundle`, which cannot depend on this binary crate).
pub(crate) const SCHEMA_VERSION: u64 = hh_core::JSON_SCHEMA_VERSION;

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
    let id = crate::resolve_session_arg(&store, hint)?;
    let session = store
        .get_session(&id)
        .map_err(|e| anyhow::anyhow!("could not load session\n  why: {e}"))?;

    if args.context {
        let ctx = load_context(&store, &session)?;
        if args.json {
            let obj = context_json(&session, &ctx);
            println!(
                "{}",
                serde_json::to_string_pretty(&obj)
                    .map_err(|e| anyhow::anyhow!("could not serialize context JSON\n  why: {e}"))?
            );
        } else {
            print!(
                "{}",
                render_context_panel(&session, &ctx, render::use_color())
            );
        }
        return Ok(std::process::ExitCode::SUCCESS);
    }

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
        let ctx = load_context(&store, &session)?;
        print_summary(&session, &ctx.models, &timeline, color);
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Read the session's derived context, with an actionable error.
fn load_context(store: &Store, session: &SessionRow) -> anyhow::Result<SessionContext> {
    store.session_context(&session.id).map_err(|e| {
        anyhow::anyhow!(
            "could not read the context of session {sid}\n  why: {e}\n  hint: run `hh doctor` to check the database",
            sid = session.short_id
        )
    })
}

/// Open the store with the same config/FR-1.7 reconciliation as every other
/// subcommand. Kept local so `inspect` does not depend on a private main-rs
/// helper.
fn open_store_for_inspect() -> anyhow::Result<(Store, hh_core::config::Paths, hh_core::Config)> {
    use hh_core::config::{Config, Paths};
    let paths0 = Paths::resolve(&Config::default())
        .map_err(|e| anyhow::anyhow!("could not resolve data directory\n  why: {e}\n  hint: set HH_DATA_DIR to a writable directory"))?;
    let (config, source) = Config::load_with_source(&paths0.config_path).unwrap_or_else(|e| {
        eprintln!(
            "hh: warning: could not load {}: {e}",
            paths0.config_path.display()
        );
        (Config::default(), hh_core::ConfigSource::Defaults)
    });
    let paths = Paths::resolve(&config).map_err(|e| {
        anyhow::anyhow!("could not resolve data directory\n  why: {e}\n  hint: set HH_DATA_DIR to a writable directory")
    })?;
    hh_core::print_legacy_config_hint_if_needed(&source, &paths0.config_path, &paths.data_dir);
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
fn print_summary(
    session: &SessionRow,
    models: &[ModelSpan],
    timeline: &[TimelineRow],
    color: bool,
) {
    print!("{}", render_summary_header(session, models, color));
    print!("{}", render_step_table(timeline, session, color));
}

/// The summary header: ` a1b2c3 ✓ ok · claude-code · 4m32s · 42 steps · 7 files changed`
/// then a dimmed command line. Matches the replay TUI header (NFR-7).
fn render_summary_header(session: &SessionRow, models: &[ModelSpan], color: bool) -> String {
    let duration = match session.ended_at {
        Some(end) => render::humanize_ms((end - session.started_at).max(0)),
        None => "—".to_string(),
    };
    let status =
        render::render_status_marked(session.status, render::session_is_degraded(session), color);
    let id = if color {
        session.short_id.green().to_string()
    } else {
        session.short_id.clone()
    };
    let name = session
        .name
        .as_deref()
        .map(|n| format!(" {n} ·"))
        .unwrap_or_default();
    let line1 = format!(
        " {id}{name} {status} · {agent} · {dur} · {steps} steps · {files} files changed\n",
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
    let models_line = if models.is_empty() {
        String::new()
    } else {
        let text = format!("models: {}", render_model_spans(models));
        if color {
            format!(" {}\n", text.dimmed())
        } else {
            format!(" {text}\n")
        }
    };
    format!("{line1} {command}\n{models_line}\n")
}

/// Every model the transcript reported with its step range, in order:
/// `claude-sonnet-4-6 (steps 1–40) → claude-opus-4-8 (steps 41–58)`.
fn render_model_spans(models: &[ModelSpan]) -> String {
    models
        .iter()
        .map(|m| match (m.first_step, m.last_step) {
            (Some(a), Some(b)) if a == b => format!("{} (step {a})", m.model_id),
            (Some(a), Some(b)) => format!("{} (steps {a}–{b})", m.model_id),
            _ => m.model_id.clone(),
        })
        .collect::<Vec<_>>()
        .join(" → ")
}

/// Per-step elapsed time, in step order: from a step's first event to the
/// next step's first event; the last step runs to the session's last agent
/// event. `None` when the end is unknown.
fn step_elapsed(timeline: &[TimelineRow], session: &SessionRow) -> Vec<(i64, Option<i64>)> {
    let starts: Vec<(i64, i64)> = timeline
        .iter()
        .filter_map(|r| match r {
            TimelineRow::Step(s) => Some((s.step, s.ts_ms)),
            TimelineRow::Terminal(_) => None,
        })
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(i, &(step, start))| {
            let end = starts
                .get(i + 1)
                .map(|&(_, next)| next)
                .or(session.last_agent_ts_ms);
            (step, end.map(|e| (e - start).max(0)))
        })
        .collect()
}

/// The per-step table: a header row, a separator, and one row per step with a
/// right-aligned step number, a padded badge, the summary, and an `HH:MM:SS`
/// timestamp. Terminal/lifecycle rows are omitted (this view is steps-only).
fn render_step_table(timeline: &[TimelineRow], session: &SessionRow, color: bool) -> String {
    let elapsed: std::collections::HashMap<i64, Option<i64>> =
        step_elapsed(timeline, session).into_iter().collect();
    let mut out = String::new();
    let header = format!(
        " {step:<5} {badge:<5} {summary:<40} {time:<8} {elapsed}\n",
        step = "STEP",
        badge = "KIND",
        summary = "SUMMARY",
        time = "TIME",
        elapsed = "ELAPSED",
    );
    out.push_str(&header);
    let _ = writeln!(
        out,
        " ----- ----- ---------------------------------------- -------- -------",
    );
    for row in timeline {
        let TimelineRow::Step(s) = row else { continue };
        let e = elapsed.get(&s.step).copied().flatten();
        out.push_str(&render_step_row(s, e, color));
    }
    out
}

/// One row of the step table.
fn render_step_row(s: &StepEntry, elapsed: Option<i64>, color: bool) -> String {
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
        " {step:<5} {badge} {summary:<40} {time} {elapsed}\n",
        step = s.step,
        badge = badge,
        time = render::format_hms(s.ts_ms),
        elapsed = elapsed.map_or_else(|| "—".to_string(), render::humanize_ms),
    )
}

/// The em dash every surface uses for "unknown". Never `0`: zero asserts a
/// known value, a dash admits ignorance.
const DASH: &str = "—";

/// Context used as a share of the limit, in tenths of a percent, when both
/// are known.
fn usage_permille(used: Option<u64>, limit: Option<i64>) -> Option<u64> {
    let limit = u64::try_from(limit?).ok().filter(|l| *l > 0)?;
    Some((used? * 1000 + limit / 2) / limit)
}

/// Render the `hh inspect --context` panel: aligned label/value rows, one
/// accent colour (cyan, on the header id), labels dimmed, every unknown a
/// dash. Self-reported and estimated token counts sit in separate,
/// separately-labelled rows. Plain when `color` is false (NO_COLOR, pipes).
#[allow(clippy::too_many_lines)] // one row per panel line; splitting would scatter the layout
fn render_context_panel(session: &SessionRow, ctx: &SessionContext, color: bool) -> String {
    const LABEL: usize = 15;
    let mut out = String::new();
    let label = |text: &str| {
        let padded = format!("{text:<LABEL$}");
        if color {
            padded.dimmed().to_string()
        } else {
            padded
        }
    };
    let id = if color {
        session.short_id.cyan().to_string()
    } else {
        session.short_id.clone()
    };
    let _ = writeln!(out, " {id} · context");
    out.push('\n');
    let row = |out: &mut String, l: &str, v: &str| {
        let _ = writeln!(out, " {} {v}", label(l));
    };

    row(&mut out, "name", session.name.as_deref().unwrap_or(DASH));
    let model = match (&session.model_display, &session.model_id) {
        (Some(d), Some(id)) => format!("{d} ({id})"),
        (None, Some(id)) => format!("{id} (not in the model table)"),
        _ => DASH.to_string(),
    };
    row(&mut out, "model", &model);
    row(
        &mut out,
        "provider",
        session.provider.as_deref().unwrap_or(DASH),
    );
    let limit = session.context_limit.map_or_else(
        || DASH.to_string(),
        |l| format!("{} tokens", render::thousands(l)),
    );
    row(&mut out, "context limit", &limit);
    let used = ctx.last_turn_context_tokens;
    let used_text = match used {
        Some(u) => {
            let pct = usage_permille(used, session.context_limit)
                .map_or_else(|| DASH.to_string(), |p| format!("{}.{}%", p / 10, p % 10));
            format!(
                "{} tokens · {pct} of limit · latest turn, self-reported",
                render::thousands(i64::try_from(u).unwrap_or(i64::MAX))
            )
        }
        None => DASH.to_string(),
    };
    row(&mut out, "context used", &used_text);
    let capture = match (&session.capture_tier, &session.adapter_id) {
        (Some(t), Some(a)) => format!("{t} · {a}"),
        (Some(t), None) => t.clone(),
        (None, Some(a)) => a.clone(),
        (None, None) => DASH.to_string(),
    };
    row(&mut out, "capture", &capture);
    out.push('\n');

    // Self-reported, accumulated: right-aligned in one column.
    let counts = [
        ("  input", session.input_tokens),
        ("  output", session.output_tokens),
        ("  cache read", session.cache_read_tokens),
        ("  cache write", session.cache_write_tokens),
        ("  reasoning", session.reasoning_tokens),
        ("  total", session.total_tokens()),
    ];
    let cells: Vec<String> = counts
        .iter()
        .map(|(_, v)| v.map_or_else(|| DASH.to_string(), render::thousands))
        .collect();
    let width = cells.iter().map(|c| c.chars().count()).max().unwrap_or(1);
    row(&mut out, "tokens", "self-reported, accumulated");
    for ((l, _), cell) in counts.iter().zip(&cells) {
        let pad = width - cell.chars().count();
        let mut v = format!("{}{cell}", " ".repeat(pad));
        if l.trim() == "total" {
            v.push_str("  derived");
        }
        row(&mut out, l, &v);
    }
    let est = format!(
        "≈ {} tokens · size-based (bytes ÷ 4), not self-reported",
        render::thousands(i64::try_from(ctx.estimated_tokens).unwrap_or(i64::MAX))
    );
    row(&mut out, "estimated", &est);
    out.push('\n');

    row(
        &mut out,
        "messages",
        &format!(
            "{} user · {} agent · {} tool call{}",
            ctx.user_messages,
            ctx.agent_messages,
            ctx.tool_calls,
            if ctx.tool_calls == 1 { "" } else { "s" }
        ),
    );
    row(&mut out, "created", &render::format_utc(session.started_at));
    let last = session.last_event_ts_ms.map_or_else(
        || DASH.to_string(),
        |t| render::format_utc(session.started_at + t),
    );
    row(&mut out, "last activity", &last);
    let wall = session
        .duration_ms()
        .map_or_else(|| DASH.to_string(), render::humanize_ms);
    let active = session
        .active_duration_ms()
        .map_or_else(|| DASH.to_string(), render::humanize_ms);
    row(
        &mut out,
        "duration",
        &format!("{wall} wall · {active} active"),
    );
    let cost = ctx.cost_nano_usd.map_or_else(
        || DASH.to_string(),
        |c| format!("{} at list price", render::format_usd(c)),
    );
    row(&mut out, "est. cost", &cost);
    if !ctx.models.is_empty() {
        row(&mut out, "models", &render_model_spans(&ctx.models));
    }
    out
}

/// The `hh inspect --context --json` object (docs/context.md, docs/json.md).
/// Self-reported counts live under `usage` (with `source:
/// "self_reported"`), the size-based figure under `estimated` — never in the
/// same field. Unknowns are `null`, never 0.
fn context_json(session: &SessionRow, ctx: &SessionContext) -> serde_json::Value {
    let used_percent = usage_permille(ctx.last_turn_context_tokens, session.context_limit)
        .and_then(|p| u32::try_from(p).ok())
        .map(|p| f64::from(p) / 10.0);
    let duration_ms = session.duration_ms();
    let active_ms = session.active_duration_ms();
    let last_activity_at = session.last_event_ts_ms.map(|t| session.started_at + t);
    serde_json::json!({
        "schema": SCHEMA_VERSION,
        "session": session_ref(session),
        "name": session.name,
        "model": {
            "id": session.model_id,
            "display": session.model_display,
            "provider": session.provider,
            "context_limit": session.context_limit,
        },
        "context_used": {
            "source": "self_reported_latest_turn",
            "tokens": ctx.last_turn_context_tokens,
            "percent_of_limit": used_percent,
        },
        "usage": crate::usage_json(session),
        "estimated": {
            "source": "size_based",
            "method": "bytes_div_4",
            "tokens": ctx.estimated_tokens,
        },
        "messages": {
            "user": ctx.user_messages,
            "agent": ctx.agent_messages,
            "tool_calls": ctx.tool_calls,
        },
        "capture_tier": session.capture_tier,
        "adapter_id": session.adapter_id,
        "created_at": session.started_at,
        "created": render::rfc3339_utc(session.started_at),
        "last_activity_at": last_activity_at,
        "last_activity": last_activity_at.map(render::rfc3339_utc),
        "duration_ms": duration_ms,
        "duration": duration_ms.map(render::iso8601_duration),
        "active_duration_ms": active_ms,
        "active_duration": active_ms.map(render::iso8601_duration),
        "estimated_cost_usd": ctx.cost_nano_usd.map(render::usd_decimal),
        "cost_basis": "list_price",
        "models": ctx.models.iter().map(|m| serde_json::json!({
            "model_id": m.model_id,
            "first_step": m.first_step,
            "last_step": m.last_step,
        })).collect::<Vec<_>>(),
    })
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
        return indent(&decode_pretty_json(&text));
    }
    tint_json(&text)
}

/// Decode every JSON string literal in already-pretty-printed `text`, turning
/// `\n`/`\t`/etc. back into real control characters (mirrors the replay
/// detail pane's `replay::json` decoding — see FR-4.2). String
/// boundaries are found with an escape-aware scan over the still-escaped
/// source before decoding, so a literal `"` or `\` inside decoded content
/// can't be mistaken for a delimiter. Falls back to the untouched literal on
/// decode failure; never panics.
fn decode_pretty_json(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' {
            let start = i;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                if chars[i] == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let raw: String = chars[start..i.min(chars.len())].iter().collect();
            out.push_str(&decode_string_literal(&raw));
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Decode `raw` (a JSON string literal, quotes included) into display text,
/// re-wrapped in display quotes. Falls back to the untouched literal if it
/// isn't well-formed JSON.
fn decode_string_literal(raw: &str) -> String {
    match serde_json::from_str::<String>(raw) {
        Ok(decoded) => format!("\"{decoded}\""),
        Err(_) => raw.to_string(),
    }
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
                let decoded = decode_string_literal(&s);
                if is_key {
                    out.push_str(&decoded.cyan().to_string());
                } else {
                    out.push_str(&decoded.green().to_string());
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
    // A rename (migration 0006) is one row on the new path; its before-side
    // is the old path's content, so the `a/` side names the old path, git
    // style. A lookup failure just renders it as a plain modify.
    let renamed_from = store
        .file_change_provenance(fc.event_id)
        .ok()
        .flatten()
        .and_then(|p| p.renamed_from);
    let a_path = renamed_from.as_deref().unwrap_or(&fc.path);
    push_diff_header(&mut out, a_path, &fc.path, renamed_from.is_some(), color);
    // A pure rename (content unchanged) is fully described by the header,
    // exactly as `git diff` prints it.
    if renamed_from.is_some() && fc.before_hash.is_some() && fc.before_hash == fc.after_hash {
        return out;
    }
    let (a_label, b_label) = match fc.change_kind {
        ChangeKind::Created => ("/dev/null".to_string(), format!("b/{path}", path = fc.path)),
        ChangeKind::Deleted => (format!("a/{path}", path = fc.path), "/dev/null".to_string()),
        ChangeKind::Modified => (format!("a/{a_path}"), format!("b/{path}", path = fc.path)),
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

/// The `diff -- a/<a_path> b/<b_path>` line, plus git's `rename from` /
/// `rename to` lines for a rename.
fn push_diff_header(out: &mut String, a_path: &str, b_path: &str, renamed: bool, color: bool) {
    let header = format!("diff -- a/{a_path} b/{b_path}\n");
    out.push_str(&if color {
        header.cyan().to_string()
    } else {
        header
    });
    if renamed {
        let rename = format!("rename from {a_path}\nrename to {b_path}\n");
        out.push_str(&if color {
            rename.dimmed().to_string()
        } else {
            rename
        });
    }
}

/// The structured twin of [`render_file_change_diff`]: the same hunk-walking
/// logic, but building JSON hunks/lines instead of a formatted+colored
/// string. Used by `hh export --html`'s client-side diff viewer, so the
/// browser never needs to implement a diff algorithm itself — it only ever
/// renders precomputed `{tag, text}` lines via `textContent`.
pub(crate) fn file_change_diff_json(fc: &FileChange, store: &Store) -> serde_json::Value {
    if fc.is_binary {
        return serde_json::json!({
            "path": fc.path,
            "change_kind": fc.change_kind.to_string(),
            "is_binary": true,
            "note": "Binary file — diff not shown.",
            "hunks": [],
        });
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

    let mut hunks: Vec<serde_json::Value> = Vec::new();
    let mut note: Option<&'static str> = None;

    if let (Some(a), Some(b)) = (before.as_deref(), after.as_deref()) {
        let diff = TextDiff::from_lines(a, b);
        let unified = diff.unified_diff();
        for hunk in unified.iter_hunks() {
            let lines: Vec<serde_json::Value> = hunk
                .iter_changes()
                .map(|change| {
                    let tag = match change.tag() {
                        ChangeTag::Delete => "delete",
                        ChangeTag::Insert => "insert",
                        ChangeTag::Equal => "equal",
                    };
                    let line = change.to_string_lossy();
                    let line = line.strip_suffix('\n').unwrap_or(&line);
                    serde_json::json!({ "tag": tag, "text": line })
                })
                .collect();
            hunks.push(serde_json::json!({
                "header": hunk.header().to_string(),
                "lines": lines,
            }));
        }
        if hunks.is_empty() {
            note = Some("(no changes)");
        }
    } else {
        // A create/delete has only one side, or the referenced blob was
        // never captured — show whichever side exists in full, as one
        // headerless hunk.
        match before.as_deref().or(after.as_deref()) {
            Some(text) => {
                let tag = if after.is_none() { "delete" } else { "insert" };
                let lines: Vec<serde_json::Value> = text
                    .lines()
                    .map(|l| serde_json::json!({ "tag": tag, "text": l }))
                    .collect();
                hunks.push(serde_json::json!({ "header": null, "lines": lines }));
            }
            None => note = Some("(content not captured)"),
        }
    }

    serde_json::json!({
        "path": fc.path,
        "change_kind": fc.change_kind.to_string(),
        "is_binary": false,
        "note": note,
        "hunks": hunks,
    })
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
    let elapsed_ms = step_elapsed(timeline, session)
        .into_iter()
        .find(|&(step, _)| step == entry.step)
        .and_then(|(_, e)| e);
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
        "elapsed_ms": elapsed_ms,
        "elapsed": elapsed_ms.map(render::iso8601_duration),
        "events": events,
    }))
}

/// The `session` reference embedded in every inspect JSON object.
/// Includes `adapter_degrade_reason` (SRS v1.1.0 BUG-1.1) so `hh inspect
/// --json` on a degraded session shows the machine-readable code.
pub(crate) fn session_ref(session: &SessionRow) -> serde_json::Value {
    serde_json::json!({
        "id": session.id,
        "short_id": session.short_id,
        "adapter_status": session.adapter_status.to_string(),
        "adapter_degrade_reason": session.adapter_degrade_reason,
    })
}

/// Build the stable JSON object for one event (the unit of both the NDJSON
/// stream and a step object's `events` array, and the `events` entries of an
/// `hh export` bundle). See `docs/json.md`.
pub(crate) fn event_to_json(d: &EventDetail, session: &SessionRow) -> serde_json::Value {
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
            render_summary_header(&fx.session, &[], false),
            render_step_table(&fx.timeline, &fx.session, false)
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

    /// [`file_change_diff_json`]'s structured hunks carry the same content as
    /// [`render_file_change_diff`]'s formatted text — same lines, same
    /// insert/delete tags — just as data instead of a colored string.
    #[test]
    fn diff_json_matches_text_diff_content() {
        let fx = Fixture::build();
        let detail = fx
            .index
            .iter()
            .find(|e| e.kind == EventKind::FileChange)
            .and_then(|e| fx.store.get_event_detail(e.id).ok())
            .expect("file change event");
        let fc = detail.file_change.as_ref().expect("file_change row");
        let json = file_change_diff_json(fc, &fx.store);
        assert_eq!(json["path"], "created.txt");
        assert_eq!(json["change_kind"], "created");
        assert_eq!(json["is_binary"], false);
        // A create has one headerless hunk of pure inserts.
        let hunks = json["hunks"].as_array().unwrap();
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0]["header"].is_null());
        let lines = hunks[0]["lines"].as_array().unwrap();
        assert_ne!(lines.as_slice(), [] as [serde_json::Value; 0]);
        assert!(lines.iter().all(|l| l["tag"] == "insert"));
        assert!(lines.iter().any(|l| l["text"] == "created-content"));
    }

    #[test]
    fn diff_json_binary_file_carries_a_note_not_content() {
        let json = file_change_diff_json(
            &FileChange {
                event_id: 0,
                path: "image.png".into(),
                change_kind: ChangeKind::Modified,
                before_hash: None,
                after_hash: None,
                is_binary: true,
            },
            &Fixture::build().store,
        );
        assert_eq!(json["is_binary"], true);
        assert!(json["note"].as_str().unwrap().contains("Binary"));
        assert_eq!(json["hunks"].as_array().unwrap().len(), 0);
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
        // Sanity: every line is a JSON object carrying schema:2, in order.
        for line in ndjson.lines() {
            let v: serde_json::Value =
                serde_json::from_str(line).expect("each NDJSON line is valid JSON");
            assert_eq!(v["schema"], 2, "every event object carries schema:2");
            assert!(v["kind"].is_string(), "every event object has a kind");
        }
    }

    /// `hh inspect --json` on a degraded session MUST include
    /// `"adapter_degrade_reason": "jsonl_parse_error"` in the `session`
    /// object of every event's NDJSON line (SRS BUG-1 acceptance criterion,
    /// ENH-2.3). This is the load-bearing assertion for BUG-1: a degraded
    /// session self-documents its degrade code in the JSON output, not just
    /// in a one-line stderr warning.
    #[test]
    fn degraded_session_json_includes_degrade_reason() {
        let fx = Fixture::build();
        // Mark the session degraded with a reason code, simulating what the
        // recorder's `finalize` does when the adapter reports an
        // `AdapterError::JsonlParseError`.
        fx.store
            .set_session_adapter_meta_with_reason(
                &fx.session.id,
                None,
                None,
                hh_core::AdapterStatus::Degraded,
                Some("jsonl_parse_error"),
            )
            .unwrap();
        // Re-read the session row so `session_ref` picks up the degrade code.
        let degraded = fx.store.get_session(&fx.session.id).unwrap();
        assert_eq!(degraded.adapter_status, hh_core::AdapterStatus::Degraded);
        assert_eq!(
            degraded.adapter_degrade_reason.as_deref(),
            Some("jsonl_parse_error"),
        );
        let mut buf = Vec::new();
        write_session_ndjson(&fx.store, &degraded, &mut buf).unwrap();
        let ndjson = String::from_utf8(buf).expect("NDJSON is UTF-8");
        // Every line's `session.adapter_degrade_reason` must be the code.
        for line in ndjson.lines() {
            let v: serde_json::Value =
                serde_json::from_str(line).expect("each NDJSON line is valid JSON");
            assert_eq!(
                v["session"]["adapter_degrade_reason"], "jsonl_parse_error",
                "every event's session ref must carry the degrade code"
            );
            assert_eq!(
                v["session"]["adapter_status"], "degraded",
                "every event's session ref must carry the degraded status"
            );
        }
        // Snapshot the first line so the exact JSON shape (including the
        // new fields) is locked — a future change that drops or renames
        // `adapter_degrade_reason` is caught.
        let first = ndjson.lines().next().expect("at least one event");
        insta::assert_snapshot!("degraded_session_ndjson_first_line", first);
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

    /// The fixture plus a name and self-reported usage from `models` (one
    /// response per entry, in order), re-read so the session row carries the
    /// accumulated columns.
    fn context_fixture(models: &[&str]) -> (Fixture, SessionContext) {
        let mut fx = Fixture::build();
        let sid = fx.session.id.clone();
        fx.store
            .set_session_name(&sid, Some("auth refactor"))
            .unwrap();
        let w = fx.store.event_writer().unwrap();
        for (i, m) in models.iter().enumerate() {
            let i = i64::try_from(i).unwrap();
            w.append_event(Event {
                session_id: sid.clone(),
                ts_ms: 2_600 + i,
                kind: EventKind::AgentMessage,
                step: None,
                summary: "answer".into(),
                body_json: Some(serde_json::json!({
                    "text": "answer", "model": m, "message_id": format!("msg_{i}"),
                    "usage": {
                        "input_tokens": 1_200, "output_tokens": 340,
                        "cache_read_input_tokens": 48_000, "cache_creation_input_tokens": 2_000
                    }
                })),
                blob_hash: None,
                blob_size: None,
                correlates: None,
            })
            .unwrap();
        }
        w.finish().unwrap();
        fx.store.assign_steps(&sid).unwrap();
        fx.session = fx.store.get_session(&sid).unwrap();
        let index = fx.store.list_event_index(&sid).unwrap();
        fx.timeline = build_timeline(&index, false);
        let ctx = fx.store.session_context(&sid).unwrap();
        (fx, ctx)
    }

    #[test]
    fn snapshot_context_panel() {
        let (fx, ctx) = context_fixture(&["claude-opus-4-8", "claude-opus-4-8"]);
        insta::assert_snapshot!(render_context_panel(&fx.session, &ctx, false));
    }

    #[test]
    fn snapshot_context_json() {
        let (fx, ctx) = context_fixture(&["claude-opus-4-8", "claude-opus-4-8"]);
        let pretty = serde_json::to_string_pretty(&context_json(&fx.session, &ctx)).unwrap();
        insta::assert_snapshot!(pretty);
    }

    /// An unknown model renders as a dash everywhere a table value would
    /// go — provider, limit, usage %, cost — and never as 0. Self-reported
    /// token counts it did report still show.
    #[test]
    fn unknown_model_renders_dashes_not_zero() {
        let (fx, ctx) = context_fixture(&["glm-5.2"]);
        let panel = render_context_panel(&fx.session, &ctx, false);
        let line = |label: &str| {
            panel
                .lines()
                .find(|l| l.trim_start().starts_with(label))
                .unwrap_or_else(|| panic!("no {label} line in\n{panel}"))
                .to_string()
        };
        assert!(line("model").contains("glm-5.2 (not in the model table)"));
        assert!(line("provider").ends_with('—'), "{panel}");
        assert!(line("context limit").ends_with('—'), "{panel}");
        assert!(line("context used").contains("— of limit"), "{panel}");
        assert!(line("est. cost").ends_with('—'), "{panel}");
        assert!(line("reasoning").ends_with('—'), "{panel}");
        assert!(line("input").contains("1,200"), "{panel}");
        assert!(!line("est. cost").contains('0'), "{panel}");
        let json = context_json(&fx.session, &ctx);
        assert!(json["estimated_cost_usd"].is_null());
        assert!(json["model"]["context_limit"].is_null());
        assert!(json["context_used"]["percent_of_limit"].is_null());
        assert!(json["usage"]["reasoning_tokens"].is_null());
        assert_eq!(json["usage"]["input_tokens"], 1_200);
    }

    /// A mid-session model change: the session row records the last model,
    /// and the summary header lists every model with its step range.
    #[test]
    fn snapshot_summary_with_mid_session_model_change() {
        let (fx, ctx) =
            context_fixture(&["claude-sonnet-4-6", "claude-sonnet-4-6", "claude-opus-4-8"]);
        assert_eq!(fx.session.model_id.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(ctx.models.len(), 2);
        let out = format!(
            "{}{}",
            render_summary_header(&fx.session, &ctx.models, false),
            render_step_table(&fx.timeline, &fx.session, false)
        );
        insta::assert_snapshot!(out);
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
