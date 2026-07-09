//! MCP stdio proxy (FR-2): a newline-delimited JSON-RPC middleman between an
//! MCP client (the host on `hh`'s stdin/stdout) and an MCP server (the child
//! process). It forwards every line **verbatim** (wire correctness preserved),
//! and as a side effect records each message as an Halfhand event:
//!
//! - client → server: a request (`id` + `method`) → [`EventKind::McpRequest`]
//!   with `correlate_key = <id>`; a notification (`method`, no `id`) →
//!   [`EventKind::McpNotification`].
//! - server → client: a response (`id`, no `method`) → [`EventKind::McpResponse`]
//!   with `correlates` pointing at the matching request's event id and
//!   `latency_ms` in the body; a notification → [`EventKind::McpNotification`].
//! - a JSON-array batch (MCP 2025-03-26) is forwarded verbatim and recorded as
//!   one [`EventKind::McpNotification`] (v0.1 simplification: no per-element
//!   correlation, flagged in the plan).
//!
//! Sessions (FR-2.3): if `HH_SESSION_ID` is set, the proxy **attaches** to that
//! session — it records events into it but leaves its lifecycle (`status`,
//! `ended_at`) untouched, because the parent `hh run` owns finalize. A missing
//! id is an actionable error (no orphan session). With no env var, the proxy
//! creates a standalone `mcp-only` session and finalizes it with the server's
//! exit code.
//!
//! Concurrency (ADR-0001): two OS threads (upstream/downstream) share an
//! `Arc<Mutex<EventWriter>>` and an `Arc<Mutex<HashMap<…>>>` correlation map.
//! Lock ordering is deadlock-free: acquire the map → copy/insert → release →
//! acquire the writer → append → release. The two locks are **never held
//! together** (CLAUDE.md single-writer rule; the `Mutex` serializes appends).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hh_core::blob::BlobStore;
use hh_core::event::{AdapterStatus, AgentKind, Event, EventKind, NewSession, SessionStatus};
use hh_core::store::{EventWriter, Store};

use crate::git::GitMeta;

/// Blob spillover threshold (matches the adapter and PTY capture): payloads
/// whose serialized JSON is ≥ this many bytes go to the blob store.
const SPILL_BYTES: usize = 256 * 1024;

/// Options for [`run_mcp_proxy`], built by the binary from `McpProxyArgs`.
#[derive(Debug, Clone)]
pub struct McpProxyOptions {
    /// The MCP server argv (program + args).
    pub command: Vec<String>,
    /// Working directory for the server process.
    pub cwd: PathBuf,
    /// Human-readable hint stored as the session `command` label for a
    /// standalone `mcp-only` session (unused when attaching).
    pub session_hint: Option<String>,
    /// `hh` version string (FR-1.2).
    pub hh_version: String,
}

/// The outcome of a finished proxy run (FR-2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpProxyOutcome {
    /// Full session UUID string (the attached parent's, or the new standalone
    /// session's).
    pub session_id: String,
    /// 6-hex-char short id (the standalone session's; empty-ish for attached,
    /// resolved best-effort from the parent row).
    pub short_id: String,
    /// `true` if the proxy attached to an existing session via `HH_SESSION_ID`.
    pub attached: bool,
    /// Server exit code, if it could be determined.
    pub exit_code: Option<i32>,
}

/// Run the MCP stdio proxy (FR-2). See the module docs for the session model
/// and the forwarding/recording contract.
///
/// # Errors
///
/// Returns [`crate::RecordError`] for setup failures (server spawn, store
/// errors, an unresolvable `HH_SESSION_ID`). A server that exits nonzero is
/// *not* an error here — it's reflected in `McpProxyOutcome.exit_code`.
#[allow(clippy::too_many_lines)] // the proxy loop is inherently linear
pub fn run_mcp_proxy(store: &Store, opts: &McpProxyOptions) -> crate::Result<McpProxyOutcome> {
    let now = now_unix_ms();

    // --- Session (FR-2.3) ------------------------------------------------
    // Attached: HH_SESSION_ID present → resolve + read the parent's started_at
    // so MCP events share the parent's timeline. Standalone: create an mcp-only
    // session we own and finalize at the end.
    let attached_id = std::env::var_os("HH_SESSION_ID").and_then(|v| v.into_string().ok());
    let (session_id, short_id, base_started_at, attached) = if let Some(id) = attached_id.as_deref()
    {
        let resolved = store.resolve_session(id).map_err(|e| {
            crate::RecordError::McpSession(format!(
                "session `{id}` not found ({e}); run `hh mcp-proxy -- …` standalone, \
                 or re-run inside `hh run` so the proxy can attach to a live session"
            ))
        })?;
        let parent_started = store.session_started_at(&resolved).map_err(|e| {
            crate::RecordError::McpSession(format!(
                "could not read start time of session `{resolved}`: {e}"
            ))
        })?;
        let short = store.session_short_id(&resolved).unwrap_or_default();
        (resolved, short, parent_started, true)
    } else {
        let session_uuid = hh_core::event::now_v7();
        let git = GitMeta::capture(&opts.cwd);
        // The session `command` records the proxy wrapping the server argv
        // (`mcp-proxy` prefix + server command) so `hh list` shows what was
        // proxied. `session_hint` is a forward-compat label (no name column
        // yet); it is not persisted in v0.1.
        let mut command = vec!["mcp-proxy".to_string()];
        command.extend_from_slice(&opts.command);
        let new_session = NewSession {
            id: session_uuid,
            started_at: now,
            agent_kind: AgentKind::McpOnly,
            adapter_status: AdapterStatus::None,
            command,
            cwd: opts.cwd.clone(),
            hostname: hostname(),
            hh_version: opts.hh_version.clone(),
            model: None,
            git_branch: git.branch,
            git_sha: git.sha,
            git_dirty: git.dirty,
        };
        let created = store.create_session(&new_session)?;
        (created.id, created.short_id, now, false)
    };

    let writer =
        Arc::new(Mutex::new(store.event_writer().map_err(|e| {
            crate::RecordError::Mcp(format!("open event writer: {e}"))
        })?));
    let blobs = Arc::new(BlobStore::new(store.blobs().root().to_path_buf()));
    // JSON-RPC id (string repr) → (request event id, request unix-ms ts).
    let pending: Arc<Mutex<HashMap<String, (i64, i64)>>> = Arc::new(Mutex::new(HashMap::new()));
    let stop = Arc::new(AtomicBool::new(false));

    // --- Spawn the server (FR-2.1) --------------------------------------
    let mut cmd = Command::new(&opts.command[0]);
    cmd.args(&opts.command[1..])
        .current_dir(&opts.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd
        .spawn()
        .map_err(|e| crate::RecordError::Mcp(format!("spawn server `{}`: {e}", opts.command[0])))?;
    let server_stdin = child
        .stdin
        .take()
        .ok_or_else(|| crate::RecordError::Mcp("server stdin not piped".into()))?;
    let server_stdout = child
        .stdout
        .take()
        .ok_or_else(|| crate::RecordError::Mcp("server stdout not piped".into()))?;

    // --- Two threads: upstream (hh stdin → server) + downstream (server → hh stdout)
    // Shared recording state (writer + blobs + correlation map + session clock)
    // is bundled in `ProxyCtx` so the thread entrypoints stay under the
    // pedantic argument limit and the lock ordering lives in one place.
    let ctx = ProxyCtx {
        writer: Arc::clone(&writer),
        blobs: Arc::clone(&blobs),
        pending: Arc::clone(&pending),
        session_id: session_id.clone(),
        base_started_at,
    };
    let up_stop = Arc::clone(&stop);
    let upstream = std::thread::Builder::new()
        .name("hh-mcp-upstream".into())
        .spawn({
            let ctx = ctx.clone();
            move || run_upstream(server_stdin, ctx, up_stop)
        })
        .map_err(|e| crate::RecordError::Mcp(format!("spawn upstream thread: {e}")))?;

    let down_stop = Arc::clone(&stop);
    let mut stdout = std::io::stdout();
    let downstream = std::thread::Builder::new()
        .name("hh-mcp-downstream".into())
        .spawn({
            let ctx = ctx.clone();
            move || run_downstream(server_stdout, &mut stdout, ctx, down_stop)
        })
        .map_err(|e| crate::RecordError::Mcp(format!("spawn downstream thread: {e}")))?;

    // Wait for both threads. The upstream thread ends when hh's stdin EOFs; it
    // closes the server's stdin, which makes the server exit, which EOFs the
    // downstream reader. `stop` is a backstop for a server that never exits.
    let _ = upstream.join();
    let _ = downstream.join();
    stop.store(true, Ordering::Release);

    let exit_code = wait_for_child(&mut child);

    // --- Finalize (standalone only) -------------------------------------
    // Flush + close the writer first (no intra-process contention: the threads
    // have joined), then assign step ordinals + finalize the session row.
    // Recovers from poisoning (see `ProxyCtx::append`) rather than failing
    // finalize outright — a panic in either proxy thread must not also abort it.
    {
        let mut w = writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.flush()
            .map_err(|e| crate::RecordError::Mcp(format!("flush writer: {e}")))?;
        w.close()
            .map_err(|e| crate::RecordError::Mcp(format!("close writer: {e}")))?;
    }
    if attached {
        // Attached: leave the parent's lifecycle untouched. Self-heal steps so
        // the late MCP events get ordinals without waiting for the next `hh`
        // invocation (best-effort; a cross-process race is still possible).
        let _ = store.assign_steps(&session_id);
    } else {
        let ended_at = now_unix_ms();
        let status = match exit_code {
            Some(0) => SessionStatus::Ok,
            Some(_) => SessionStatus::Error,
            None => SessionStatus::Interrupted,
        };
        store
            .assign_steps(&session_id)
            .map_err(|e| crate::RecordError::Mcp(format!("assign steps: {e}")))?;
        store
            .finalize_session(&session_id, ended_at, exit_code, status)
            .map_err(|e| crate::RecordError::Mcp(format!("finalize session: {e}")))?;
    }

    Ok(McpProxyOutcome {
        session_id,
        short_id,
        attached,
        exit_code,
    })
}

/// Shared recording state for the two proxy threads. Bundling the writer, blob
/// store, correlation map, and session clock here keeps the thread entrypoints
/// under the pedantic argument limit and centralizes the lock-ordering rule
/// (map → release → writer → release; the two locks are never held together).
#[derive(Clone)]
struct ProxyCtx {
    /// Single-writer event sink (CLAUDE.md intra-process rule; the `Mutex`
    /// serializes appends from both threads).
    writer: Arc<Mutex<EventWriter>>,
    /// Blob store for >256 KiB spillover.
    blobs: Arc<BlobStore>,
    /// JSON-RPC id (string repr) → (request event id, request absolute unix-ms
    /// ts). Populated by upstream; consumed by downstream for `correlates` +
    /// `latency_ms`.
    pending: Arc<Mutex<HashMap<String, (i64, i64)>>>,
    /// Session id events are recorded into (the attached parent's, or the
    /// standalone mcp-only session's).
    session_id: String,
    /// `started_at` of the session whose timeline these events share. Event
    /// `ts_ms` is `now_unix_ms() - base_started_at` so attached MCP events
    /// interleave correctly on the parent's clock (FR-2.3).
    base_started_at: i64,
}

impl ProxyCtx {
    /// Append `event` under the shared writer lock, returning the assigned row
    /// id on success. A poisoned lock is recovered rather than treated as a
    /// failure (a panic in the sibling thread must not blind this one too);
    /// store errors are still swallowed (best-effort recording; forwarding
    /// always continues).
    fn append(&self, event: Event) -> Option<i64> {
        let w = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.append_event(event).ok()
    }

    /// Look up a pending request by correlation key, returning
    /// `(correlates_event_id, request_absolute_ts)`. Lock ordering: acquire the
    /// map, copy out, release — never nest with the writer lock.
    fn take_pending(&self, key: &str) -> Option<(i64, i64)> {
        self.pending.lock().ok()?.get(key).copied()
    }

    /// Register a pending request (id → (event id, absolute ts)). Lock ordering:
    /// the writer lock is already released by the caller before this is called.
    fn register_pending(&self, key: String, event_id: i64, ts_abs: i64) {
        if let Ok(mut m) = self.pending.lock() {
            m.insert(key, (event_id, ts_abs));
        }
    }

    /// Build an event with the common fields filled, spilling `body` to a blob
    /// if it is ≥ [`SPILL_BYTES`].
    fn event(
        &self,
        kind: EventKind,
        ts_ms: i64,
        summary: String,
        body: serde_json::Value,
    ) -> Event {
        let (body_json, blob_hash, blob_size) = maybe_spill(body, &self.blobs);
        Event {
            session_id: self.session_id.clone(),
            ts_ms,
            kind,
            step: None,
            summary,
            body_json,
            blob_hash,
            blob_size,
            correlates: None,
        }
    }
}

/// Upstream: read NDJSON from `hh`'s stdin, classify each line, record it, and
/// forward the raw bytes verbatim to the server's stdin. Closes the server's
/// stdin on EOF, which is how the server learns the client is done.
#[allow(clippy::needless_pass_by_value)] // ctx + stop are moved into the spawned thread; only refs are used inside, but ownership transfer is the point (no 'static ref can cross the thread boundary).
fn run_upstream(mut server_stdin: impl Write + Send, ctx: ProxyCtx, stop: Arc<AtomicBool>) {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = Vec::with_capacity(8 * 1024);
    while !stop.load(Ordering::Acquire) {
        line.clear();
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(0) => break, // stdin EOF
            Ok(n) => n,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::Interrupted {
                    break;
                }
                continue;
            }
        };
        // Classify + record (best-effort), then forward the raw bytes verbatim.
        record_upstream_line(line.as_slice(), &ctx);
        if server_stdin.write_all(&line[..n]).is_err() {
            break; // server closed its stdin
        }
        let _ = server_stdin.flush();
    }
    // Closing the server's stdin signals end-of-input; the server then exits.
    let _ = server_stdin.flush();
}

/// Downstream: read NDJSON from the server's stdout, classify each line, record
/// it (resolving `correlates` + `latency_ms` for responses), and forward the
/// raw bytes verbatim to `hh`'s stdout.
#[allow(clippy::needless_pass_by_value)] // ctx + stop are moved into the spawned thread; only refs are used inside, but ownership transfer is the point (no 'static ref can cross the thread boundary).
fn run_downstream(
    server_stdout: impl Read + Send,
    stdout: &mut (impl Write + Send),
    ctx: ProxyCtx,
    stop: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(server_stdout);
    let mut line = Vec::with_capacity(8 * 1024);
    while !stop.load(Ordering::Acquire) {
        line.clear();
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(0) => break, // server stdout EOF (server exited)
            Ok(n) => n,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::Interrupted {
                    break;
                }
                continue;
            }
        };
        record_downstream_line(line.as_slice(), &ctx);
        if stdout.write_all(&line[..n]).is_err() {
            break; // client closed its stdout
        }
        let _ = stdout.flush();
    }
}

/// Classify a client→server line and append an event. A request (`id` +
/// `method`) registers its id → (event id, ts) in `pending` for the matching
/// response; a notification (`method`, no `id`) records an `mcp_notification`.
/// Batches (JSON arrays) record one `mcp_notification`. Unparseable lines are
/// forwarded but not recorded (wire correctness > recording completeness).
fn record_upstream_line(line: &[u8], ctx: &ProxyCtx) {
    let Some(parsed) = parse_json_line(line) else {
        return;
    };
    let now_abs = now_unix_ms();
    let ts_ms = now_abs - ctx.base_started_at;
    match &parsed {
        serde_json::Value::Object(obj) => {
            // Clone the small classifier fields out of the borrow scope so
            // `parsed` can be moved into the event body below without E0505.
            let id = obj.get("id").cloned();
            let method = obj
                .get("method")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            match (id.as_ref(), method.as_deref()) {
                (Some(id_val), Some(method)) => {
                    // Request: record + register the correlation key.
                    let key = id_key(id_val);
                    let summary = truncate_summary(&format!("mcp request: {method}"));
                    let body = serde_json::to_value(&parsed).unwrap_or_else(|_| parsed.clone());
                    let mut event = ctx.event(EventKind::McpRequest, ts_ms, summary, body);
                    if let Some(k) = &key {
                        if let Some(b) = event.body_json.as_mut() {
                            if let Some(o) = b.as_object_mut() {
                                o.insert("correlate_key".into(), serde_json::json!(k));
                            }
                        }
                    }
                    let event_id = ctx.append(event);
                    if let (Some(k), Some(eid)) = (key, event_id) {
                        // Lock ordering: the writer lock is already released
                        // (append returned) before the map lock is taken — never
                        // nested.
                        ctx.register_pending(k, eid, now_abs);
                    }
                }
                (None, Some(method)) => record_notification(ctx, ts_ms, method, parsed),
                // Object with neither id nor method, or id without method:
                // malformed MCP; forward only.
                _ => {}
            }
        }
        serde_json::Value::Array(arr) => record_batch(ctx, ts_ms, arr.len()),
        // Scalars: forward only.
        _ => {}
    }
}

/// Classify a server→client line and append an event. A response (`id`, no
/// `method`) resolves `correlates` from `pending` and records `latency_ms`;
/// a notification records an `mcp_notification`. Batches and unparseable lines
/// are handled as in [`record_upstream_line`].
fn record_downstream_line(line: &[u8], ctx: &ProxyCtx) {
    let Some(parsed) = parse_json_line(line) else {
        return;
    };
    let ts_ms = now_unix_ms() - ctx.base_started_at;
    match &parsed {
        serde_json::Value::Object(obj) => {
            // Clone the small classifier fields out of the borrow scope so
            // `parsed` can be moved into the event body below without E0505.
            let id = obj.get("id").cloned();
            let method = obj
                .get("method")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            match (id.as_ref(), method.as_deref()) {
                (Some(id_val), None) => record_response(ctx, ts_ms, id_val, &parsed),
                (Some(_), Some(method)) => record_server_request(ctx, ts_ms, method, parsed),
                (None, Some(method)) => record_notification(ctx, ts_ms, method, parsed),
                // Object with neither id nor method: malformed MCP; forward only.
                _ => {}
            }
        }
        serde_json::Value::Array(arr) => record_batch(ctx, ts_ms, arr.len()),
        _ => {}
    }
}

/// Record a server→client response: resolve `correlates` + `latency_ms` from
/// the pending request, and inject `latency_ms` into the body.
fn record_response(
    ctx: &ProxyCtx,
    ts_ms: i64,
    id_val: &serde_json::Value,
    parsed: &serde_json::Value,
) {
    let key = id_key(id_val);
    let (correlates, request_ts) = match &key {
        Some(k) => ctx
            .take_pending(k)
            .map_or((None, None), |(eid, ts)| (Some(eid), Some(ts))),
        None => (None, None),
    };
    let latency_ms = request_ts.map(|t| now_unix_ms() - t);
    let id_disp = id_key(id_val).unwrap_or_else(|| "?".into());
    let summary = truncate_summary(&format!("mcp response: id {id_disp}"));
    let mut body = serde_json::to_value(parsed).unwrap_or_else(|_| parsed.clone());
    if let Some(lat) = latency_ms {
        if let Some(b) = body.as_object_mut() {
            b.insert("latency_ms".into(), serde_json::json!(lat));
        }
    }
    let mut event = ctx.event(EventKind::McpResponse, ts_ms, summary, body);
    event.correlates = correlates;
    ctx.append(event);
}

/// Record a server-side request (a server → client message that itself has
/// `id` + `method`, e.g. sampling/roots elicitation). No correlation target
/// exists on this side, so `correlates` stays `None`.
fn record_server_request(ctx: &ProxyCtx, ts_ms: i64, method: &str, parsed: serde_json::Value) {
    let summary = truncate_summary(&format!("mcp request: {method}"));
    let body = serde_json::to_value(&parsed).unwrap_or(parsed);
    ctx.append(ctx.event(EventKind::McpRequest, ts_ms, summary, body));
}

/// Record an `mcp_notification` for a `method`-only message.
fn record_notification(ctx: &ProxyCtx, ts_ms: i64, method: &str, parsed: serde_json::Value) {
    let summary = truncate_summary(&format!("mcp notification: {method}"));
    let body = serde_json::to_value(&parsed).unwrap_or(parsed);
    ctx.append(ctx.event(EventKind::McpNotification, ts_ms, summary, body));
}

/// Record one `mcp_notification` for a JSON-array batch (MCP 2025-03-26). v0.1
/// simplification: the raw line is forwarded verbatim (wire correctness holds),
/// but no per-element correlation is recorded.
fn record_batch(ctx: &ProxyCtx, ts_ms: i64, len: usize) {
    let summary = truncate_summary(&format!("mcp batch: {len} messages"));
    let body = serde_json::json!({ "batch": true, "count": len });
    ctx.append(ctx.event(EventKind::McpNotification, ts_ms, summary, body));
}

/// Parse a single NDJSON line (trailing `\n` trimmed) as a JSON value. Returns
/// `None` on any parse error (caller forwards the raw line verbatim regardless).
fn parse_json_line(line: &[u8]) -> Option<serde_json::Value> {
    let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
    let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
    serde_json::from_slice(trimmed).ok()
}

/// The JSON-RPC `id` as a correlation key: numbers and strings use their
/// string repr; `null` (not usable for correlation per the spec) → `None`.
fn id_key(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// If `body` serializes to ≥ [`SPILL_BYTES`], store it as a blob and return an
/// overflow envelope; otherwise return the body inline. Mirrors the adapter's
/// `maybe_spill` (FR-1.5 / FR-2 spillover contract).
fn maybe_spill(
    body: serde_json::Value,
    blobs: &BlobStore,
) -> (Option<serde_json::Value>, Option<String>, Option<u64>) {
    let serialized = serde_json::to_vec(&body).unwrap_or_default();
    if serialized.len() >= SPILL_BYTES {
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

/// Truncate a summary to the SRS §4.1 limit of 120 chars (matches the runner).
fn truncate_summary(s: &str) -> String {
    const LIMIT: usize = 120;
    if s.chars().count() <= LIMIT {
        return s.to_string();
    }
    let truncated: String = s.chars().take(LIMIT - 1).collect();
    format!("{truncated}…")
}

/// Current unix-ms UTC timestamp.
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Best-effort hostname (FR-1.2). Shells out to `hostname` (no extra dep).
fn hostname() -> Option<String> {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Wait for the server child to exit and return its exit code (best-effort).
fn wait_for_child(child: &mut Child) -> Option<i32> {
    match child.wait() {
        Ok(status) => status.code(),
        Err(_) => None,
    }
}

/// Fuzz-only entry points into the MCP JSON-RPC line classifier (`cargo fuzz`
/// target `mcp_frame`). Gated behind the `fuzzing` feature so it never widens
/// the crate's normal public API.
#[cfg(feature = "fuzzing")]
pub mod fuzzing {
    use super::{record_downstream_line, record_upstream_line, ProxyCtx};
    use hh_core::blob::BlobStore;
    use hh_core::event::{AdapterStatus, AgentKind, NewSession};
    use hh_core::store::Store;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    /// A `ProxyCtx` backed by a real (process-unique temp dir) store, built
    /// once and reused across fuzz iterations — the interesting logic under
    /// fuzz is the NDJSON classification in `record_*_line`, not store setup.
    fn ctx() -> &'static ProxyCtx {
        static CTX: OnceLock<ProxyCtx> = OnceLock::new();
        CTX.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("hh-fuzz-mcp-{}", std::process::id()));
            let store =
                Store::open(&dir.join("hh.db"), &dir.join("blobs")).expect("open fuzz store");
            let new_session = NewSession {
                id: hh_core::event::now_v7(),
                started_at: 0,
                agent_kind: AgentKind::McpOnly,
                adapter_status: AdapterStatus::None,
                command: vec!["fuzz".to_string()],
                cwd: std::env::temp_dir(),
                hostname: None,
                hh_version: "fuzz".to_string(),
                model: None,
                git_branch: None,
                git_sha: None,
                git_dirty: None,
            };
            let created = store
                .create_session(&new_session)
                .expect("create fuzz session");
            let writer = Arc::new(Mutex::new(store.event_writer().expect("fuzz writer")));
            let blobs = Arc::new(BlobStore::new(store.blobs().root().to_path_buf()));
            ProxyCtx {
                writer,
                blobs,
                pending: Arc::new(Mutex::new(HashMap::new())),
                session_id: created.id,
                base_started_at: 0,
            }
        })
    }

    /// Fuzz a client→server line exactly as the upstream thread processes it.
    pub fn fuzz_upstream_line(line: &[u8]) {
        record_upstream_line(line, ctx());
    }

    /// Fuzz a server→client line exactly as the downstream thread processes it.
    pub fn fuzz_downstream_line(line: &[u8]) {
        record_downstream_line(line, ctx());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_key_handles_string_number_null() {
        assert_eq!(id_key(&serde_json::json!("abc")), Some("abc".into()));
        assert_eq!(id_key(&serde_json::json!(42)), Some("42".into()));
        assert_eq!(id_key(&serde_json::json!(null)), None);
        assert_eq!(id_key(&serde_json::json!(true)), None);
    }

    #[test]
    fn parse_json_line_handles_trailing_newline_and_cr() {
        assert!(parse_json_line(b"{\"a\":1}\n").is_some());
        assert!(parse_json_line(b"{\"a\":1}\r\n").is_some());
        assert!(parse_json_line(b"not json\n").is_none());
        assert!(parse_json_line(b"").is_none());
    }

    #[test]
    fn maybe_spill_inline_below_threshold_and_spills_above() {
        // Real in-temp-dir blob store (CLAUDE.md: no mocks).
        let dir = std::env::temp_dir().join(format!("hh-mcp-spill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let blobs = BlobStore::new(dir.clone());

        // A small body stays inline.
        let small = serde_json::json!({"method": "tools/call"});
        let (body, hash, size) = maybe_spill(small.clone(), &blobs);
        assert_eq!(body.as_ref().unwrap(), &small);
        assert!(hash.is_none() && size.is_none());

        // A body that serializes ≥ 256 KiB spills to a blob with an envelope.
        let big = serde_json::json!({"data": "x".repeat(SPILL_BYTES)});
        let (body, hash, size) = maybe_spill(big, &blobs);
        let envelope = body.expect("large body must spill");
        assert_eq!(envelope["overflow"], serde_json::json!(true));
        assert_eq!(envelope["encoding"], serde_json::json!("blob"));
        let hash = hash.expect("blob hash set");
        assert!(size.unwrap() >= SPILL_BYTES as u64);
        // The spilled bytes round-trip from the blob store.
        let stored = blobs.get(&hash).unwrap();
        assert!(stored.len() >= SPILL_BYTES);

        std::fs::remove_dir_all(&dir).ok();
    }
}
