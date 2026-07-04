//! PTY runner: spawn the agent in a PTY and transparently proxy stdin/stdout
//! (FR-1.1, FR-1.3). Drives terminal-output capture (chunked at 8 KiB or
//! 50 ms), window-resize forwarding (SIGWINCH), and graceful shutdown on
//! SIGTERM/SIGINT to `hh`.
//!
//! Concurrency model (ADR-0001): OS threads + `std::sync::mpsc`/`Arc<Mutex>`.
//! The shared [`EventWriter`] is wrapped in `Arc<Mutex<_>>` so the PTY reader,
//! FS watcher, and optional input recorder all feed the single-writer task
//! without sharing a `Connection` (CLAUDE.md). The lock is held only for the
//! channel send + reply, serializing appends — which is exactly the
//! single-writer invariant.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hh_core::blob::BlobStore;
use hh_core::event::{Event, EventKind, NewSession};
use hh_core::store::{EventWriter, Store};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use signal_hook::consts::{SIGINT, SIGTERM, SIGWINCH};
use signal_hook::flag::register as register_flag;

use crate::watcher::{spawn_watcher, WatchOptions};

/// Chunk flush thresholds (FR-1.3): 8 KiB or 50 ms, whichever first.
const CHUNK_BYTES: usize = 8 * 1024;
const CHUNK_INTERVAL: Duration = Duration::from_millis(50);

/// Per-recording options passed by the binary.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The command argv to spawn (program + args).
    pub command: Vec<String>,
    /// Working directory for the child and the FS watcher.
    pub cwd: PathBuf,
    /// Max file size for FS capture (bytes).
    pub max_file_size: u64,
    /// Record user keystrokes (FR-1.3/NFR-4; default off).
    pub record_input: bool,
    /// Store binary file contents (FR-1.4; default off).
    pub record_binary: bool,
    /// Extra ignore patterns for the watcher.
    pub extra_ignore: Vec<String>,
    /// Absolute halfhand-owned paths under cwd to exclude from the watcher
    /// (the db file, the blobs dir) so the recorder doesn't record itself.
    pub internal_exclude: Vec<PathBuf>,
    /// `hh` version string (FR-1.2).
    pub hh_version: String,
}

/// The outcome of a finished recording (FR-1.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// Full session UUID string.
    pub session_id: String,
    /// 6-hex-char short id.
    pub short_id: String,
    /// Child exit code, if it could be determined.
    pub exit_code: Option<i32>,
    /// Wall-clock duration in ms.
    pub duration_ms: i64,
    /// Number of events written (including raw terminal chunks).
    pub event_count: i64,
    /// Number of step-eligible events (FR-3.4; everything except
    /// `terminal_output`). The "N steps" in the epilogue (FR-1.6).
    pub steps: i64,
    /// Number of distinct files changed.
    pub files_changed: i64,
    /// Final session status (`ok` | `error` | `interrupted`).
    pub status: &'static str,
}

/// A guard that disables raw mode on drop. Created only when stdin is a TTY.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Best-effort; we're tearing down anyway.
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Run one recording session (FR-1.1, FR-1.3). Creates the session row,
/// spawns the PTY + watcher, drives capture, and finalizes. Returns the
/// outcome for the epilogue (FR-1.6).
///
/// # Errors
///
/// Returns [`crate::RecordError`] for setup failures (PTY spawn, watcher
/// init, store errors). A child that exits nonzero is *not* an error here —
/// it's reflected in `RunOutcome.status`/`exit_code`.
#[allow(clippy::too_many_lines)] // the run loop is inherently linear
pub fn run(store: &Store, opts: &RunOptions) -> crate::Result<RunOutcome> {
    // --- Session row (FR-1.2) -------------------------------------------
    let start = Instant::now();
    let started_at = now_unix_ms();
    let session_uuid = hh_core::event::now_v7();
    let agent_kind = crate::agent::detect_agent(&opts.command);
    let git = crate::git::GitMeta::capture(&opts.cwd);
    let new_session = NewSession {
        id: session_uuid,
        started_at,
        agent_kind,
        command: opts.command.clone(),
        cwd: opts.cwd.clone(),
        hostname: hostname(),
        hh_version: opts.hh_version.clone(),
        model: None,
        git_branch: git.branch,
        git_sha: git.sha,
        git_dirty: git.dirty,
    };
    let created = store.create_session(&new_session)?;
    let session_id = created.id;
    let short_id = created.short_id;
    let writer = Arc::new(Mutex::new(store.event_writer()?));

    // --- FS watcher (FR-1.4) --------------------------------------------
    let watch_opts = WatchOptions {
        cwd: opts.cwd.clone(),
        max_file_size: opts.max_file_size,
        record_binary: opts.record_binary,
        extra_ignore: opts.extra_ignore.clone(),
        internal_exclude: opts.internal_exclude.clone(),
    };
    let blobs = Arc::new(BlobStore::new(store.blobs().root().to_path_buf()));
    let watcher = spawn_watcher(
        watch_opts,
        Arc::clone(&writer),
        Arc::clone(&blobs),
        session_id.clone(),
        start,
    )?;

    // --- PTY + child (FR-1.1) --------------------------------------------
    let pty_system = native_pty_system();
    let initial_size = current_pty_size();
    let pty_pair = pty_system
        .openpty(initial_size)
        .map_err(|e| crate::RecordError::Pty(e.to_string()))?;

    let mut cmd = CommandBuilder::new(&opts.command[0]);
    for arg in &opts.command[1..] {
        cmd.arg(arg);
    }
    cmd.cwd(&opts.cwd);
    // FR-2.2: advertise the session id to the child process tree so a nested
    // `hh mcp-proxy` can attach.
    cmd.env("HH_SESSION_ID", &session_id);

    let child = pty_pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| crate::RecordError::Spawn {
            command: opts.command.join(" "),
            reason: e.to_string(),
        })?;
    // Drop the slave so EOF propagates to the master reader when the child exits.
    drop(pty_pair.slave);

    let reader = pty_pair
        .master
        .try_clone_reader()
        .map_err(|e| crate::RecordError::Pty(e.to_string()))?;
    let stdin_writer = pty_pair
        .master
        .take_writer()
        .map_err(|e| crate::RecordError::Pty(e.to_string()))?;
    let master = pty_pair.master;

    // --- Raw mode on stdin TTY (transparent proxy) ------------------------
    let raw_guard = if std::io::stdin().is_terminal() {
        match crossterm::terminal::enable_raw_mode() {
            Ok(()) => Some(RawModeGuard),
            Err(e) => {
                eprintln!("hh: warning: could not enter raw mode: {e}");
                None
            }
        }
    } else {
        None
    };

    // --- Signals ---------------------------------------------------------
    let stop = Arc::new(AtomicBool::new(false));
    let resize_flag = Arc::new(AtomicBool::new(false));
    // Register signal flags. Best-effort: if a signal can't be registered, we
    // simply don't handle it (the child still runs).
    let _ = register_flag(SIGWINCH, Arc::clone(&resize_flag));
    let _ = register_flag(SIGTERM, Arc::clone(&stop));
    // SIGINT: in raw mode the user's Ctrl-C goes through the PTY to the child,
    // so hh only receives SIGINT if something explicitly targets hh's PID.
    let _ = register_flag(SIGINT, Arc::clone(&stop));

    // --- Reader thread (terminal_output capture, FR-1.3) ----------------
    let writer_for_reader = Arc::clone(&writer);
    let blobs_for_reader = Arc::clone(&blobs);
    let session_id_for_reader = session_id.clone();
    let reader_stop = Arc::new(AtomicBool::new(false));
    let reader_stop_for_check = Arc::clone(&reader_stop);
    let reader_thread = std::thread::Builder::new()
        .name("hh-pty-reader".into())
        .spawn(move || {
            run_reader(
                reader,
                writer_for_reader,
                blobs_for_reader,
                session_id_for_reader,
                start,
                reader_stop_for_check,
            );
        })
        .map_err(|e| crate::RecordError::Pty(format!("spawn reader thread: {e}")))?;

    // --- Stdin proxy thread ---------------------------------------------
    let writer_for_stdin = Arc::clone(&writer);
    let session_id_for_stdin = session_id.clone();
    let record_input = opts.record_input;
    let stdin_thread = std::thread::Builder::new()
        .name("hh-stdin-proxy".into())
        .spawn(move || {
            run_stdin_proxy(
                stdin_writer,
                writer_for_stdin,
                session_id_for_stdin,
                start,
                record_input,
            );
        })
        .map_err(|e| crate::RecordError::Pty(format!("spawn stdin thread: {e}")))?;

    // --- Main wait loop (child exit + signals) --------------------------
    let mut child = child;
    let mut killed_by_signal = false;
    let exit_status = loop {
        if stop.load(Ordering::Acquire) {
            killed_by_signal = true;
            let _ = child.kill();
            // Fall through to wait for the kill to take effect.
        }
        if resize_flag.swap(false, Ordering::AcqRel) {
            let size = current_pty_size();
            let _ = master.resize(size);
        }
        match child
            .try_wait()
            .map_err(|e| crate::RecordError::Child(e.to_string()))?
        {
            Some(status) => break status,
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };

    // Child is gone: stop the reader and drain it.
    reader_stop.store(true, Ordering::Release);
    // Drop the master so the reader's blocking read hits EOF promptly.
    drop(master);
    let _ = reader_thread.join();

    // Detach the stdin proxy: it blocks on a TTY read we can't interrupt
    // portably. Dropping the handle lets the OS reclaim it at process exit;
    // the master is already closed so any further write fails fast.
    drop(stdin_thread);

    // Stop the FS watcher.
    watcher.stop_and_join();

    // --- Finalize (FR-1.6) ----------------------------------------------
    drop(raw_guard); // restore terminal before printing the epilogue
    let duration_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    let code = i32::try_from(exit_status.exit_code()).unwrap_or(i32::MAX);
    let (exit_code, status) = if killed_by_signal {
        (Some(code), "interrupted")
    } else if exit_status.success() {
        (Some(0), "ok")
    } else {
        (Some(code), "error")
    };

    let (event_count, steps, files_changed) =
        finalize(store, &session_id, &writer, duration_ms, exit_code, status)?;
    // Close the writer thread (the writer is shared via Arc<Mutex>, so we
    // close it in place rather than consuming it with `finish()`).
    writer
        .lock()
        .map_err(|e| crate::RecordError::Pty(format!("writer lock poisoned: {e}")))?
        .close()
        .map_err(crate::RecordError::from)?;

    Ok(RunOutcome {
        session_id,
        short_id,
        exit_code,
        duration_ms,
        event_count,
        steps,
        files_changed,
        status,
    })
}

/// The reader thread: read PTY output, copy to stdout, and emit chunked
/// `terminal_output` events (FR-1.3).
///
/// `needless_pass_by_value`: these values are moved into the spawning closure
/// and owned for the thread's lifetime; taking them by value (not `&`) keeps
/// the thread self-contained and is idiomatic for thread entry points.
#[allow(clippy::needless_pass_by_value)]
fn run_reader(
    mut reader: Box<dyn Read + Send>,
    writer: Arc<Mutex<EventWriter>>,
    blobs: Arc<BlobStore>,
    session_id: String,
    start: Instant,
    stop: Arc<AtomicBool>,
) {
    let mut out = std::io::stdout();
    let mut buf = [0u8; CHUNK_BYTES];
    let mut acc: Vec<u8> = Vec::with_capacity(CHUNK_BYTES);
    let mut last_flush = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                // Forward to the user's terminal first (imperceptible latency).
                let _ = out.write_all(&buf[..n]);
                let _ = out.flush();
                acc.extend_from_slice(&buf[..n]);
                if acc.len() >= CHUNK_BYTES || last_flush.elapsed() >= CHUNK_INTERVAL {
                    flush_terminal_chunk(&writer, &blobs, &session_id, start, &acc);
                    acc.clear();
                    last_flush = Instant::now();
                }
            }
            Err(e) => {
                // EINTR is transient; anything else ends the reader.
                if e.kind() != std::io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }
    if !acc.is_empty() {
        flush_terminal_chunk(&writer, &blobs, &session_id, start, &acc);
    }
}

/// The stdin proxy thread: forward the user's stdin to the PTY writer, and
/// optionally record keystrokes (FR-1.3 `--record-input`).
#[allow(clippy::needless_pass_by_value)] // owned for the thread's lifetime; see run_reader
fn run_stdin_proxy(
    mut stdin_writer: Box<dyn Write + Send>,
    writer: Arc<Mutex<EventWriter>>,
    session_id: String,
    start: Instant,
    record_input: bool,
) {
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; CHUNK_BYTES];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break, // stdin EOF
            Ok(n) => {
                if record_input {
                    flush_input_chunk(&writer, &session_id, start, &buf[..n]);
                }
                if stdin_writer.write_all(&buf[..n]).is_err() {
                    break; // PTY closed
                }
                let _ = stdin_writer.flush();
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }
}

/// Emit a `terminal_output` event for an output chunk (FR-1.3).
///
/// UTF-8 chunks are stored inline in `body_json` as `{"text":"...","encoding":"utf8"}`
/// for replay convenience; non-UTF-8 chunks go to the blob store and the
/// event references them by hash (`{"encoding":"blob","size":N}`). Raw ANSI
/// bytes are preserved either way.
fn flush_terminal_chunk(
    writer: &Arc<Mutex<EventWriter>>,
    blobs: &BlobStore,
    session_id: &str,
    start: Instant,
    bytes: &[u8],
) {
    let ts_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    let (body, blob_hash, blob_size) = match std::str::from_utf8(bytes) {
        Ok(text) => (
            serde_json::json!({ "text": text, "encoding": "utf8" }),
            None,
            None,
        ),
        Err(_) => match blobs.put(bytes) {
            Ok(outcome) => (
                serde_json::json!({ "encoding": "blob", "size": outcome.size }),
                Some(outcome.hash),
                Some(outcome.size),
            ),
            Err(_) => (
                serde_json::json!({ "encoding": "base64", "bytes": base64_encode(bytes) }),
                None,
                None,
            ),
        },
    };
    let event = Event {
        session_id: session_id.to_string(),
        ts_ms,
        kind: EventKind::TerminalOutput,
        step: None, // terminal chunks are not steps (FR-3.4)
        summary: truncate_summary(&format!("terminal output {} bytes", bytes.len())),
        body_json: Some(body),
        blob_hash,
        blob_size,
        correlates: None,
    };
    if let Ok(w) = writer.lock() {
        let _ = w.append_event(event);
    }
}

/// Emit a `terminal_output` event for a recorded input chunk (`--record-input`).
/// Marked with `direction: "input"` so replay can distinguish it from output.
fn flush_input_chunk(
    writer: &Arc<Mutex<EventWriter>>,
    session_id: &str,
    start: Instant,
    bytes: &[u8],
) {
    let ts_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    let body = match std::str::from_utf8(bytes) {
        Ok(text) => serde_json::json!({ "text": text, "encoding": "utf8", "direction": "input" }),
        Err(_) => serde_json::json!({
            "encoding": "base64",
            "bytes": base64_encode(bytes),
            "direction": "input",
        }),
    };
    let event = Event {
        session_id: session_id.to_string(),
        ts_ms,
        kind: EventKind::TerminalOutput,
        step: None,
        summary: truncate_summary(&format!("terminal input {} bytes", bytes.len())),
        body_json: Some(body),
        blob_hash: None,
        blob_size: None,
        correlates: None,
    };
    if let Ok(w) = writer.lock() {
        let _ = w.append_event(event);
    }
}

/// Finalize the session row and return `(event_count, steps, files_changed)`.
fn finalize(
    store: &Store,
    session_id: &str,
    writer: &Arc<Mutex<EventWriter>>,
    _duration_ms: i64,
    exit_code: Option<i32>,
    status: &str,
) -> crate::Result<(i64, i64, i64)> {
    let status_enum = match status {
        "ok" => hh_core::event::SessionStatus::Ok,
        "error" => hh_core::event::SessionStatus::Error,
        _ => hh_core::event::SessionStatus::Interrupted,
    };
    let ended_at = now_unix_ms();
    store.finalize_session(session_id, ended_at, exit_code, status_enum)?;
    // Flush the writer so all events are durable before we count them.
    writer
        .lock()
        .map_err(|e| crate::RecordError::Pty(format!("writer lock poisoned: {e}")))?
        .flush()
        .map_err(crate::RecordError::from)?;
    let (event_count, files_changed) = store.session_stats(session_id)?;
    let steps = store.session_step_count(session_id)?;
    Ok((event_count, steps, files_changed))
}

/// Query the current terminal size, falling back to 24×80 on non-TTY or
/// error (FR-1.1 resize forwarding).
fn current_pty_size() -> PtySize {
    match crossterm::terminal::size() {
        Ok((cols, rows)) => PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        },
        Err(_) => PtySize::default(),
    }
}

/// Current unix-ms UTC timestamp.
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Best-effort hostname (FR-1.2). Shells out to `hostname` so we don't add a
/// hostname crate (not in SRS §6); returns `None` if unavailable.
fn hostname() -> Option<String> {
    Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Truncate a summary to the SRS §4.1 limit of 120 chars.
fn truncate_summary(s: &str) -> String {
    const LIMIT: usize = 120;
    if s.chars().count() <= LIMIT {
        return s.to_string();
    }
    let truncated: String = s.chars().take(LIMIT - 1).collect();
    format!("{truncated}…")
}

/// A minimal base64 encoder (avoids adding a base64 dependency just for the
/// rare binary-terminal-output fallback path when blob storage also fails).
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let triple = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip_ascii() {
        let s = base64_encode(b"hello");
        // "hello" -> "aGVsbG8="
        assert_eq!(s, "aGVsbG8=");
    }

    #[test]
    fn base64_handles_empty_and_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn truncate_summary_respects_limit() {
        assert_eq!(truncate_summary("short"), "short");
        let long: String = "x".repeat(200);
        let t = truncate_summary(&long);
        assert!(t.chars().count() <= 120);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn now_unix_ms_is_plausible() {
        let ms = now_unix_ms();
        // After 2026-01-01 (~1_767_000_000_000) and before year 2100.
        assert!(ms > 1_767_000_000_000);
        assert!(ms < 4_000_000_000_000);
    }
}
