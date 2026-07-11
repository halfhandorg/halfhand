//! Agent adapters: tail a structured event stream and yield [`Event`]s (FR-1.5).
//!
//! An [`Adapter`] owns a tailer thread that produces [`Event`]s on an
//! `mpsc::Receiver`; the recorder (`hh-record`) owns a drain thread that
//! consumes them, resolves the adapter's `correlate_key` to a DB row id in
//! [`Event::correlates`], and appends to the single-writer task. The adapter
//! receives an [`Arc<BlobStore>`] (for >256 KiB spillover) and a stop flag, but
//! **no `EventWriter`** — keeping it unit-testable without a database and the
//! writer ownership in `hh-record`. A future Codex adapter is a new impl + one
//! new branch in [`select`]; `hh-record` is unchanged.
//!
//! ## Cross-event correlation
//!
//! Adapters cannot know the DB row id an event will receive, so to express
//! "this `tool_result` belongs to that `tool_call`" they emit a string
//! `correlate_key` field inside [`Event::body_json`] (the Claude adapter uses
//! `tool_use.id` for a `tool_call` and `tool_use_id` for a `tool_result`). The
//! recorder's drain thread keeps a `HashMap<String, i64>` (key → event id) for
//! `tool_call`s and, for a `tool_result`, sets `event.correlates =
//! map.get(key)` before appending. A result whose key was never seen (orphan /
//! truly-concurrent result-before-call) gets `correlates = None` and its own
//! step (handled by the FR-3.4 step pass).
//!
//! ## Failure mode (FR-1.5)
//!
//! If the structured log cannot be located (no `~/.claude/projects` dir, or no
//! transcript file appears in time), the adapter emits a single stderr warning
//! and returns [`AdapterStatus::Degraded`]; PTY + FS recording in the recorder
//! continue unaffected.

use crate::blob::BlobStore;
use crate::event::{truncate_summary, AdapterStatus, AgentKind, Event, EventKind};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Payloads at or above this size (in bytes) spill to the blob store rather
/// than being stored inline in `events.body_json` (SRS §4.1, matching the
/// recorder's terminal-chunk threshold).
const SPILLOVER_BYTES: usize = 256 * 1024;
/// How long to wait for the transcript file to appear after the session starts
/// (Claude creates it lazily on first output). Tuned for slow disk / cold start.
const FILE_APPEAR_TIMEOUT: Duration = Duration::from_secs(3);
/// Tail poll interval when the transcript is at EOF but the session continues.
const TAIL_POLL: Duration = Duration::from_millis(50);
/// Poll interval while waiting for the transcript file to appear.
const APPEAR_POLL: Duration = Duration::from_millis(200);

/// Context handed to an adapter when it is spawned.
#[derive(Clone)]
pub struct AdapterContext {
    /// Owning session id (full UUID string).
    pub session_id: String,
    /// Session start as a unix-ms UTC timestamp; `ts_ms` is relative to this.
    pub started_at_unix_ms: i64,
    /// Session working directory — used to locate/verify the structured log.
    pub cwd: PathBuf,
    /// Full command argv (used for detection and the fallback scan).
    pub command: Vec<String>,
    /// Blob store for >256 KiB spillover.
    pub blobs: std::sync::Arc<BlobStore>,
    /// Stop flag: set by the recorder when the child has exited.
    pub stop: std::sync::Arc<AtomicBool>,
    /// Override for the Claude projects directory (`None` → resolve from `HOME`).
    /// Tests set this to a temp dir so the adapter never touches the real home and
    /// avoids `HOME` env races across parallel tests; production passes `None`.
    pub projects_dir: Option<PathBuf>,
}

/// A handle to a running adapter tailer. Drop the `events` receiver (or drain
/// it to EOF) and [`AdapterHandle::join`] the outcome when the session ends.
pub struct AdapterHandle {
    /// The stream of parsed events, in arrival order. EOF when the tailer exits.
    pub events: mpsc::Receiver<Event>,
    /// Joins the tailer thread and returns the adapter's final outcome.
    pub outcome: JoinHandle<AdapterOutcome>,
}

impl AdapterHandle {
    /// Block until the tailer thread exits and return its outcome. The tailer
    /// is total (never panics on record content), so a panic here indicates an
    /// internal bug, surfaced as [`crate::Error::Storage`] for the binary to
    /// attach `anyhow` context.
    pub fn join(self) -> crate::Result<AdapterOutcome> {
        self.outcome.join().map_err(|_| {
            crate::Error::Storage(crate::error::StorageError::Open {
                // The exact path is irrelevant for an adapter-thread panic; reuse
                // the storage error surface so the binary's print_error renders it.
                path: PathBuf::new(),
                source: std::io::Error::other("adapter thread panicked"),
            })
        })
    }
}

/// The final outcome of an adapter run, reported when its tailer thread exits.
#[derive(Debug, Default, Clone)]
pub struct AdapterOutcome {
    /// Model name extracted from the structured stream, if any (last-seen wins).
    pub model: Option<String>,
    /// Token-usage JSON, verbatim from the last assistant record that carried it
    /// (rich fields preserved; not cumulative). Written to `sessions.usage_json`.
    pub usage_json: Option<serde_json::Value>,
    /// Final adapter status: [`AdapterStatus::Active`] if a transcript was
    /// tailed, [`AdapterStatus::Degraded`] if it could not be located.
    pub status: AdapterStatus,
}

/// An agent adapter: tail a structured event stream and yield [`Event`]s.
///
/// Implementations are `Send + 'static` so the recorder can spawn the tailer on
/// its own thread. [`Adapter::spawn`] performs any setup that can fail (e.g. the
/// tailer thread spawn) and returns an `io::Error` only for that low-level
/// failure; higher-level "no transcript found" conditions are reported via the
/// returned handle's [`AdapterOutcome::status`] (degraded), not as an error, so
/// the recorder can keep recording PTY/FS output (FR-1.5).
pub trait Adapter: Send + 'static {
    /// The agent kind this adapter reports for the session row.
    fn agent_kind(&self) -> AgentKind;
    /// Spawn the tailer thread, returning the event stream + outcome handle.
    ///
    /// Takes `self: Box<Self>` so the recorder can dispatch through the
    /// `Box<dyn Adapter>` returned by [`select`] (a by-value `self` would not
    /// be callable on a trait object). A stateless adapter like [`ClaudeAdapter`]
    /// ignores the receiver.
    fn spawn(self: Box<Self>, ctx: AdapterContext) -> std::io::Result<AdapterHandle>;
}

/// Detect which adapter applies to `command` (FR-1.5). Returns `None` for a
/// generic agent (PTY-only capture). Today only Claude Code is detected, by the
/// `claude` basename of the command; a forced adapter from `hh run --adapter`
/// is resolved by the recorder, not here.
#[must_use]
pub fn select(command: &[String], _cwd: &Path) -> Option<Box<dyn Adapter>> {
    if is_claude_code(command) {
        Some(Box::new(ClaudeAdapter))
    } else {
        None
    }
}

/// Resolve a forced adapter name (from `hh run --adapter <name>`) to an
/// adapter, or `None` if the name is unrecognized — the recorder surfaces that
/// as an actionable error. Keeping this map in `hh-core` (not the recorder)
/// means adding a forced adapter is a one-branch change here, with no
/// `hh-record` edit (the trait's extensibility goal).
#[must_use]
pub fn resolve_override(name: &str) -> Option<Box<dyn Adapter>> {
    match name {
        "claude-code" => Some(Box::new(ClaudeAdapter)),
        _ => None,
    }
}

/// True if the command's program basename is `claude` (stripping a Windows
/// `.exe`). Mirrors the detection in `hh-record::agent::detect_agent`.
#[must_use]
pub fn is_claude_code(command: &[String]) -> bool {
    let prog = command.first().map_or("", String::as_str);
    // Split on both `/` and `\` so a Windows path like `C:\Apps\claude.exe` yields
    // `claude.exe` on any platform — `Path::file_name` treats `\` as a normal
    // character on Unix, which would mis-basename Windows command lines.
    let base = prog.rsplit(['/', '\\']).next().unwrap_or(prog);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    base.eq_ignore_ascii_case("claude")
}

// ---------------------------------------------------------------------------
// Claude Code adapter
// ---------------------------------------------------------------------------

/// The Claude Code adapter: tails `~/.claude/projects/<slug>/*.jsonl` and
/// converts records to `user_message` / `agent_message` / `thinking` /
/// `tool_call` / `tool_result` events. Stateless; the tailer thread holds all
/// mutable state. A unit struct (rather than `struct ClaudeAdapter {}`) so it
/// is constructible as a value without a `{}`; reserved for future per-adapter
/// knobs (e.g. a forced projects dir) by adding fields + a `Default` impl.
#[derive(Debug, Clone)]
pub struct ClaudeAdapter;

impl Adapter for ClaudeAdapter {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::ClaudeCode
    }

    #[allow(clippy::unused_self)] // ClaudeAdapter is stateless; the `Box<Self>` receiver exists only so the recorder can dispatch through `Box<dyn Adapter>` (FR-1.5). A future stateful adapter would read its config from `self` here.
    fn spawn(self: Box<Self>, ctx: AdapterContext) -> std::io::Result<AdapterHandle> {
        let (tx, rx) = mpsc::channel::<Event>();
        let outcome = std::thread::Builder::new()
            .name("hh-claude-adapter".into())
            .spawn(move || run_claude_tailer(ctx, tx))?;
        Ok(AdapterHandle {
            events: rx,
            outcome,
        })
    }
}

/// The tailer thread body: locate the transcript, then read it to EOF/stop,
/// parsing each line into events sent on `tx`. Returns the final outcome.
#[allow(clippy::needless_pass_by_value)] // ctx/tx are owned for the thread's lifetime; taking them by value keeps the tailer self-contained (mirrors runner::run_reader).
fn run_claude_tailer(ctx: AdapterContext, tx: mpsc::Sender<Event>) -> AdapterOutcome {
    let Some(projects) = ctx.projects_dir.clone().or_else(claude_projects_dir) else {
        warn("claude adapter: no ~/.claude/projects directory found (HOME unset?)");
        return AdapterOutcome {
            status: AdapterStatus::Degraded,
            ..Default::default()
        };
    };
    if !projects.is_dir() {
        warn("claude adapter: projects directory does not exist; tailing skipped");
        return AdapterOutcome {
            status: AdapterStatus::Degraded,
            ..Default::default()
        };
    }
    let start = Instant::now();
    let Some(file) = locate_transcript(
        &projects,
        &ctx.cwd,
        ctx.started_at_unix_ms,
        &start,
        &ctx.stop,
    ) else {
        warn("claude adapter: could not locate a transcript for this session within deadline");
        return AdapterOutcome {
            status: AdapterStatus::Degraded,
            ..Default::default()
        };
    };
    let mut acc = OutcomeAcc::default();
    tail_file(&file, &projects, &ctx, &tx, &start, &mut acc);
    acc.finish(AdapterStatus::Active)
}

/// Accumulates model/usage across assistant records while tailing.
#[derive(Default)]
struct OutcomeAcc {
    model: Option<String>,
    usage_json: Option<serde_json::Value>,
}

impl OutcomeAcc {
    fn note_assistant(&mut self, message: &serde_json::Value) {
        if let Some(m) = message.get("model").and_then(|v| v.as_str()) {
            self.model = Some(m.to_string());
        }
        if let Some(u) = message.get("usage") {
            self.usage_json = Some(u.clone());
        }
    }

    fn finish(self, status: AdapterStatus) -> AdapterOutcome {
        AdapterOutcome {
            model: self.model,
            usage_json: self.usage_json,
            status,
        }
    }
}

/// Locate the transcript file for this session: the newest `*.jsonl` under the
/// slug dir (cwd with `/`+`.`→`-`), polling up to [`FILE_APPEAR_TIMEOUT`] for it
/// to appear, then a fallback scan of all project dirs by first-record `cwd`.
/// Returns `None` if nothing qualifies before the stop flag is set.
fn locate_transcript(
    projects: &Path,
    cwd: &Path,
    started_at_unix_ms: i64,
    start: &Instant,
    stop: &std::sync::Arc<AtomicBool>,
) -> Option<PathBuf> {
    let slug_dir = projects.join(slugify(cwd));
    // Only poll an existing slug dir for the transcript to appear late. If the slug
    // dir is absent, the transcript (if any) lives elsewhere — go straight to the
    // cwd-based fallback scan rather than polling a path that cannot grow a file.
    if slug_dir.is_dir() {
        if let Some(f) = newest_in_dir(&slug_dir, started_at_unix_ms) {
            return Some(f);
        }
        let deadline = *start + FILE_APPEAR_TIMEOUT;
        while Instant::now() < deadline {
            if stop.load(Ordering::Acquire) {
                return None;
            }
            if let Some(f) = newest_in_dir(&slug_dir, started_at_unix_ms) {
                return Some(f);
            }
            std::thread::sleep(APPEAR_POLL);
        }
    }
    // Fallback: the slug may disagree with Claude's encoding. Scan all project
    // dirs for the newest transcript whose first record's cwd matches ours.
    fallback_scan_by_cwd(projects, cwd, started_at_unix_ms)
}

/// The newest `*.jsonl` in `dir` by mtime, with a small slack window so a file
/// created just before `started_at_unix_ms` (clock skew) still qualifies.
fn newest_in_dir(dir: &Path, since_unix_ms: i64) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(PathBuf, i64)> = None;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(m) = e.metadata() else { continue };
        let Ok(modified) = m.modified() else { continue };
        let mtime_ms = system_time_to_unix_ms(modified);
        if mtime_ms < since_unix_ms - 5_000 {
            continue; // 5s slack
        }
        match &best {
            Some((_, best_ms)) if mtime_ms <= *best_ms => {}
            _ => best = Some((p, mtime_ms)),
        }
    }
    best.map(|(p, _)| p)
}

/// Scan every project dir for `*.jsonl` transcripts newer than the session,
/// newest-first, returning the first whose first parseable record's `cwd`
/// matches (or is under) the session cwd. A last resort when the slug misses.
fn fallback_scan_by_cwd(projects: &Path, cwd: &Path, started_at_unix_ms: i64) -> Option<PathBuf> {
    let mut candidates: Vec<(PathBuf, i64)> = Vec::new();
    let slug_dirs = std::fs::read_dir(projects).ok()?;
    for sd in slug_dirs.flatten() {
        let sp = sd.path();
        if !sp.is_dir() {
            continue;
        }
        if let Some(f) = newest_in_dir(&sp, started_at_unix_ms) {
            let mtime_ms = std::fs::metadata(&f)
                .ok()
                .and_then(|m| m.modified().ok())
                .map_or(0, system_time_to_unix_ms);
            candidates.push((f, mtime_ms));
        }
    }
    candidates.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
    candidates
        .into_iter()
        .map(|(f, _)| f)
        .find(|f| first_record_cwd_matches(f, cwd))
}

/// True if the first parseable JSONL record's `cwd` equals or is under `session_cwd`.
fn first_record_cwd_matches(file: &Path, session_cwd: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(file) else {
        return false;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(rc) = v.get("cwd").and_then(|c| c.as_str()) {
            let rp = Path::new(rc);
            return rp == session_cwd || rp.starts_with(session_cwd);
        }
    }
    false
}

/// Read `file` from byte 0, parsing each complete line into events sent on `tx`.
/// Handles partial last lines (buffered until a newline arrives), rotation to a
/// newer `*.jsonl` in the slug dir, and stop-flag termination.
fn tail_file(
    file: &Path,
    projects: &Path,
    ctx: &AdapterContext,
    tx: &mpsc::Sender<Event>,
    start: &Instant,
    acc: &mut OutcomeAcc,
) {
    let slug_dir = projects.join(slugify(&ctx.cwd));
    let mut current = file.to_path_buf();
    let mut reader = match std::fs::File::open(&current) {
        Ok(f) => std::io::BufReader::new(f),
        Err(e) => {
            warn(&format!("claude adapter: could not open transcript: {e}"));
            return;
        }
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut line = Vec::new();
    loop {
        if ctx.stop.load(Ordering::Acquire) {
            break;
        }
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => {
                // EOF: parse any *complete* lines buffered so far, keep the
                // trailing partial (no newline yet) for the next grow.
                drain_complete_lines(&mut buf, &mut line, ctx, tx, start, acc);
                // Rotation: a newer .jsonl appeared in the slug dir?
                if let Some(newer) = rotate_to_newer(&slug_dir, &current, ctx.started_at_unix_ms) {
                    current = newer;
                    reader = match std::fs::File::open(&current) {
                        Ok(f) => std::io::BufReader::new(f),
                        Err(_) => break,
                    };
                    line.clear();
                    continue;
                }
                if ctx.stop.load(Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(TAIL_POLL);
            }
            Ok(_n) => {
                drain_complete_lines(&mut buf, &mut line, ctx, tx, start, acc);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    // Final flush of any complete trailing line on stop.
    drain_complete_lines(&mut buf, &mut line, ctx, tx, start, acc);
}

/// Move the just-read `line` bytes into `buf`, then parse every complete
/// (`\n`-terminated) line in `buf`, leaving the trailing partial in `buf`.
fn drain_complete_lines(
    buf: &mut Vec<u8>,
    line: &mut Vec<u8>,
    ctx: &AdapterContext,
    tx: &mpsc::Sender<Event>,
    start: &Instant,
    acc: &mut OutcomeAcc,
) {
    if line.is_empty() {
        return;
    }
    buf.append(line);
    line.clear();
    // Find the last `\n`; everything up to and including it is complete.
    while let Some(nl) = buf.iter().rposition(|&b| b == b'\n') {
        let complete: Vec<u8> = buf.drain(..=nl).collect();
        parse_and_send(&complete, ctx, tx, start, acc);
    }
}

/// Parse one buffered line (bytes) into events and send them. Malformed JSON or
/// skipped record types produce no events; a debug trace is gated on `HH_DEBUG`.
fn parse_and_send(
    bytes: &[u8],
    ctx: &AdapterContext,
    tx: &mpsc::Sender<Event>,
    start: &Instant,
    acc: &mut OutcomeAcc,
) {
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s.trim(),
        Err(_) => return,
    };
    if s.is_empty() {
        return;
    }
    let value: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => {
            debug(&format!("claude adapter: skipping unparseable line: {e}"));
            return;
        }
    };
    let ts_ms = value
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| ts_ms_from_iso(s, ctx.started_at_unix_ms))
        .unwrap_or_else(|| elapsed_ms(start));
    let parsed = parse_record(&value, &ctx.session_id, ts_ms, &ctx.blobs);
    if parsed.is_assistant_message {
        if let Some(msg) = value.get("message") {
            acc.note_assistant(msg);
        }
    }
    for ev in parsed.events {
        if tx.send(ev).is_err() {
            break; // recorder dropped the receiver: stop sending.
        }
    }
}

/// If a `*.jsonl` newer than `current` exists in the slug dir, return it.
fn rotate_to_newer(slug_dir: &Path, current: &Path, started_at_unix_ms: i64) -> Option<PathBuf> {
    let newest = newest_in_dir(slug_dir, started_at_unix_ms)?;
    if newest == current {
        return None;
    }
    // Only rotate if the candidate is actually newer than the current file.
    let newer_mtime = std::fs::metadata(&newest)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(system_time_to_unix_ms);
    let cur_mtime = std::fs::metadata(current)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(system_time_to_unix_ms);
    match (newer_mtime, cur_mtime) {
        (Some(n), Some(c)) if n > c => Some(newest),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Pure parser (the insta target)
// ---------------------------------------------------------------------------

/// One parsed record: the events it produced, plus whether it was an assistant
/// message (so the tailer can collect model/usage) and any model/usage seen.
#[derive(Debug, Default, Clone)]
pub(crate) struct ParsedRecord {
    /// Events derived from the record's content blocks.
    pub events: Vec<Event>,
    /// True for `assistant` records (the tailer collects model/usage from these).
    pub is_assistant_message: bool,
}

/// Parse one Claude JSONL record into events. Pure given `(value, ts_ms,
/// blobs)`: deterministic output for fixed inputs (the only side effect is
/// deterministic blob spillover for >256 KiB payloads, whose hash is BLAKE3 of
/// fixed content). Unknown `type`s, `isSidechain`/`isMeta` records, and
/// unexpected shapes produce no events (tolerant — see module docs).
#[must_use]
pub(crate) fn parse_record(
    value: &serde_json::Value,
    session_id: &str,
    ts_ms: i64,
    blobs: &BlobStore,
) -> ParsedRecord {
    let mut out = ParsedRecord::default();
    let Some(obj) = value.as_object() else {
        return out; // non-object line: nothing to parse
    };
    let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if ty != "user" && ty != "assistant" {
        return out; // system / mode / attachment / ai-title / file-history-snapshot / ...
    }
    if obj
        .get("isSidechain")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return out; // subagent transcript
    }
    if obj
        .get("isMeta")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return out; // injected caveat / system-reminder wrappers
    }
    let Some(message) = obj.get("message") else {
        return out;
    };
    let is_assistant = ty == "assistant";
    out.is_assistant_message = is_assistant;
    let Some(content) = message.get("content") else {
        return out;
    };
    match content {
        serde_json::Value::String(s) => {
            let kind = if is_assistant {
                EventKind::AgentMessage
            } else {
                EventKind::UserMessage
            };
            out.events
                .push(text_event(session_id, ts_ms, kind, s, blobs));
        }
        serde_json::Value::Array(blocks) => {
            for block in blocks {
                if let Some(b) = block.as_object() {
                    parse_block(b, is_assistant, session_id, ts_ms, blobs, &mut out.events);
                }
            }
        }
        _ => {} // content is null/number/etc: skip
    }
    out
}

/// Parse one content block into events appended to `events`.
fn parse_block(
    b: &serde_json::Map<String, serde_json::Value>,
    is_assistant: bool,
    session_id: &str,
    ts_ms: i64,
    blobs: &BlobStore,
    events: &mut Vec<Event>,
) {
    let bty = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match bty {
        "text" => {
            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                let kind = if is_assistant {
                    EventKind::AgentMessage
                } else {
                    EventKind::UserMessage
                };
                events.push(text_event(session_id, ts_ms, kind, t, blobs));
            }
        }
        "thinking" => {
            if let Some(t) = b.get("thinking").and_then(|v| v.as_str()) {
                events.push(text_event(session_id, ts_ms, EventKind::Thinking, t, blobs));
            }
        }
        "tool_use" => {
            let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let input = b.get("input").cloned().unwrap_or(serde_json::Value::Null);
            events.push(tool_call_event(session_id, ts_ms, id, name, &input, blobs));
        }
        "tool_result" => {
            let tool_use_id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
            let is_error = b
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let content_text = extract_result_content(b.get("content"));
            events.push(tool_result_event(
                session_id,
                ts_ms,
                tool_use_id,
                is_error,
                &content_text,
                blobs,
            ));
        }
        _ => {} // unknown block type: skip (tolerant)
    }
}

/// Build a text-bearing event (`user_message`/`agent_message`/`thinking`).
fn text_event(
    session_id: &str,
    ts_ms: i64,
    kind: EventKind,
    text: &str,
    blobs: &BlobStore,
) -> Event {
    let body = serde_json::json!({ "text": text });
    let (body_json, blob_hash, blob_size) = maybe_spill(body, blobs);
    Event {
        session_id: session_id.to_string(),
        ts_ms,
        kind,
        step: None, // assigned by the FR-3.4 pass
        summary: truncate_summary(text),
        body_json,
        blob_hash,
        blob_size,
        correlates: None,
    }
}

/// Build a `tool_call` event, correlating to its later `tool_result` by id.
fn tool_call_event(
    session_id: &str,
    ts_ms: i64,
    id: &str,
    name: &str,
    input: &serde_json::Value,
    blobs: &BlobStore,
) -> Event {
    let body = serde_json::json!({
        "name": name,
        "input": input,
        "correlate_key": id,
    });
    let (body_json, blob_hash, blob_size) = maybe_spill(body, blobs);
    Event {
        session_id: session_id.to_string(),
        ts_ms,
        kind: EventKind::ToolCall,
        step: None,
        summary: truncate_summary(&format!("tool_call: {name}")),
        body_json,
        blob_hash,
        blob_size,
        correlates: None,
    }
}

/// Build a `tool_result` event, correlating to its `tool_call` by `tool_use_id`.
fn tool_result_event(
    session_id: &str,
    ts_ms: i64,
    tool_use_id: &str,
    is_error: bool,
    content: &str,
    blobs: &BlobStore,
) -> Event {
    let body = serde_json::json!({
        "tool_use_id": tool_use_id,
        "is_error": is_error,
        "content": content,
        "correlate_key": tool_use_id,
    });
    let (body_json, blob_hash, blob_size) = maybe_spill(body, blobs);
    let summary = if is_error {
        format!("tool_result (error): {}", one_line(content))
    } else {
        format!("tool_result: {}", one_line(content))
    };
    Event {
        session_id: session_id.to_string(),
        ts_ms,
        kind: EventKind::ToolResult,
        step: None,
        summary: truncate_summary(&summary),
        body_json,
        blob_hash,
        blob_size,
        correlates: None,
    }
}

/// If `body` serializes to ≥ [`SPILLOVER_BYTES`], store it as a blob and return
/// an overflow envelope; otherwise return the body inline. Best-effort: a blob
/// write failure falls back to inline storage rather than dropping the event.
fn maybe_spill(
    body: serde_json::Value,
    blobs: &BlobStore,
) -> (Option<serde_json::Value>, Option<String>, Option<u64>) {
    let serialized = serde_json::to_vec(&body).unwrap_or_default();
    if serialized.len() >= SPILLOVER_BYTES {
        if let Ok(outcome) = blobs.put(&serialized) {
            let envelope = serde_json::json!({
                "overflow": true,
                "size": outcome.size,
                "blob_hash": outcome.hash,
                "encoding": "blob",
            });
            return (Some(envelope), Some(outcome.hash), Some(outcome.size));
        }
    }
    (Some(body), None, None)
}

/// Extract a tool_result's `content` as a string, joining text blocks if it is
/// an array. Empty string for missing/unknown shapes.
fn extract_result_content(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::with_capacity(arr.len());
            for b in arr {
                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                    parts.push(t.to_string());
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// Collapse newlines so a summary fits on one line (length capped by the caller).
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

/// Convert a Claude `timestamp` (RFC3339, e.g. `2026-07-02T06:14:40.699Z`) to
/// milliseconds relative to `started_at_unix_ms`. Returns `None` on parse
/// failure (the tailer falls back to wall-clock elapsed time).
pub(crate) fn ts_ms_from_iso(iso: &str, started_at_unix_ms: i64) -> Option<i64> {
    let dt =
        time::OffsetDateTime::parse(iso, &time::format_description::well_known::Rfc3339).ok()?;
    let unix_ms = dt.unix_timestamp_nanos() / 1_000_000;
    let unix_ms = i64::try_from(unix_ms).unwrap_or(i64::MAX);
    Some((unix_ms - started_at_unix_ms).max(0))
}

// ---------------------------------------------------------------------------
// Path / time helpers
// ---------------------------------------------------------------------------

/// The Claude Code projects directory: `$HOME/.claude/projects` (Unix) or
/// `%USERPROFILE%\.claude\projects` (Windows). `None` if neither env var is set.
#[must_use]
fn claude_projects_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(projects_dir_under(&PathBuf::from(home)))
}

/// The projects directory under a given home directory. Split from
/// [`claude_projects_dir`] so the path shape is unit-testable without mutating
/// process-global env vars (`HOME`/`USERPROFILE`) across parallel tests.
#[must_use]
pub(crate) fn projects_dir_under(home: &Path) -> PathBuf {
    home.join(".claude").join("projects")
}

/// Encode `cwd` as a Claude slug: every `/`, `\`, `.` — and `:`, so a Windows
/// drive-letter cwd like `C:\Users\me` yields `C--Users-me`, matching how
/// Claude Code names project dirs for Windows paths — becomes `-`. This is
/// inferred from sample transcripts (the SRS is absent); it is a *hint only* —
/// the tailer falls back to a cwd-based scan when the slug dir misses.
#[must_use]
pub(crate) fn slugify(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '\\' | '.' | ':' => '-',
            other => other,
        })
        .collect()
}

/// A `SystemTime` as unix-ms, clamped to `i64` range.
fn system_time_to_unix_ms(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Wall-clock ms elapsed since `start` (the tailer's fallback timestamp).
fn elapsed_ms(start: &Instant) -> i64 {
    i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// A user-visible adapter warning on stderr (one line, `hh:`-prefixed).
fn warn(msg: &str) {
    eprintln!("hh: warning: {msg}");
}

/// A debug trace on stderr, gated on the `HH_DEBUG` env var (no `log`/`tracing`
/// dep, per CLAUDE.md's "small, well-maintained crates" guidance).
fn debug(msg: &str) {
    if std::env::var_os("HH_DEBUG").is_some() {
        eprintln!("hh: debug: {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::Write;

    /// A fixed session start (2026-07-02T06:14:40.000Z) so timestamps derive
    /// small, stable, readable `ts_ms` values for snapshots.
    const STARTED: i64 = 1_782_972_880_000;
    const SID: &str = "test-session-id";

    fn iso(s: &str) -> i64 {
        ts_ms_from_iso(s, STARTED).expect("fixture timestamps must parse")
    }

    fn parse(value: &Value, ts_ms: i64) -> ParsedRecord {
        let tmp = tempfile::TempDir::new().unwrap();
        let blobs = BlobStore::new(tmp.path().join("blobs"));
        parse_record(value, SID, ts_ms, &blobs)
    }

    /// Serialize a parsed record's events (and model/usage when present) as
    /// pretty JSON for a stable insta snapshot.
    fn snap(pr: &ParsedRecord) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "events": pr.events,
            "is_assistant_message": pr.is_assistant_message,
        }))
        .unwrap()
    }

    fn rec(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn parse_user_text_prompt() {
        let v = rec(r#"{"type":"user","isSidechain":false,"isMeta":false,
            "message":{"role":"user","content":"Please list the files in the repo and show Cargo.toml."},
            "timestamp":"2026-07-02T06:14:40.699Z","cwd":"/tmp/work","sessionId":"s"}"#);
        let pr = parse(&v, iso("2026-07-02T06:14:40.699Z"));
        insta::assert_snapshot!(snap(&pr));
    }

    #[test]
    fn parse_assistant_with_two_tool_uses() {
        let v = rec(r#"{"type":"assistant","isSidechain":false,"isMeta":false,
            "message":{"role":"assistant","model":"glm-5.2","content":[
              {"type":"thinking","thinking":"I'll list files then read Cargo.toml.","signature":""},
              {"type":"tool_use","id":"call_abc123","name":"Bash","input":{"command":"ls -la"}},
              {"type":"tool_use","id":"call_def456","name":"Read","input":{"file_path":"Cargo.toml"}}
            ],"usage":{"input_tokens":1200,"output_tokens":80}},
            "timestamp":"2026-07-02T06:14:41.500Z","cwd":"/tmp/work","sessionId":"s"}"#);
        let pr = parse(&v, iso("2026-07-02T06:14:41.500Z"));
        insta::assert_snapshot!(snap(&pr));
        // thinking + two tool calls = 3 events; assistant message flagged.
        assert_eq!(pr.events.len(), 3);
        assert!(pr.is_assistant_message);
        assert_eq!(pr.events[0].kind, EventKind::Thinking);
        assert_eq!(pr.events[1].kind, EventKind::ToolCall);
        assert_eq!(pr.events[2].kind, EventKind::ToolCall);
    }

    #[test]
    fn parse_user_with_two_tool_results_one_error() {
        let v = rec(r#"{"type":"user","isSidechain":false,"isMeta":false,
            "message":{"role":"user","content":[
              {"type":"tool_result","tool_use_id":"call_abc123","content":"total 0","is_error":false},
              {"type":"tool_result","tool_use_id":"call_def456","content":"Error: file not found","is_error":true}
            ]},
            "timestamp":"2026-07-02T06:14:42.800Z","cwd":"/tmp/work","sessionId":"s"}"#);
        let pr = parse(&v, iso("2026-07-02T06:14:42.800Z"));
        insta::assert_snapshot!(snap(&pr));
        assert_eq!(pr.events.len(), 2);
        assert_eq!(pr.events[0].kind, EventKind::ToolResult);
        assert_eq!(pr.events[1].kind, EventKind::ToolResult);
        // correlate_key == tool_use_id in each body.
        assert_eq!(
            pr.events[0].body_json.as_ref().unwrap()["correlate_key"],
            "call_abc123"
        );
        assert!(pr.events[1].body_json.as_ref().unwrap()["is_error"]
            .as_bool()
            .unwrap());
    }

    #[test]
    fn parse_assistant_followup_with_model_usage() {
        let v = rec(r#"{"type":"assistant","isSidechain":false,"isMeta":false,
            "message":{"role":"assistant","model":"glm-5.2","content":[
              {"type":"text","text":"The repo is empty and Cargo.toml is missing."}
            ],"usage":{"input_tokens":1500,"output_tokens":40,"cache_read_input_tokens":300}},
            "timestamp":"2026-07-02T06:14:43.100Z","cwd":"/tmp/work","sessionId":"s"}"#);
        let pr = parse(&v, iso("2026-07-02T06:14:43.100Z"));
        insta::assert_snapshot!(snap(&pr));
        assert_eq!(pr.events.len(), 1);
        assert_eq!(pr.events[0].kind, EventKind::AgentMessage);
        // The tailer collects model/usage from the message; exercise that path.
        let mut acc = OutcomeAcc::default();
        acc.note_assistant(v.get("message").unwrap());
        assert_eq!(acc.model.as_deref(), Some("glm-5.2"));
        assert_eq!(acc.usage_json.as_ref().unwrap()["output_tokens"], 40);
    }

    #[test]
    fn parse_skips_unknown_type() {
        let v = rec(r#"{"type":"system","subtype":"init","cwd":"/tmp/work","sessionId":"s"}"#);
        let pr = parse(&v, 0);
        assert!(pr.events.is_empty());
        assert!(!pr.is_assistant_message);
    }

    #[test]
    fn parse_skips_non_object_value() {
        // A malformed line that parsed as a JSON array/null produces no events.
        for v in [rec("[]"), rec("null"), rec("\"oops\"")] {
            assert!(
                parse(&v, 0).events.is_empty(),
                "non-object must yield no events"
            );
        }
    }

    #[test]
    fn parse_skips_is_sidechain() {
        let v = rec(r#"{"type":"assistant","isSidechain":true,"isMeta":false,
            "message":{"role":"assistant","content":[{"type":"text","text":"subagent"}]},
            "timestamp":"2026-07-02T06:14:41.000Z","cwd":"/tmp/work","sessionId":"s"}"#);
        assert!(parse(&v, 0).events.is_empty());
    }

    #[test]
    fn parse_skips_is_meta() {
        let v = rec(r#"{"type":"user","isSidechain":false,"isMeta":true,
            "message":{"role":"user","content":"<local-command-caveat>system-reminder</local-command-caveat>"},
            "timestamp":"2026-07-02T06:14:40.600Z","cwd":"/tmp/work","sessionId":"s"}"#);
        assert!(parse(&v, 0).events.is_empty());
    }

    #[test]
    fn parse_256kib_spills_to_blob() {
        let big = "x".repeat(SPILLOVER_BYTES + 1024);
        let v = rec(&format!(
            r#"{{"type":"user","isSidechain":false,"isMeta":false,
            "message":{{"role":"user","content":{}}},
            "timestamp":"2026-07-02T06:14:40.699Z","cwd":"/tmp/work","sessionId":"s"}}"#,
            serde_json::Value::String(big)
        ));
        let tmp = tempfile::TempDir::new().unwrap();
        let blobs = BlobStore::new(tmp.path().join("blobs"));
        let pr = parse_record(&v, SID, 699, &blobs);
        let ev = &pr.events[0];
        assert!(ev.blob_hash.is_some(), "large payload must spill to a blob");
        let hash = ev.blob_hash.as_ref().unwrap();
        assert_eq!(ev.body_json.as_ref().unwrap()["overflow"], true);
        assert_eq!(ev.body_json.as_ref().unwrap()["encoding"], "blob");
        // The blob round-trips to the serialized body (a JSON object with the text).
        let raw = blobs.get(hash).unwrap();
        let body: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(body["text"].as_str().unwrap().len(), SPILLOVER_BYTES + 1024);
    }

    #[test]
    fn slugify_replaces_slash_and_dot() {
        assert_eq!(
            slugify(Path::new("/home/saadman/switch")),
            "-home-saadman-switch"
        );
        assert_eq!(
            slugify(Path::new("/home/saadman/switch/.claude/worktrees/x")),
            "-home-saadman-switch--claude-worktrees-x"
        );
    }

    #[test]
    fn slugify_windows_style_cwds() {
        // Typed PathBuf fixtures (not string munging): Windows-style cwds are
        // constructed as PathBuf values and slugified. `slugify` operates on
        // the lossy string form, so `\` components behave identically on every
        // host platform — these assertions hold on Unix CI too.
        let drive_root = PathBuf::from(r"C:\Users\me");
        assert_eq!(slugify(&drive_root), "C--Users-me");
        let nested = PathBuf::from(r"C:\Users\me\my.project\sub");
        assert_eq!(slugify(&nested), "C--Users-me-my-project-sub");
        // A UNC-style path: every separator collapses to `-`.
        let unc = PathBuf::from(r"\\server\share\proj");
        assert_eq!(slugify(&unc), "--server-share-proj");
    }

    #[test]
    fn projects_dir_shape_per_platform_home() {
        // Typed PathBuf fixtures for both home layouts. Built with join() so
        // the expected value uses the platform-native separator — this is a
        // path-shape test, not a string test.
        let unix_home = PathBuf::from("/home/me");
        assert_eq!(
            projects_dir_under(&unix_home),
            PathBuf::from("/home/me").join(".claude").join("projects")
        );
        let win_home = PathBuf::from(r"C:\Users\me");
        assert_eq!(
            projects_dir_under(&win_home),
            PathBuf::from(r"C:\Users\me")
                .join(".claude")
                .join("projects")
        );
    }

    #[test]
    fn select_detects_claude_basename() {
        assert!(is_claude_code(&["claude".into()]));
        assert!(is_claude_code(&["/usr/local/bin/claude".into()]));
        assert!(is_claude_code(&["C:\\Apps\\claude.exe".into()]));
        assert!(!is_claude_code(&["python3".into()]));
        assert!(!is_claude_code(&[]));
        // select returns a Claude adapter for claude, None for a generic command.
        assert!(select(&["claude".into()], Path::new("/tmp")).is_some());
        assert!(select(&["python3".into()], Path::new("/tmp")).is_none());
    }

    #[test]
    fn ts_ms_from_iso_is_relative_and_clamped() {
        assert_eq!(
            ts_ms_from_iso("2026-07-02T06:14:40.699Z", STARTED),
            Some(699)
        );
        assert_eq!(ts_ms_from_iso("2026-07-02T06:14:40.000Z", STARTED), Some(0));
        // A timestamp before the session start clamps to 0 (no negative ts_ms).
        assert_eq!(ts_ms_from_iso("2026-07-02T06:14:39.000Z", STARTED), Some(0));
        assert!(ts_ms_from_iso("not-a-date", STARTED).is_none());
    }

    // --- tailer behavior (real threads, temp HOME) -----------------------

    /// A self-contained adapter spawn: temp HOME + cwd, a transcript file, and a
    /// stop flag. Returns the handle so the test can drain events then join.
    fn spawn_with(
        home: &Path,
        cwd: &Path,
        transcript: &str,
    ) -> (AdapterHandle, std::sync::Arc<AtomicBool>) {
        std::fs::create_dir_all(home.join(".claude").join("projects").join(slugify(cwd))).unwrap();
        std::fs::write(
            home.join(".claude")
                .join("projects")
                .join(slugify(cwd))
                .join("session.jsonl"),
            transcript,
        )
        .unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let blobs = std::sync::Arc::new(BlobStore::new(home.join("blobs")));
        let ctx = AdapterContext {
            session_id: SID.to_string(),
            started_at_unix_ms: STARTED,
            cwd: cwd.to_path_buf(),
            command: vec!["claude".into()],
            blobs,
            stop: std::sync::Arc::clone(&stop),
            projects_dir: Some(home.join(".claude").join("projects")),
        };
        let handle = Box::new(ClaudeAdapter).spawn(ctx).expect("spawn adapter");
        (handle, stop)
    }

    /// Drain `handle.events` until EOF, collecting events by kind.
    fn drain(events: &mpsc::Receiver<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(ev) = events.recv() {
            out.push(ev);
        }
        out
    }

    const FIXTURE: &str = "\
{\"type\":\"system\",\"cwd\":\"/tmp/work\",\"sessionId\":\"s\",\"timestamp\":\"2026-07-02T06:14:40.500Z\"}
{\"type\":\"user\",\"isSidechain\":false,\"isMeta\":true,\"message\":{\"role\":\"user\",\"content\":\"caveat\"},\"timestamp\":\"2026-07-02T06:14:40.600Z\",\"cwd\":\"/tmp/work\",\"sessionId\":\"s\"}
{\"type\":\"user\",\"isSidechain\":false,\"isMeta\":false,\"message\":{\"role\":\"user\",\"content\":\"hello\"},\"timestamp\":\"2026-07-02T06:14:40.699Z\",\"cwd\":\"/tmp/work\",\"sessionId\":\"s\"}
{\"type\":\"assistant\",\"isSidechain\":true,\"isMeta\":false,\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"side\"}]},\"timestamp\":\"2026-07-02T06:14:41.000Z\",\"cwd\":\"/tmp/work\",\"sessionId\":\"s\"}
{\"type\":\"assistant\",\"isSidechain\":false,\"isMeta\":false,\"message\":{\"role\":\"assistant\",\"model\":\"glm-5.2\",\"content\":[{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}}]},\"timestamp\":\"2026-07-02T06:14:41.500Z\",\"cwd\":\"/tmp/work\",\"sessionId\":\"s\"}
{\"type\":\"user\",\"isSidechain\":false,\"isMeta\":false,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"call_1\",\"content\":\"ok\",\"is_error\":false}]},\"timestamp\":\"2026-07-02T06:14:42.800Z\",\"cwd\":\"/tmp/work\",\"sessionId\":\"s\"}
";

    #[test]
    fn tailer_parses_fixture_and_skips_filtered_records() {
        let home = tempfile::TempDir::new().unwrap();
        let cwd = Path::new("/tmp/work");
        let (handle, stop) = spawn_with(home.path(), cwd, FIXTURE);
        // Give the tailer a moment to read the file, then stop + drain.
        std::thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::Release);
        let events = drain(&handle.events);
        // system + isMeta + isSidechain skipped → user_message + tool_call + tool_result.
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::UserMessage,
                EventKind::ToolCall,
                EventKind::ToolResult
            ]
        );
    }

    #[test]
    fn tailer_finds_file_late() {
        let home = tempfile::TempDir::new().unwrap();
        let cwd = Path::new("/tmp/work-late");
        // Pre-create the slug dir but write the file *after* spawning, so the
        // adapter must poll for it within FILE_APPEAR_TIMEOUT.
        std::fs::create_dir_all(
            home.path()
                .join(".claude")
                .join("projects")
                .join(slugify(cwd)),
        )
        .unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let blobs = std::sync::Arc::new(BlobStore::new(home.path().join("blobs")));
        let ctx = AdapterContext {
            session_id: SID.to_string(),
            started_at_unix_ms: STARTED,
            cwd: cwd.to_path_buf(),
            command: vec!["claude".into()],
            blobs,
            stop: std::sync::Arc::clone(&stop),
            projects_dir: Some(home.path().join(".claude").join("projects")),
        };
        let handle = Box::new(ClaudeAdapter).spawn(ctx).expect("spawn");
        // Write the transcript after the adapter is already polling.
        std::thread::sleep(Duration::from_millis(150));
        std::fs::write(
            home
                .path()
                .join(".claude")
                .join("projects")
                .join(slugify(cwd))
                .join("late.jsonl"),
            "{\"type\":\"user\",\"isSidechain\":false,\"isMeta\":false,\"message\":{\"role\":\"user\",\"content\":\"late\"},\"timestamp\":\"2026-07-02T06:14:40.699Z\",\"cwd\":\"/tmp/work-late\",\"sessionId\":\"s\"}\n",
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::Release);
        let events = drain(&handle.events);
        assert_eq!(
            events.len(),
            1,
            "adapter should pick up the late-appearing file"
        );
        assert_eq!(events[0].kind, EventKind::UserMessage);
    }

    #[test]
    fn tailer_partial_last_line_retries() {
        let home = tempfile::TempDir::new().unwrap();
        let cwd = Path::new("/tmp/work-partial");
        // A file with one complete line plus a trailing partial (no newline).
        let complete = "{\"type\":\"user\",\"isSidechain\":false,\"isMeta\":false,\"message\":{\"role\":\"user\",\"content\":\"first\"},\"timestamp\":\"2026-07-02T06:14:40.699Z\",\"cwd\":\"/tmp/work-partial\",\"sessionId\":\"s\"}\n";
        let partial = "{\"type\":\"user\",\"isSidechain\":false,\"isMeta\":false,\"message\":{\"role\":\"user\",\"content\":\"sec";
        let path = home
            .path()
            .join(".claude")
            .join("projects")
            .join(slugify(cwd))
            .join("partial.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{complete}{partial}")).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let blobs = std::sync::Arc::new(BlobStore::new(home.path().join("blobs")));
        let ctx = AdapterContext {
            session_id: SID.to_string(),
            started_at_unix_ms: STARTED,
            cwd: cwd.to_path_buf(),
            command: vec!["claude".into()],
            blobs,
            stop: std::sync::Arc::clone(&stop),
            projects_dir: Some(home.path().join(".claude").join("projects")),
        };
        let handle = Box::new(ClaudeAdapter).spawn(ctx).expect("spawn");
        std::thread::sleep(Duration::from_millis(150));
        // Append the rest of the partial line; it should now parse as a second event.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "ond\"}}}}\n").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::Release);
        let events = drain(&handle.events);
        let contents: Vec<&str> = events
            .iter()
            .map(|e| e.body_json.as_ref().unwrap()["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            contents,
            vec!["first", "second"],
            "partial line must be retried until complete"
        );
    }

    #[test]
    fn tailer_fallback_scan_by_cwd() {
        let home = tempfile::TempDir::new().unwrap();
        let cwd = Path::new("/tmp/work-fallback");
        // Put the transcript under a *wrong* slug dir (not slugify(cwd)) so the
        // slug lookup misses and the fallback scan by cwd must find it.
        let wrong_slug = "-totally-wrong-slug";
        std::fs::create_dir_all(
            home.path()
                .join(".claude")
                .join("projects")
                .join(wrong_slug),
        )
        .unwrap();
        std::fs::write(
            home
                .path()
                .join(".claude")
                .join("projects")
                .join(wrong_slug)
                .join("fb.jsonl"),
            "{\"type\":\"user\",\"isSidechain\":false,\"isMeta\":false,\"message\":{\"role\":\"user\",\"content\":\"fb\"},\"timestamp\":\"2026-07-02T06:14:40.699Z\",\"cwd\":\"/tmp/work-fallback\",\"sessionId\":\"s\"}\n",
        )
        .unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let blobs = std::sync::Arc::new(BlobStore::new(home.path().join("blobs")));
        let ctx = AdapterContext {
            session_id: SID.to_string(),
            started_at_unix_ms: STARTED,
            cwd: cwd.to_path_buf(),
            command: vec!["claude".into()],
            blobs,
            stop: std::sync::Arc::clone(&stop),
            projects_dir: Some(home.path().join(".claude").join("projects")),
        };
        let handle = Box::new(ClaudeAdapter).spawn(ctx).expect("spawn");
        std::thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::Release);
        let events = drain(&handle.events);
        assert_eq!(
            events.len(),
            1,
            "fallback scan should locate the transcript by cwd"
        );
        assert_eq!(events[0].kind, EventKind::UserMessage);
        let outcome = handle.outcome.join().expect("tailer thread");
        assert_eq!(outcome.status, AdapterStatus::Active);
    }

    #[test]
    fn tailer_degrades_when_projects_dir_missing() {
        // No ~/.claude/projects anywhere: HOME points at an empty temp dir.
        let home = tempfile::TempDir::new().unwrap();
        let cwd = Path::new("/tmp/nowhere");
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let blobs = std::sync::Arc::new(BlobStore::new(home.path().join("blobs")));
        let ctx = AdapterContext {
            session_id: SID.to_string(),
            started_at_unix_ms: STARTED,
            cwd: cwd.to_path_buf(),
            command: vec!["claude".into()],
            blobs,
            stop: std::sync::Arc::clone(&stop),
            projects_dir: Some(home.path().join(".claude").join("projects")),
        };
        let handle = Box::new(ClaudeAdapter).spawn(ctx).expect("spawn");
        let events = drain(&handle.events);
        assert!(events.is_empty(), "no projects dir → no events");
        let outcome = handle.outcome.join().expect("tailer thread");
        assert_eq!(outcome.status, AdapterStatus::Degraded);
    }
}
