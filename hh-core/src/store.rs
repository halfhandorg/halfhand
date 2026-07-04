//! SQLite-backed session/event store (SRS §4.1, §3, FR-3.1).
//!
//! The [`Store`] owns one [`rusqlite::Connection`] on the thread that opened
//! it, used for session lifecycle and reads. During recording an
//! [`EventWriter`] runs the single-writer task on its own thread with its own
//! connection, fed by an `mpsc` channel — satisfying CLAUDE.md's
//! "single-writer, never share a Connection across threads" rule.

use crate::blob::BlobStore;
use crate::error::{Error, ResolveError, Result, StorageError};
use crate::event::{
    AdapterStatus, AgentKind, Event, FileChange, NewSession, SessionRow, SessionStatus,
};
use crate::migrations::LATEST_VERSION;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

/// The on-disk store: SQLite DB + blob store.
pub struct Store {
    conn: Connection,
    blobs: BlobStore,
    db_path: PathBuf,
}

/// A handle to the single-writer task. Drop is intentional and explicit: call
/// [`EventWriter::finish`] to flush and join the writer thread.
pub struct EventWriter {
    tx: Sender<WriterReq>,
    handle: Option<JoinHandle<()>>,
}

enum WriterReq {
    Append(Event, Sender<std::result::Result<i64, StorageError>>),
    AppendFileChange(
        Event,
        FileChange,
        Sender<std::result::Result<i64, StorageError>>,
    ),
    Flush(Sender<std::result::Result<(), StorageError>>),
    Finish(Sender<std::result::Result<(), StorageError>>),
}

/// The result of creating a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    /// Full UUID string.
    pub id: String,
    /// 6-hex-char short id.
    pub short_id: String,
}

impl Store {
    /// Open or create the store at `db_path`, applying migrations idempotently
    /// (DR-1). The data and blob directories are created with `0700`
    /// permissions (NFR-4).
    pub fn open(db_path: &Path, blobs_dir: &Path) -> Result<Self> {
        secure_create_parent(db_path)?;
        secure_create_dir(blobs_dir)?;
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| StorageError::Open {
            path: db_path.to_path_buf(),
            source: std::io::Error::other(e),
        })?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // foreign_keys is per-connection (the migration sets the persistent
        // journal_mode=WAL on the DB file; foreign_keys must be set on every
        // connection that wants enforcement).
        conn.execute("PRAGMA foreign_keys = ON", [])?;
        run_migrations(&conn)?;
        Ok(Self {
            conn,
            blobs: BlobStore::new(blobs_dir.to_path_buf()),
            db_path: db_path.to_path_buf(),
        })
    }

    /// Borrow the blob store (e.g. to write file snapshots before referencing
    /// them from an event).
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Create a new session row (FR-1.2).
    pub fn create_session(&self, new: &NewSession) -> Result<CreatedSession> {
        let id = new.id.to_string();
        let short_id = new.short_id();
        let command_json = serde_json::to_string(&new.command).map_err(|e| {
            StorageError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "INSERT INTO sessions
               (id, short_id, started_at, status, agent_kind, adapter_status,
                command, cwd, hostname, hh_version, model, git_branch, git_sha, git_dirty)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                id,
                short_id,
                new.started_at,
                SessionStatus::Recording.to_string(),
                new.agent_kind.to_string(),
                AdapterStatus::None.to_string(),
                command_json,
                new.cwd.to_string_lossy(),
                new.hostname,
                new.hh_version,
                new.model,
                new.git_branch,
                new.git_sha,
                new.git_dirty.map(i64::from),
            ],
        )?;
        Ok(CreatedSession { id, short_id })
    }

    /// Finalize a session with end metadata (FR-1.6) and checkpoint WAL
    /// (NFR-3 fsync-on-finalize).
    pub fn finalize_session(
        &self,
        id: &str,
        ended_at: i64,
        exit_code: Option<i32>,
        status: SessionStatus,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?1, exit_code = ?2, status = ?3 WHERE id = ?4",
            params![ended_at, exit_code, status.to_string(), id],
        )?;
        // Best-effort WAL checkpoint to make the finalize durable on disk.
        // `wal_checkpoint` returns a row, so use `execute_batch` (which discards
        // result rows) rather than `execute` (which errors with
        // `ExecuteReturnedResults`).
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        Ok(())
    }

    /// List sessions newest-first (FR-5.1).
    pub fn list_sessions(&self, limit: u32) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.short_id, s.started_at, s.ended_at, s.exit_code, s.status,
                    s.agent_kind, s.adapter_status, s.command, s.cwd,
                    (SELECT COUNT(*) FROM events e WHERE e.session_id = s.id AND e.step IS NOT NULL) AS step_count,
                    (SELECT COUNT(*) FROM events e JOIN file_changes fc ON fc.event_id = e.id
                       WHERE e.session_id = s.id) AS files_changed
             FROM sessions s
             ORDER BY s.started_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![i64::from(limit)], map_session_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Resolve a session id per FR-3.1: `last` → most recently started; a
    /// short-id prefix → the unique session whose short id starts with it
    /// (ambiguity is an error listing candidates); a full id → itself.
    pub fn resolve_session(&self, id_or_last: &str) -> Result<String> {
        if id_or_last == "last" {
            return self
                .conn
                .query_row(
                    "SELECT id FROM sessions ORDER BY started_at DESC LIMIT 1",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .ok_or_else(|| StorageError::from(ResolveError::Empty).into());
        }
        // Exact full-id match short-circuits.
        let exact: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE id = ?1",
                params![id_or_last],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        if let Some(id) = exact {
            return Ok(id);
        }
        // Prefix match on short_id.
        let pattern = format!("{id_or_last}%");
        let mut stmt = self.conn.prepare(
            "SELECT id, short_id, started_at FROM sessions
             WHERE short_id LIKE ?1
             ORDER BY started_at DESC",
        )?;
        let candidates: Vec<(String, String, i64)> = stmt
            .query_map(params![pattern], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        match candidates.len() {
            0 => Err(StorageError::NotFound(id_or_last.to_string()).into()),
            1 => Ok(candidates[0].0.clone()),
            n => {
                use std::fmt::Write;
                let mut lines = String::new();
                for (_, sid, started) in &candidates {
                    // `lines` is a String; writeln! never fails on it.
                    let _ = writeln!(lines, "  {sid}  {}", format_ts_ms(*started));
                }
                Err(StorageError::from(ResolveError::Ambiguous {
                    prefix: id_or_last.to_string(),
                    count: n,
                    candidates: lines,
                })
                .into())
            }
        }
    }

    /// Delete a session and garbage-collect blobs no longer referenced by any
    /// event (FR-6.1). Returns the number of blob files removed.
    pub fn delete_session(&self, id: &str) -> Result<usize> {
        // Collect blob hashes referenced by this session before cascading.
        let hashes: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT e.blob_hash FROM events e
                 WHERE e.session_id = ?1 AND e.blob_hash IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            v
        };
        let tx = self.conn.unchecked_transaction()?;
        // Decrement refcounts for each referenced blob.
        let mut removed = 0usize;
        for hash in &hashes {
            let refcount: i64 = tx.query_row(
                "UPDATE blobs SET refcount = refcount - 1
                 WHERE hash = ?1 RETURNING refcount",
                params![hash],
                |r| r.get::<_, i64>(0),
            )?;
            if refcount <= 0 {
                tx.execute("DELETE FROM blobs WHERE hash = ?1", params![hash])?;
                if self.blobs.remove_if_unreferenced(hash, refcount)? {
                    removed += 1;
                }
            }
        }
        // Cascade deletes events + file_changes (FK ON DELETE CASCADE).
        let deleted = tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        if deleted == 0 {
            tx.rollback()?;
            return Err(StorageError::NotFound(id.to_string()).into());
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Return `(event_count, files_changed)` for a session (FR-1.6 epilogue).
    /// `event_count` is the total event rows; `files_changed` is the count of
    /// distinct paths in `file_changes`. Reads from the store's own
    /// connection (concurrent with the writer under WAL).
    pub fn session_stats(&self, id: &str) -> Result<(i64, i64)> {
        let event_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1",
            params![id],
            |r| r.get::<_, i64>(0),
        )?;
        let files_changed: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT path) FROM file_changes
             WHERE event_id IN (SELECT id FROM events WHERE session_id = ?1)",
            params![id],
            |r| r.get::<_, i64>(0),
        )?;
        Ok((event_count, files_changed))
    }

    /// Count the step-eligible events for a session (FR-3.4: steps are every
    /// semantic event except raw `terminal_output`). Used for the FR-1.6
    /// epilogue ("N steps"). Step ordinals themselves are derived at read time
    /// (see ADR-0001 / decisions summary), so this is a count, not an
    /// assignment.
    pub fn session_step_count(&self, id: &str) -> Result<i64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND kind != 'terminal_output'",
            params![id],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(count)
    }

    /// Mark any sessions still in `recording` status as `interrupted`
    /// (FR-1.7 crash-safety). Called on `Store::open` so a crashed `hh run`
    /// is readable on the next invocation.
    ///
    /// # SRS deviation (flagged)
    ///
    /// FR-1.7 says "detected via missing end timestamp + no live PID". The
    /// SRS schema (§4.1 `sessions`) has no PID column, so we cannot check
    /// liveness; we mark *all* `recording` sessions as `interrupted` on open.
    /// This is correct for the common case (one `hh` at a time) but would
    /// mis-mark a genuinely-live recording if two `hh run`s share a data dir
    /// — which the SRS does not support in v0.1 anyway. See the decisions
    /// summary.
    pub fn mark_stale_interrupted(&self) -> Result<usize> {
        let updated = self.conn.execute(
            "UPDATE sessions SET status = 'interrupted'
             WHERE status = 'recording' AND ended_at IS NULL",
            [],
        )?;
        Ok(updated)
    }

    /// Spawn the single-writer task and return a handle for appending events.
    /// The writer opens its own [`Connection`] (never shared with the store's).
    pub fn event_writer(&self) -> Result<EventWriter> {
        let (tx, rx) = mpsc::channel::<WriterReq>();
        let db_path_for_thread = self.db_path.clone();
        let handle = std::thread::Builder::new()
            .name("hh-writer".into())
            .spawn(move || writer_run(&db_path_for_thread, rx))
            .map_err(|e| StorageError::Open {
                path: self.db_path.clone(),
                source: e,
            })?;
        Ok(EventWriter {
            tx,
            handle: Some(handle),
        })
    }
}

impl EventWriter {
    /// Append an event via the single-writer task. Returns the new event row id.
    pub fn append_event(&self, event: Event) -> Result<i64> {
        let (rtx, rrx) = mpsc::channel();
        self.tx
            .send(WriterReq::Append(event, rtx))
            .map_err(|_| StorageError::WriterClosed)?;
        rrx.recv()
            .map_err(|_| StorageError::WriterClosed)?
            .map_err(Error::from)
    }

    /// Append a `file_change` event and attach its `file_changes` row in one
    /// writer transaction (SRS §4.1 `file_changes`; FR-1.4). The event is
    /// inserted first, then the `file_changes` row is written with
    /// `event_id = last_insert_rowid()`. Returns the event row id.
    ///
    /// The `FileChange.event_id` field is ignored and overwritten with the
    /// new event's id; callers do not need to pre-allocate it.
    pub fn append_file_change(&self, event: Event, change: FileChange) -> Result<i64> {
        let (rtx, rrx) = mpsc::channel();
        self.tx
            .send(WriterReq::AppendFileChange(event, change, rtx))
            .map_err(|_| StorageError::WriterClosed)?;
        rrx.recv()
            .map_err(|_| StorageError::WriterClosed)?
            .map_err(Error::from)
    }

    /// Flush the writer: ensure all queued events are durable on disk.
    pub fn flush(&self) -> Result<()> {
        let (rtx, rrx) = mpsc::channel();
        self.tx
            .send(WriterReq::Flush(rtx))
            .map_err(|_| StorageError::WriterClosed)?;
        rrx.recv()
            .map_err(|_| StorageError::WriterClosed)?
            .map_err(Error::from)
    }

    /// Close the writer without consuming it: send `Finish`, join the thread,
    /// and release the handle. Subsequent `append_event`/`flush` calls fail
    /// with [`StorageError::WriterClosed`]. For use when the writer is shared
    /// (e.g. behind `Arc<Mutex<EventWriter>>`) and cannot be moved out.
    ///
    /// Idempotent with [`EventWriter::finish`] / `Drop`: taking the handle
    /// here leaves `None` for `Drop`, so a later drop does not double-join.
    pub fn close(&mut self) -> Result<()> {
        let (rtx, rrx) = mpsc::channel();
        let _ = self.tx.send(WriterReq::Finish(rtx));
        let _ = rrx.recv();
        if let Some(handle) = self.handle.take() {
            handle.join().map_err(|_| StorageError::WriterPanic)?;
        }
        Ok(())
    }

    /// Finish the writer: flush, close the channel, and join the thread.
    pub fn finish(mut self) -> Result<()> {
        let (rtx, rrx) = mpsc::channel();
        if self.tx.send(WriterReq::Finish(rtx)).is_err() {
            // Thread already gone; fall through to join to surface the cause.
        }
        let _ = rrx.recv();
        if let Some(handle) = self.handle.take() {
            handle.join().map_err(|_| StorageError::WriterPanic)?;
        }
        Ok(())
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        // Best-effort drain if the caller forgot to finish(): signal the
        // thread to exit and join it so it never lingers past the store.
        let (rtx, rrx) = mpsc::channel();
        let _ = self.tx.send(WriterReq::Finish(rtx));
        let _ = rrx.recv();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn writer_run(db_path: &Path, rx: Receiver<WriterReq>) {
    // Opening failed: just return. `rx` drops, which closes the channel; any
    // in-flight caller's `send`/`recv` then fails with [`StorageError::WriterClosed`].
    let Ok(mut conn) = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return;
    };
    let _ = conn.busy_timeout(Duration::from_secs(5));
    let _ = conn.execute("PRAGMA foreign_keys = ON", []);
    for req in rx {
        match req {
            WriterReq::Append(event, reply) => {
                let res = insert_event(&conn, &event);
                let _ = reply.send(res);
            }
            WriterReq::AppendFileChange(event, change, reply) => {
                let res = insert_event_with_file_change(&mut conn, &event, &change);
                let _ = reply.send(res);
            }
            WriterReq::Flush(reply) => {
                let res = conn
                    .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                    .map_err(StorageError::from);
                let _ = reply.send(res);
            }
            WriterReq::Finish(reply) => {
                let _ = reply.send(Ok(()));
                break;
            }
        }
    }
}

fn insert_event(conn: &Connection, event: &Event) -> std::result::Result<i64, StorageError> {
    let body = event
        .body_json
        .as_ref()
        .map(|v| {
            serde_json::to_string(v).map_err(|e| {
                StorageError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })
        })
        .transpose()?;
    // Bump blob refcount upsert, if a blob is referenced. Saturate to i64::MAX
    // if a (hypothetical) oversized blob exceeds the column range.
    if let Some(hash) = &event.blob_hash {
        let size = i64::try_from(event.blob_size.unwrap_or(0)).unwrap_or(i64::MAX);
        conn.execute(
            "INSERT INTO blobs (hash, size, refcount) VALUES (?1, ?2, 1)
             ON CONFLICT(hash) DO UPDATE SET refcount = refcount + 1",
            params![hash, size],
        )?;
    }
    conn.execute(
        "INSERT INTO events
           (session_id, ts_ms, kind, step, summary, body_json, blob_hash, correlates)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            event.session_id,
            event.ts_ms,
            event.kind.to_string(),
            event.step,
            event.summary,
            body,
            event.blob_hash,
            event.correlates,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Insert an event and its attached `file_changes` row in one transaction
/// (SRS §4.1; FR-1.4). Used by [`EventWriter::append_file_change`].
fn insert_event_with_file_change(
    conn: &mut Connection,
    event: &Event,
    change: &FileChange,
) -> std::result::Result<i64, StorageError> {
    let tx = conn.transaction().map_err(StorageError::from)?;
    let body = event
        .body_json
        .as_ref()
        .map(|v| {
            serde_json::to_string(v).map_err(|e| {
                StorageError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })
        })
        .transpose()?;
    if let Some(hash) = &event.blob_hash {
        let size = i64::try_from(event.blob_size.unwrap_or(0)).unwrap_or(i64::MAX);
        tx.execute(
            "INSERT INTO blobs (hash, size, refcount) VALUES (?1, ?2, 1)
             ON CONFLICT(hash) DO UPDATE SET refcount = refcount + 1",
            params![hash, size],
        )?;
    }
    tx.execute(
        "INSERT INTO events
           (session_id, ts_ms, kind, step, summary, body_json, blob_hash, correlates)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            event.session_id,
            event.ts_ms,
            event.kind.to_string(),
            event.step,
            event.summary,
            body,
            event.blob_hash,
            event.correlates,
        ],
    )?;
    let event_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO file_changes (event_id, path, change_kind, before_hash, after_hash, is_binary)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            event_id,
            change.path,
            change.change_kind.to_string(),
            change.before_hash,
            change.after_hash,
            i64::from(change.is_binary),
        ],
    )?;
    tx.commit().map_err(StorageError::from)?;
    Ok(event_id)
}

fn map_session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let status_str: String = r.get(5)?;
    let agent_str: String = r.get(6)?;
    let adapter_str: String = r.get(7)?;
    let command_json: String = r.get(8)?;
    let command: Vec<String> = serde_json::from_str(&command_json).unwrap_or_default();
    let status = status_str.parse().unwrap_or(SessionStatus::Recording);
    let agent = agent_str.parse().unwrap_or(AgentKind::Generic);
    let adapter = match adapter_str.as_str() {
        "active" => AdapterStatus::Active,
        "degraded" => AdapterStatus::Degraded,
        _ => AdapterStatus::None,
    };
    Ok(SessionRow {
        id: r.get(0)?,
        short_id: r.get(1)?,
        started_at: r.get(2)?,
        ended_at: r.get(3)?,
        exit_code: r.get(4)?,
        status,
        agent_kind: agent,
        adapter_status: adapter,
        command,
        cwd: PathBuf::from(r.get::<_, String>(9)?),
        step_count: r.get(10)?,
        files_changed: r.get(11)?,
    })
}

/// Apply embedded migrations idempotently (DR-1).
///
/// The migration DDL itself creates `schema_migrations` (it is part of the
/// public schema per DR-2), so we cannot assume the table exists before the
/// first migration runs. We probe `sqlite_master` instead: if the table is
/// absent the database is fresh and we run migration 0001 in full.
fn run_migrations(conn: &Connection) -> std::result::Result<(), StorageError> {
    let table_exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_migrations')",
        [],
        |r| r.get::<_, i64>(0),
    )?;
    let applied: i64 = if table_exists == 1 {
        conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |r| r.get::<_, i64>(0),
        )?
    } else {
        0
    };
    if applied >= LATEST_VERSION {
        return Ok(());
    }
    // Run migration 0001. The DDL includes PRAGMAs which cannot run inside a
    // transaction, so execute the batch outside a tx, then record the version.
    conn.execute_batch(crate::migrations::MIGRATION_0001)
        .map_err(|e| StorageError::Migration {
            version: LATEST_VERSION,
            source: e,
        })?;
    let now = unix_ms();
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![LATEST_VERSION, now],
    )?;
    Ok(())
}

/// Current unix time in milliseconds. Uses `std::SystemTime` (no `Date::now`,
/// which is unavailable in some test harnesses; this is fine in normal Rust).
fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Format a unix-ms timestamp as an ISO-8601 UTC string for error messages.
fn format_ts_ms(ms: i64) -> String {
    use time::OffsetDateTime;
    match OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000) {
        Ok(t) => t
            .format(&time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]:[second]"
            ))
            .unwrap_or_else(|_| ms.to_string()),
        Err(_) => ms.to_string(),
    }
}

/// Create the parent directory of `path` with `0700` perms (NFR-4).
fn secure_create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        secure_create_dir(parent)?;
    }
    Ok(())
}

fn secure_create_dir(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|e| StorageError::Open {
                path: path.to_path_buf(),
                source: e,
            })?;
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::create_dir_all(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, NewSession};
    use tempfile::TempDir;
    use uuid::Uuid;

    fn open_store() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("hh.db");
        let blobs = tmp.path().join("blobs");
        let store = Store::open(&db, &blobs).unwrap();
        (tmp, store)
    }

    fn new_session() -> NewSession {
        NewSession {
            id: Uuid::now_v7(),
            started_at: 1_700_000_000_000,
            agent_kind: AgentKind::Generic,
            command: vec!["claude".into()],
            cwd: PathBuf::from("/tmp"),
            hostname: Some("host".into()),
            hh_version: "0.1.0-beta.1".into(),
            model: None,
            git_branch: Some("main".into()),
            git_sha: Some("deadbeef".into()),
            git_dirty: Some(false),
        }
    }

    fn event(session_id: &str, ts: i64, step: Option<i64>) -> Event {
        Event {
            session_id: session_id.to_string(),
            ts_ms: ts,
            kind: EventKind::AgentMessage,
            step,
            summary: "hello".into(),
            body_json: Some(serde_json::json!({"text": "hi"})),
            blob_hash: None,
            blob_size: None,
            correlates: None,
        }
    }

    #[test]
    fn migration_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("hh.db");
        let blobs = tmp.path().join("blobs");
        // First open creates + migrates.
        {
            let store = Store::open(&db, &blobs).unwrap();
            // Sanity: schema_migrations recorded version 1.
            let v: i64 = store
                .conn
                .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(v, 1);
            // Tables exist.
            let count: i64 = store
                .conn
                .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0);
        }
        // Second open must not error and must not re-run the DDL.
        {
            let store = Store::open(&db, &blobs).unwrap();
            let v: i64 = store
                .conn
                .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(v, 1);
        }
        // Third open via the same path: still fine.
        {
            let _store = Store::open(&db, &blobs).unwrap();
        }
    }

    #[test]
    fn wal_mode_is_active() {
        let (_tmp, store) = open_store();
        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn create_list_finalize_session() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        let rows = store.list_sessions(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].short_id, created.short_id);
        assert_eq!(rows[0].status, SessionStatus::Recording);
        assert_eq!(rows[0].agent_kind, AgentKind::Generic);

        store
            .finalize_session(&created.id, 1_700_000_060_000, Some(0), SessionStatus::Ok)
            .unwrap();
        let rows = store.list_sessions(10).unwrap();
        assert_eq!(rows[0].status, SessionStatus::Ok);
        assert_eq!(rows[0].ended_at, Some(1_700_000_060_000));
        assert_eq!(rows[0].exit_code, Some(0));
    }

    #[test]
    fn writer_appends_events_and_counts_steps() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        {
            let writer = store.event_writer().unwrap();
            writer.append_event(event(&created.id, 0, Some(1))).unwrap();
            writer.append_event(event(&created.id, 5, Some(2))).unwrap();
            // Non-step event (terminal output) — not counted as a step.
            writer
                .append_event(Event {
                    kind: EventKind::TerminalOutput,
                    step: None,
                    ..event(&created.id, 7, None)
                })
                .unwrap();
            writer.flush().unwrap();
            writer.finish().unwrap();
        }
        let rows = store.list_sessions(10).unwrap();
        assert_eq!(rows[0].step_count, 2);
    }

    #[test]
    fn resolve_last_returns_most_recent() {
        let (_tmp, store) = open_store();
        let a = store.create_session(&new_session()).unwrap();
        // Slightly newer start time.
        let mut b = new_session();
        b.started_at = 1_700_000_001_000;
        let b = store.create_session(&b).unwrap();
        let resolved = store.resolve_session("last").unwrap();
        assert_eq!(resolved, b.id);
        // Full id resolves.
        assert_eq!(store.resolve_session(&a.id).unwrap(), a.id);
    }

    #[test]
    fn resolve_short_id_prefix_unique() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        // Full short id resolves.
        let resolved = store.resolve_session(&created.short_id).unwrap();
        assert_eq!(resolved, created.id);
        // A prefix of the short id resolves while unique.
        let prefix: String = created.short_id.chars().take(3).collect();
        let resolved = store.resolve_session(&prefix).unwrap();
        assert_eq!(resolved, created.id);
    }

    #[test]
    fn resolve_ambiguous_prefix_lists_candidates() {
        // Build two sessions whose short ids share a common prefix.
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("hh.db");
        let blobs = tmp.path().join("blobs");
        let store = Store::open(&db, &blobs).unwrap();
        // Force deterministic short ids by crafting UUIDs with known hex prefixes.
        let id_a = Uuid::now_v7(); // we'll just insert rows directly to control short_id
        store.create_session(&new_session()).unwrap();
        // Insert a second session with a short_id sharing the first 2 chars by
        // writing directly (bypassing NewSession short_id derivation).
        let shared_prefix = "ab";
        store.conn.execute(
            "INSERT INTO sessions (id, short_id, started_at, status, agent_kind, adapter_status, command, cwd, hh_version)
             VALUES (?1, ?2, ?3, 'recording', 'generic', 'none', '[]', '/tmp', '0.1.0-beta.1')",
            params![id_a.to_string(), format!("{shared_prefix}cd12"), 1_700_000_000_000_i64],
        ).unwrap();
        store.conn.execute(
            "INSERT INTO sessions (id, short_id, started_at, status, agent_kind, adapter_status, command, cwd, hh_version)
             VALUES (?1, ?2, ?3, 'recording', 'generic', 'none', '[]', '/tmp', '0.1.0-beta.1')",
            params![Uuid::now_v7().to_string(), format!("{shared_prefix}ef34"), 1_700_000_001_000_i64],
        ).unwrap();
        let err = store.resolve_session(shared_prefix).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ambiguous"), "msg was: {msg}");
        assert!(msg.contains("abcd12"), "msg was: {msg}");
        assert!(msg.contains("abef34"), "msg was: {msg}");
    }

    #[test]
    fn resolve_empty_when_no_sessions() {
        let (_tmp, store) = open_store();
        let err = store.resolve_session("last").unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Storage(StorageError::Resolve(ResolveError::Empty))
        ));
    }

    #[test]
    fn resolve_not_found_for_unknown_prefix() {
        let (_tmp, store) = open_store();
        let err = store.resolve_session("zzzzzz").unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Storage(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn delete_session_gcs_unreferenced_blobs() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        // Put a blob on disk and reference it from an event.
        let out = store.blobs().put(b"payload to gc").unwrap();
        let writer = store.event_writer().unwrap();
        writer
            .append_event(Event {
                kind: EventKind::FileChange,
                blob_hash: Some(out.hash.clone()),
                blob_size: Some(out.size),
                summary: "file changed".into(),
                ..event(&created.id, 1, Some(1))
            })
            .unwrap();
        writer.finish().unwrap();
        // Blob is on disk and refcounted to 1.
        let blob_path = store.blobs().blob_path(&out.hash);
        assert!(blob_path.exists());
        let rc: i64 = store
            .conn
            .query_row(
                "SELECT refcount FROM blobs WHERE hash = ?1",
                params![out.hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rc, 1);
        // Delete the session -> blob GC'd.
        let removed = store.delete_session(&created.id).unwrap();
        assert_eq!(removed, 1);
        assert!(!blob_path.exists());
        // blobs row gone.
        let rc: Option<i64> = store
            .conn
            .query_row(
                "SELECT refcount FROM blobs WHERE hash = ?1",
                params![out.hash],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert!(rc.is_none());
    }

    #[test]
    fn shared_blob_refcount_survives_one_session_delete() {
        let (_tmp, store) = open_store();
        let a = store.create_session(&new_session()).unwrap();
        let mut b = new_session();
        b.started_at += 1000;
        let b = store.create_session(&b).unwrap();
        let out = store.blobs().put(b"shared payload").unwrap();
        let writer = store.event_writer().unwrap();
        for sid in [&a.id, &b.id] {
            writer
                .append_event(Event {
                    kind: EventKind::FileChange,
                    blob_hash: Some(out.hash.clone()),
                    blob_size: Some(out.size),
                    summary: "file changed".into(),
                    ..event(sid, 1, Some(1))
                })
                .unwrap();
        }
        writer.finish().unwrap();
        let blob_path = store.blobs().blob_path(&out.hash);
        // Deleting one session decrements to 1 — file stays.
        store.delete_session(&a.id).unwrap();
        assert!(blob_path.exists());
        // Deleting the second drops to 0 — file removed.
        store.delete_session(&b.id).unwrap();
        assert!(!blob_path.exists());
    }
}
