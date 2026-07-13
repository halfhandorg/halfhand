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
    AdapterStatus, AgentKind, ChangeKind, Event, EventDetail, EventIndexRow, EventKind, EventRow,
    FileChange, NewSession, RawEventRow, SessionRow, SessionStatus,
};
use crate::step::assign_steps as assign_steps_pass;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use std::fs;
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

/// Reclaim report from [`Store::prune_orphan_blobs`]: how many orphan blob
/// files and stale `blobs` rows were removed, and how much on-disk space was
/// freed. Used by `hh gc` (Area 3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Number of on-disk blob files removed: files with no `blobs` row, or a
    /// zero-refcount row, backing them (a crash between [`BlobStore::put`] and
    /// the referencing event's commit, or a [`Store::delete_session`] that
    /// decremented the refcount but crashed before removing the file).
    pub orphan_files_removed: u64,
    /// Total compressed on-disk bytes reclaimed by removing those files.
    pub orphan_bytes_reclaimed: u64,
    /// Number of `blobs` rows removed: zero-refcount rows paired with an orphan
    /// file, plus refcount-positive rows whose backing file was missing
    /// (deleted out of band). Removing the row lets a future reference
    /// re-create the blob instead of pointing at a missing file.
    pub orphan_rows_removed: u64,
}

/// Whole-store inventory reported by [`Store::store_stats`]: counts and disk
/// footprint broken down by the DB file (plus its WAL/SHM sidecars) and the
/// blob directory, plus the largest sessions by event count. Used by
/// `hh stats` (Area 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreStats {
    /// Number of recorded sessions.
    pub sessions: u64,
    /// Total event rows across all sessions.
    pub events: u64,
    /// Number of distinct blobs in the `blobs` table.
    pub blobs_count: u64,
    /// Sum of `blobs.size` (uncompressed content size) — "how much did I
    /// record", distinct from the on-disk footprint in [`Self::blobs_dir_bytes`].
    pub blobs_uncompressed_bytes: u64,
    /// Size of the `hh.db` file in bytes.
    pub db_bytes: u64,
    /// Size of the `hh.db-wal` write-ahead log in bytes (0 if absent).
    pub wal_bytes: u64,
    /// Size of the `hh.db-shm` shared-memory file in bytes (0 if absent).
    pub shm_bytes: u64,
    /// Total on-disk size of the compressed blob files (sum of file sizes).
    pub blobs_dir_bytes: u64,
    /// The largest sessions by event count, descending (up to the count
    /// requested from [`Store::store_stats`]).
    pub largest_sessions: Vec<LargestSession>,
}

/// One row of [`StoreStats::largest_sessions`]: a session id and its event
/// count, for the "largest sessions" summary in `hh stats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargestSession {
    /// Full session id.
    pub id: String,
    /// 6-hex-char short id.
    pub short_id: String,
    /// Number of events in this session.
    pub event_count: u64,
}

/// Where a scan finding was located within a session
/// (docs/redaction-design.md, `hh scan`).
///
/// `#[non_exhaustive]`: new location kinds are plausible as scan coverage
/// grows; variant construction from other crates is unaffected, only
/// exhaustive `match` needs a wildcard, which nothing outside this crate
/// currently does.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FindingLocation {
    /// The session's recorded command line (`sessions.command`).
    Command,
    /// An event's one-line summary.
    Summary,
    /// An event's inline `body_json` payload.
    Body,
    /// A blob referenced by an event (an overflowed payload, a file
    /// snapshot, or a non-UTF-8 terminal chunk).
    Blob {
        /// The blob's BLAKE3 hex hash.
        hash: String,
        /// The file path, when the referencing event is a `file_change`.
        path: Option<String>,
    },
}

impl std::fmt::Display for FindingLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Command => f.write_str("command"),
            Self::Summary => f.write_str("summary"),
            Self::Body => f.write_str("body"),
            Self::Blob { hash, path } => match path {
                Some(p) => write!(f, "file {p}"),
                None => write!(f, "blob {}", &hash[..hash.len().min(8)]),
            },
        }
    }
}

/// One `hh scan` finding: what kind of secret, where, and its correlation
/// tag — never the secret itself (docs/redaction-design.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFinding {
    /// The containing event's row id; `None` for session-level locations
    /// (the command line).
    pub event_id: Option<i64>,
    /// The containing event's step ordinal, if it has one.
    pub step: Option<i64>,
    /// The containing event's kind, if event-scoped.
    pub event_kind: Option<EventKind>,
    /// Where within the session the secret sits.
    pub location: FindingLocation,
    /// The detected secret kind.
    pub secret: crate::redact::SecretKind,
    /// `BLAKE3(secret)[..8]` — correlates the same secret across findings.
    pub hash8: String,
    /// Number of occurrences of this `(secret, hash8)` at this location.
    pub count: u64,
}

/// Aggregated per-secret tally for a [`RedactOutcome`] and the audit event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretSummary {
    /// The detected secret kind.
    pub secret: crate::redact::SecretKind,
    /// The secret's correlation tag.
    pub hash8: String,
    /// Total occurrences replaced.
    pub count: u64,
}

/// The result of [`Store::redact_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactOutcome {
    /// Event rows whose summary/body were rewritten.
    pub events_rewritten: u64,
    /// Blobs whose content was rewritten (stored under a new hash).
    pub blobs_rewritten: u64,
    /// Original blob files securely deleted (refcount reached zero).
    pub blobs_shredded: u64,
    /// Per-secret tallies of everything replaced.
    pub secrets: Vec<SecretSummary>,
    /// Row id of the appended redaction audit event.
    pub audit_event_id: i64,
}

/// A search result from FTS5 full-text search.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    /// The matching event's row id.
    pub event_id: i64,
    /// The owning session's full UUID.
    pub session_id: String,
    /// The owning session's 6-hex-char short id.
    pub session_short_id: String,
    /// The event's step ordinal, if any.
    pub step: Option<i64>,
    /// The event kind.
    pub kind: EventKind,
    /// The event's one-line summary.
    pub summary: String,
    /// A highlighted snippet from the FTS5 `snippet()` function, with `<b>`
    /// and `</b>` markers around matching terms.
    pub snippet: String,
    /// Milliseconds since session start.
    pub ts_ms: i64,
}

/// Filters for [`Store::search`].
#[derive(Debug, Default)]
pub struct SearchFilters {
    /// Restrict to sessions with this agent kind.
    pub agent_kind: Option<AgentKind>,
    /// Restrict to events of this kind.
    pub event_kind: Option<EventKind>,
    /// Only sessions started after this unix-ms timestamp.
    pub since: Option<i64>,
    /// Only sessions whose cwd contains this path fragment.
    pub path: Option<String>,
    /// Maximum results (default 50).
    pub limit: u32,
}

/// Extract searchable text from an event's `body_json` for FTS5 indexing.
/// Returns a flat text string suitable for full-text search.
fn extract_fts_text(body_json: Option<&serde_json::Value>) -> String {
    let Some(body) = body_json else {
        return String::new();
    };
    match body {
        serde_json::Value::Object(obj) => {
            // Text events: extract `text` field
            if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                return text.to_string();
            }
            // Tool calls: extract `name` + `input`
            if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                let mut text = format!("tool_call: {name}");
                if let Some(input) = obj.get("input") {
                    if let Some(input_str) = input.as_str() {
                        text.push_str(" input: ");
                        text.push_str(input_str);
                    } else if let Some(input_obj) = input.as_object() {
                        if let Some(cmd) = input_obj.get("command").and_then(|v| v.as_str()) {
                            text.push_str(" command: ");
                            text.push_str(cmd);
                        }
                    }
                }
                return text;
            }
            // Tool results: extract `content`
            if let Some(content) = obj.get("content").and_then(|v| v.as_str()) {
                return content.to_string();
            }
            // Error events: extract `reason`
            if let Some(reason) = obj.get("reason").and_then(|v| v.as_str()) {
                return reason.to_string();
            }
            // Overflow envelopes: no searchable text
            if obj
                .get("overflow")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                return String::new();
            }
            // Fallback: pretty-print the JSON (capped)
            serde_json::to_string(body).unwrap_or_default()
        }
        serde_json::Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

/// One event row as read by [`Store::scan_session`] (summary + inline body +
/// blob reference; no ts).
struct ScanEventRow {
    id: i64,
    kind: EventKind,
    step: Option<i64>,
    summary: String,
    body: Option<String>,
    blob_hash: Option<String>,
}

/// The raw column tuple backing [`Store::get_event_detail`], grouped into a
/// struct rather than a bare tuple (clippy::type_complexity).
struct EventRawRow {
    session_id: String,
    ts_ms: i64,
    kind_str: String,
    step: Option<i64>,
    correlates: Option<i64>,
    summary: String,
    body_str: Option<String>,
    blob_hash: Option<String>,
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
        // foreign_keys + synchronous are per-connection (the migration sets the
        // persistent journal_mode=WAL on the DB file; these two must be set on
        // every connection that wants enforcement / the tuned setting).
        conn.execute("PRAGMA foreign_keys = ON", [])?;
        // NFR-1 / NFR-3: `synchronous = NORMAL` (not the SQLite default FULL).
        // In WAL mode NORMAL keeps ACID — no corruption on crash — but does not
        // fsync the WAL on every commit; it fsyncs only at checkpoint. That is
        // exactly the durability design NFR-3 names ("fsync on session
        // finalize"): `finalize_session` runs `wal_checkpoint(TRUNCATE)`, which
        // fsyncs, so a session is durable at finalize. The default FULL fsyncs
        // per event (~3 ms fsync → ~300 events/s), which meets NFR-3's durability
        // *and then some* but caps sustained ingest far below NFR-1's ≥5,000/s.
        // NORMAL removes the per-commit fsync so ingest is not fsync-bound;
        // SQLite's default `wal_autocheckpoint` (1000 pages ≈ 4 MiB) bounds the
        // mid-session power-loss window to the last autocheckpoint. Set on the
        // writer connection too (see `writer_run`).
        conn.execute("PRAGMA synchronous = NORMAL", [])?;
        run_migrations(&conn)?;
        let store = Self {
            conn,
            blobs: BlobStore::new(blobs_dir.to_path_buf()),
            db_path: db_path.to_path_buf(),
        };
        // Self-heal step ordinals (ADR-0002): re-run the step pass for any
        // session with a semantic event whose step is still NULL — a crashed
        // finalize, or an attached MCP proxy's late events landing after the
        // parent's finalize. Usually the empty set, so cheap.
        store.heal_steps()?;
        Ok(store)
    }

    /// Borrow the blob store (e.g. to write file snapshots before referencing
    /// them from an event).
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Run `PRAGMA integrity_check` and return its result. A healthy database
    /// yields `"ok"`; a corrupt one yields the first reported fault line. Used by
    /// `hh doctor` as a non-mutating health probe (integrity_check is read-only).
    pub fn integrity_check(&self) -> Result<String> {
        let row: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(StorageError::Sqlite)?;
        Ok(row)
    }

    /// Reclaim free pages and shrink `hh.db` on disk (Area 3 / `hh gc`).
    /// `VACUUM` rebuilds the database file, so it must run with no other
    /// connection open and outside a transaction — `hh gc` is the only caller
    /// and owns the store's sole connection, with the single-writer task not
    /// running. A failure here (e.g. another process holds the file) is
    /// surfaced; it never corrupts the store.
    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// Remove orphaned blob files and stale `blobs` rows (Area 3 / `hh gc`).
    /// Two passes:
    /// 1. For every blob file on disk, if no live `blobs` row references it
    ///    (no row, or refcount ≤ 0), remove the file (and the stale row, if
    ///    any). These are files leaked by a crash between [`BlobStore::put`]
    ///    and the referencing event's commit, or a [`Store::delete_session`]
    ///    that decremented the refcount but crashed before removing the file.
    /// 2. For every `blobs` row with a positive refcount, if its file is
    ///    missing on disk (deleted out of band), remove the row so a future
    ///    reference re-creates the blob instead of pointing at a missing file.
    ///
    /// See [`PruneStats`] for what is reported. Never panics on a stray file
    /// in the blobs directory; unparseable names are skipped.
    pub fn prune_orphan_blobs(&self) -> Result<PruneStats> {
        let mut stats = PruneStats::default();
        // Pass 1: orphan files on disk.
        for hash in self.blobs.iter_hashes()? {
            let refcount: Option<i64> = self
                .conn
                .query_row(
                    "SELECT refcount FROM blobs WHERE hash = ?1",
                    params![&hash],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?;
            let is_orphan = refcount.map_or(true, |r| r <= 0);
            if is_orphan {
                let path = self.blobs.blob_path(&hash);
                let bytes = fs::metadata(&path).map_or(0, |m| m.len());
                if self.blobs.remove_if_unreferenced(&hash, 0)? {
                    stats.orphan_files_removed += 1;
                    stats.orphan_bytes_reclaimed += bytes;
                }
                if refcount.is_some() {
                    // A (zero-refcount) row backed this orphan file; drop it too.
                    self.conn
                        .execute("DELETE FROM blobs WHERE hash = ?1", params![&hash])?;
                    stats.orphan_rows_removed += 1;
                }
            }
        }
        // Pass 2: dangling rows (refcount > 0, file missing on disk).
        let dangling: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT hash FROM blobs WHERE refcount > 0")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut v = Vec::new();
            for r in rows {
                let hash = r?;
                if !self.blobs.blob_path(&hash).exists() {
                    v.push(hash);
                }
            }
            v
        };
        for hash in &dangling {
            self.conn
                .execute("DELETE FROM blobs WHERE hash = ?1", params![hash])?;
            stats.orphan_rows_removed += 1;
        }
        Ok(stats)
    }

    /// Whole-store inventory: counts, per-file disk footprint (the `hh.db`
    /// file plus its `-wal`/`-shm` sidecars and the compressed blob directory),
    /// and the `largest` sessions by event count (Area 3 / `hh stats`).
    pub fn store_stats(&self, largest: u32) -> Result<StoreStats> {
        let sessions: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        let events: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        let blobs_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))?;
        let blobs_uncompressed: i64 =
            self.conn
                .query_row("SELECT COALESCE(SUM(size), 0) FROM blobs", [], |r| r.get(0))?;
        let mut largest_sessions = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT s.id, s.short_id, COUNT(e.id) AS n
                 FROM sessions s
                 LEFT JOIN events e ON e.session_id = s.id
                 GROUP BY s.id, s.short_id
                 ORDER BY n DESC, s.started_at DESC
                 LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![i64::from(largest)], |r| {
                Ok(LargestSession {
                    id: r.get(0)?,
                    short_id: r.get(1)?,
                    event_count: r.get::<_, i64>(2).map(|n| u64::try_from(n).unwrap_or(0))?,
                })
            })?;
            for r in rows {
                largest_sessions.push(r?);
            }
        }
        // On-disk footprint: the db file + WAL/SHM sidecars + compressed blobs.
        let db_bytes = file_size(&self.db_path);
        let wal_bytes = file_size(&sidecar(&self.db_path, "-wal"));
        let shm_bytes = file_size(&sidecar(&self.db_path, "-shm"));
        let mut blobs_dir_bytes = 0u64;
        for hash in self.blobs.iter_hashes()? {
            blobs_dir_bytes += file_size(&self.blobs.blob_path(&hash));
        }
        Ok(StoreStats {
            sessions: u64::try_from(sessions).unwrap_or(0),
            events: u64::try_from(events).unwrap_or(0),
            blobs_count: u64::try_from(blobs_count).unwrap_or(0),
            blobs_uncompressed_bytes: u64::try_from(blobs_uncompressed).unwrap_or(0),
            db_bytes,
            wal_bytes,
            shm_bytes,
            blobs_dir_bytes,
            largest_sessions,
        })
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
                new.adapter_status.to_string(),
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

    /// Update a session's adapter-reported metadata at finalize (FR-1.5): model
    /// name, token-usage JSON, and final adapter status. `model` and
    /// `usage_json` use `COALESCE` so passing `None` (the adapter saw no
    /// assistant records) does not clobber a value an earlier update set;
    /// `adapter_status` is always overwritten with the outcome's status.
    pub fn set_session_adapter_meta(
        &self,
        id: &str,
        model: Option<&str>,
        usage_json: Option<&serde_json::Value>,
        status: AdapterStatus,
    ) -> Result<()> {
        let usage: Option<String> = match usage_json {
            Some(v) => Some(serde_json::to_string(v).map_err(|e| {
                StorageError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })?),
            None => None,
        };
        self.conn.execute(
            "UPDATE sessions SET
                model = COALESCE(?1, model),
                usage_json = COALESCE(?2, usage_json),
                adapter_status = ?3
             WHERE id = ?4",
            params![model, usage, status.to_string(), id],
        )?;
        Ok(())
    }

    /// List sessions newest-first (FR-5.1).
    pub fn list_sessions(&self, limit: u32) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.short_id, s.started_at, s.ended_at, s.exit_code, s.status,
                    s.agent_kind, s.adapter_status, s.command, s.cwd,
                    (SELECT COUNT(DISTINCT e.step) FROM events e
                       WHERE e.session_id = s.id AND e.step IS NOT NULL) AS step_count,
                    (SELECT COUNT(DISTINCT fc.path) FROM file_changes fc
                       JOIN events e ON e.id = fc.event_id
                       WHERE e.session_id = s.id) AS files_changed,
                    s.imported_from
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

    /// Look up a session's `started_at` (unix-ms). Used by the MCP proxy in
    /// attached mode to express event timestamps relative to the parent
    /// session's clock, so MCP events interleave correctly on the parent's
    /// timeline (FR-2). Errors if the session id does not exist.
    pub fn session_started_at(&self, id: &str) -> Result<i64> {
        let started_at = self
            .conn
            .query_row(
                "SELECT started_at FROM sessions WHERE id = ?1",
                params![id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| StorageError::NotFound(id.to_string()))?;
        Ok(started_at)
    }

    /// Look up a session's short id (6 hex). Used by the MCP proxy to print the
    /// parent session's short id in its epilogue when attaching (FR-2). Errors
    /// if the session id does not exist.
    pub fn session_short_id(&self, id: &str) -> Result<String> {
        let short_id = self
            .conn
            .query_row(
                "SELECT short_id FROM sessions WHERE id = ?1",
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StorageError::NotFound(id.to_string()))?;
        Ok(short_id)
    }

    /// Search events across all sessions using FTS5 full-text search.
    /// Returns matching events with highlighted snippets, ordered by session
    /// start time descending then event timestamp ascending.
    ///
    /// The `query` parameter uses FTS5 query syntax: words, phrases in quotes,
    /// prefix with `*`, AND/OR/NOT operators. An empty query returns no results.
    pub fn search(&self, query: &str, filters: &SearchFilters) -> Result<Vec<SearchResult>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let limit = i64::from(filters.limit.clamp(1, 500));
        let agent_kind = filters.agent_kind.as_ref().map(AgentKind::to_string);
        let event_kind = filters.event_kind.as_ref().map(EventKind::to_string);

        let sql = "SELECT e.id, e.session_id, s.short_id, e.step, e.kind, e.summary,
                   snippet(events_fts, 2, '<b>', '</b>', '...', 32) AS snippet,
                   e.ts_ms
            FROM events_fts
            JOIN events e ON e.id = events_fts.rowid
            JOIN sessions s ON s.id = e.session_id
            WHERE events_fts MATCH ?1
              AND (?2 IS NULL OR s.agent_kind = ?2)
              AND (?3 IS NULL OR e.kind = ?3)
              AND (?4 IS NULL OR s.started_at >= ?4)
              AND (?5 IS NULL OR s.cwd LIKE '%' || ?5 || '%')
            ORDER BY s.started_at DESC, e.ts_ms ASC
            LIMIT ?6";

        let mut stmt = self.conn.prepare(sql).map_err(StorageError::Sqlite)?;
        let rows = stmt
            .query_map(
                params![
                    query,
                    agent_kind,
                    event_kind,
                    filters.since,
                    filters.path,
                    limit,
                ],
                |r| {
                    let kind_str: String = r.get(4)?;
                    let kind = kind_str.parse().unwrap_or(EventKind::AgentMessage);
                    Ok(SearchResult {
                        event_id: r.get(0)?,
                        session_id: r.get(1)?,
                        session_short_id: r.get(2)?,
                        step: r.get(3)?,
                        kind,
                        summary: r.get(5)?,
                        snippet: r.get(6)?,
                        ts_ms: r.get(7)?,
                    })
                },
            )
            .map_err(StorageError::Sqlite)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(StorageError::Sqlite)?);
        }
        Ok(out)
    }

    /// Delete a session and garbage-collect blobs no longer referenced by any
    /// event (FR-6.1). Returns the number of blob files removed.
    ///
    /// Refcount semantics: the writer bumps a blob's refcount once *per
    /// referencing event* (see `insert_event`'s `ON CONFLICT DO UPDATE SET
    /// refcount = refcount + 1`), so deleting a session must decrement by the
    /// number of this session's events that reference each blob — not by 1.
    /// Otherwise a session that references the same content blob from two
    /// events (e.g. two files with identical content) would leave the blob
    /// leaked at refcount 1 after deletion. Grouping by `blob_hash` with
    /// `COUNT(*)` and decrementing by that count keeps the books balanced and
    /// lets a blob shared *across* sessions survive the deletion of one.
    pub fn delete_session(&self, id: &str) -> Result<usize> {
        // Collect (blob_hash, reference_count) for every blob this session's
        // events reference, before the cascade deletes the events.
        let refs: Vec<(String, i64)> = {
            let mut stmt = self.conn.prepare(
                "SELECT e.blob_hash, COUNT(*)
                 FROM events e
                 WHERE e.session_id = ?1 AND e.blob_hash IS NOT NULL
                 GROUP BY e.blob_hash",
            )?;
            let rows = stmt.query_map(params![id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            v
        };
        let tx = self.conn.unchecked_transaction()?;
        // Clean up the FTS5 index for this session's events before the cascade
        // deletes the events (FTS rows are not cascade-deleted).
        tx.execute(
            "DELETE FROM events_fts WHERE rowid IN (SELECT id FROM events WHERE session_id = ?1)",
            params![id],
        )?;
        // Decrement refcounts by the per-blob reference count, GC'ing any blob
        // that reaches zero (content-addressed, so a blob still referenced by
        // another session stays on disk).
        let mut removed = 0usize;
        for (hash, count) in &refs {
            let refcount: i64 = tx.query_row(
                "UPDATE blobs SET refcount = refcount - ?2
                 WHERE hash = ?1 RETURNING refcount",
                params![hash, count],
                |r| r.get::<_, i64>(0),
            )?;
            if refcount <= 0 {
                tx.execute("DELETE FROM blobs WHERE hash = ?1", params![hash])?;
                if self.blobs.remove_if_unreferenced(hash, refcount)? {
                    removed += 1;
                }
            }
        }
        // Null out intra-session event correlations before the cascade. The
        // `events.correlates` self-FK has no ON DELETE clause (default NO
        // ACTION), so without this the sessions→events cascade could trip
        // RESTRICT deleting a `tool_call` while a `tool_result` still
        // references it. Nulling first breaks the self-references; the whole
        // session is being deleted anyway.
        tx.execute(
            "UPDATE events SET correlates = NULL WHERE session_id = ?1",
            params![id],
        )?;
        // Cascade deletes events + file_changes (FK ON DELETE CASCADE).
        let deleted = tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        if deleted == 0 {
            tx.rollback()?;
            return Err(StorageError::NotFound(id.to_string()).into());
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Scan a session for secrets without mutating anything (`hh scan`,
    /// docs/redaction-design.md). Runs `detectors` over the session's
    /// recorded command line, every event's summary and inline body, and
    /// every referenced blob's content (JSON-aware for structured payloads).
    /// Findings carry the secret kind, location, and hash8 — never the
    /// secret itself. Each distinct blob is scanned once, attributed to its
    /// first referencing event (and to the file path when that event is a
    /// `file_change`).
    pub fn scan_session(
        &self,
        id: &str,
        detectors: &crate::redact::Detectors,
    ) -> Result<Vec<ScanFinding>> {
        let command_text: String = self
            .conn
            .query_row(
                "SELECT command FROM sessions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StorageError::NotFound(id.to_string()))?;
        let mut out = Vec::new();
        // Session command line.
        if let Ok(cmd) = serde_json::from_str::<serde_json::Value>(&command_text) {
            push_grouped(
                &mut out,
                detectors.detect_json(&cmd),
                None,
                None,
                None,
                &FindingLocation::Command,
            );
        }
        // Events: summary + inline body; collect blob references as we go.
        let rows: Vec<ScanEventRow> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, kind, step, summary, body_json, blob_hash FROM events
                 WHERE session_id = ?1 ORDER BY ts_ms, id",
            )?;
            let mapped = stmt.query_map(params![id], |r| {
                let kind_str: String = r.get(1)?;
                Ok(ScanEventRow {
                    id: r.get(0)?,
                    kind: kind_str.parse().unwrap_or(EventKind::AgentMessage),
                    step: r.get(2)?,
                    summary: r.get(3)?,
                    body: r.get(4)?,
                    blob_hash: r.get(5)?,
                })
            })?;
            let mut v = Vec::new();
            for m in mapped {
                v.push(m?);
            }
            v
        };
        let mut seen_blobs = std::collections::HashSet::new();
        for row in &rows {
            push_grouped(
                &mut out,
                detectors.detect(&row.summary),
                Some(row.id),
                row.step,
                Some(row.kind),
                &FindingLocation::Summary,
            );
            if let Some(body) = &row.body {
                // Structured bodies are scanned as a parsed tree (see
                // redact::Detectors::detect_json); a (defensively handled)
                // unparseable body is scanned as text so scan and redact
                // agree on what they cover.
                let findings = match serde_json::from_str::<serde_json::Value>(body) {
                    Ok(v) => detectors.detect_json(&v),
                    Err(_) => detectors.detect(body),
                };
                push_grouped(
                    &mut out,
                    findings,
                    Some(row.id),
                    row.step,
                    Some(row.kind),
                    &FindingLocation::Body,
                );
            }
            if let Some(hash) = &row.blob_hash {
                if !seen_blobs.insert(hash.clone()) {
                    continue;
                }
                // A missing/corrupt blob is `hh doctor`'s problem, not a scan
                // failure — skip it rather than aborting the report.
                let Ok(content) = self.blobs.get(hash) else {
                    continue;
                };
                let path: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT path FROM file_changes WHERE event_id = ?1",
                        params![row.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                push_grouped(
                    &mut out,
                    detectors.detect_bytes(&content),
                    Some(row.id),
                    row.step,
                    Some(row.kind),
                    &FindingLocation::Blob {
                        hash: hash.clone(),
                        path,
                    },
                );
            }
        }
        Ok(out)
    }

    /// Redact a session in place (`hh redact`, docs/redaction-design.md):
    /// rewrite every event summary/body, rewrite affected blobs under new
    /// hashes (repointing this session's references and moving refcounts),
    /// securely delete originals that reach refcount zero, append a
    /// redaction audit event, and purge plaintext remnants from the WAL and
    /// freelist (checkpoint + VACUUM). Irreversible by design.
    ///
    /// Refuses a session still in `recording` status — a live writer could
    /// re-insert plaintext mid-rewrite.
    #[allow(clippy::too_many_lines)] // one transaction, inherently sequential phases
    pub fn redact_session(
        &self,
        id: &str,
        detectors: &crate::redact::Detectors,
    ) -> Result<RedactOutcome> {
        let (status, started_at): (String, i64) = self
            .conn
            .query_row(
                "SELECT status, started_at FROM sessions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| StorageError::NotFound(id.to_string()))?;
        if status == "recording" {
            return Err(StorageError::StillRecording(id.to_string()).into());
        }

        let mut tally: std::collections::BTreeMap<(crate::redact::SecretKind, String), u64> =
            std::collections::BTreeMap::new();
        let count_findings =
            |tally: &mut std::collections::BTreeMap<(crate::redact::SecretKind, String), u64>,
             findings: &[crate::redact::Finding]| {
                for f in findings {
                    *tally.entry((f.kind.clone(), f.hash8.clone())).or_insert(0) += 1;
                }
            };
        let mut events_rewritten = 0u64;
        let mut blobs_rewritten = 0u64;
        let mut shred_list: Vec<String> = Vec::new();

        let tx = self.conn.unchecked_transaction()?;

        // Phase 1: the session's recorded command line.
        let command_text: String = tx.query_row(
            "SELECT command FROM sessions WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        if let Ok(mut cmd) = serde_json::from_str::<serde_json::Value>(&command_text) {
            let findings = detectors.redact_json(&mut cmd);
            if !findings.is_empty() {
                count_findings(&mut tally, &findings);
                tx.execute(
                    "UPDATE sessions SET command = ?1 WHERE id = ?2",
                    params![cmd.to_string(), id],
                )?;
            }
        }

        // Phase 2: event summaries and inline bodies.
        let event_rows: Vec<(i64, String, Option<String>)> = {
            let mut stmt =
                tx.prepare("SELECT id, summary, body_json FROM events WHERE session_id = ?1")?;
            let mapped = stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let mut v = Vec::new();
            for m in mapped {
                v.push(m?);
            }
            v
        };
        for (event_id, summary, body) in &event_rows {
            let mut touched = false;
            let new_summary = detectors.redact_text(summary).map(|r| {
                count_findings(&mut tally, &r.findings);
                crate::event::truncate_summary(&r.text)
            });
            let new_body: Option<String> = body.as_ref().and_then(|b| {
                match serde_json::from_str::<serde_json::Value>(b) {
                    Ok(mut v) => {
                        let findings = detectors.redact_json(&mut v);
                        if findings.is_empty() {
                            None
                        } else {
                            count_findings(&mut tally, &findings);
                            Some(v.to_string())
                        }
                    }
                    // Defensive: an unparseable body is redacted as text so
                    // redact covers everything scan reports.
                    Err(_) => detectors.redact_text(b).map(|r| {
                        count_findings(&mut tally, &r.findings);
                        r.text
                    }),
                }
            });
            if let Some(ref s) = new_summary {
                tx.execute(
                    "UPDATE events SET summary = ?1 WHERE id = ?2",
                    params![s, event_id],
                )?;
                touched = true;
            }
            if let Some(ref b) = new_body {
                tx.execute(
                    "UPDATE events SET body_json = ?1 WHERE id = ?2",
                    params![b, event_id],
                )?;
                touched = true;
            }
            if touched {
                events_rewritten += 1;
                // Update the FTS5 index with the redacted summary/body.
                // Parse the new body back to a Value for text extraction.
                let new_body_value: Option<serde_json::Value> = new_body
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok());
                let body_text = extract_fts_text(new_body_value.as_ref());
                let _ = tx.execute(
                    "UPDATE events_fts SET summary = ?1, body_text = ?2 WHERE rowid = ?3",
                    params![
                        new_summary.as_deref().unwrap_or(summary.as_str()),
                        body_text,
                        event_id
                    ],
                );
            }
        }

        // Phase 3: referenced blobs. Each affected blob is re-stored redacted
        // under its new content hash; this session's references (events,
        // file_changes, overflow envelopes) are repointed and the refcounts
        // move with them. Originals that reach refcount zero are deleted from
        // the table now and shredded from disk after commit.
        let blob_refs: Vec<(String, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT blob_hash, COUNT(*) FROM events
                 WHERE session_id = ?1 AND blob_hash IS NOT NULL GROUP BY blob_hash",
            )?;
            let mapped = stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let mut v = Vec::new();
            for m in mapped {
                v.push(m?);
            }
            v
        };
        for (old_hash, refs) in &blob_refs {
            let Ok(content) = self.blobs.get(old_hash) else {
                continue; // missing blob: gc/doctor territory, not redact's
            };
            let Some(redacted) = detectors.redact_bytes(&content) else {
                continue; // clean or binary
            };
            count_findings(&mut tally, &redacted.findings);
            let put = self.blobs.put(&redacted.bytes)?;
            // Repoint this session's event references.
            tx.execute(
                "UPDATE events SET blob_hash = ?1 WHERE session_id = ?2 AND blob_hash = ?3",
                params![put.hash, id, old_hash],
            )?;
            // Repoint file-change before/after hashes within this session.
            for column_update in [
                "UPDATE file_changes SET before_hash = ?1
                 WHERE before_hash = ?2
                   AND event_id IN (SELECT id FROM events WHERE session_id = ?3)",
                "UPDATE file_changes SET after_hash = ?1
                 WHERE after_hash = ?2
                   AND event_id IN (SELECT id FROM events WHERE session_id = ?3)",
            ] {
                tx.execute(column_update, params![put.hash, old_hash, id])?;
            }
            // Rewrite overflow envelopes that embed the old hash/size.
            let envelope_rows: Vec<(i64, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, body_json FROM events
                     WHERE session_id = ?1 AND blob_hash = ?2 AND body_json IS NOT NULL",
                )?;
                let mapped =
                    stmt.query_map(params![id, put.hash], |r| Ok((r.get(0)?, r.get(1)?)))?;
                let mut v = Vec::new();
                for m in mapped {
                    v.push(m?);
                }
                v
            };
            for (event_id, body) in envelope_rows {
                let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&body) else {
                    continue;
                };
                let is_envelope = v
                    .get("overflow")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if !is_envelope {
                    continue;
                }
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "blob_hash".into(),
                        serde_json::Value::String(put.hash.clone()),
                    );
                    obj.insert("size".into(), serde_json::json!(put.size));
                }
                tx.execute(
                    "UPDATE events SET body_json = ?1 WHERE id = ?2",
                    params![v.to_string(), event_id],
                )?;
            }
            // Move the refcounts: +refs on the new blob, -refs on the old.
            tx.execute(
                "INSERT INTO blobs (hash, size, refcount) VALUES (?1, ?2, ?3)
                 ON CONFLICT(hash) DO UPDATE SET refcount = refcount + ?3",
                params![put.hash, i64::try_from(put.size).unwrap_or(i64::MAX), refs],
            )?;
            let remaining: Option<i64> = tx
                .query_row(
                    "UPDATE blobs SET refcount = refcount - ?2
                     WHERE hash = ?1 RETURNING refcount",
                    params![old_hash, refs],
                    |r| r.get(0),
                )
                .optional()?;
            if remaining.is_some_and(|r| r <= 0) {
                tx.execute("DELETE FROM blobs WHERE hash = ?1", params![old_hash])?;
                shred_list.push(old_hash.clone());
            }
            blobs_rewritten += 1;
        }

        // Phase 4: the audit event — the session self-documents what was
        // removed (types + hash8 tallies, never secret material). Reuses
        // kind = 'lifecycle' so the documented events.kind value set is
        // unchanged (v1.0.0 addendum: additive only).
        let secrets: Vec<SecretSummary> = tally
            .iter()
            .map(|((secret, hash8), count)| SecretSummary {
                secret: secret.clone(),
                hash8: hash8.clone(),
                count: *count,
            })
            .collect();
        let total: u64 = secrets.iter().map(|s| s.count).sum();
        let audit_body = serde_json::json!({
            "redaction_audit": {
                "secrets": secrets
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "type": s.secret.to_string(),
                            "hash8": s.hash8,
                            "count": s.count,
                        })
                    })
                    .collect::<Vec<_>>(),
                "events_rewritten": events_rewritten,
                "blobs_rewritten": blobs_rewritten,
            }
        });
        let ts_ms = (unix_ms() - started_at).max(0);
        tx.execute(
            "INSERT INTO events (session_id, ts_ms, kind, step, summary, body_json, blob_hash, correlates)
             VALUES (?1, ?2, 'lifecycle', NULL, ?3, ?4, NULL, NULL)",
            params![
                id,
                ts_ms,
                crate::event::truncate_summary(&format!(
                    "redaction: {total} secret occurrence(s) removed"
                )),
                audit_body.to_string(),
            ],
        )?;
        let audit_event_id = tx.last_insert_rowid();
        tx.commit()?;

        // Post-commit: give the audit event its step ordinal, then destroy
        // the plaintext remnants — shred zero-ref originals on disk, and
        // checkpoint + VACUUM so no copy survives in the WAL or in freelist
        // pages of hh.db (invariant I1/I2 in docs/redaction-design.md).
        self.assign_steps(id)?;
        let mut blobs_shredded = 0u64;
        for hash in &shred_list {
            if self.blobs.shred(hash)? {
                blobs_shredded += 1;
            }
        }
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        self.vacuum()?;
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

        Ok(RedactOutcome {
            events_rewritten,
            blobs_rewritten,
            blobs_shredded,
            secrets,
            audit_event_id,
        })
    }

    /// Import a validated [`crate::bundle::Bundle`] (`hh import file.hh`) as
    /// a brand-new local session: a fresh UUIDv7 id, `imported_from` set to
    /// the bundle's original session id, every referenced blob written
    /// content-addressed into this store, and every event/file_change
    /// re-inserted with fresh row ids.
    ///
    /// `correlates` is resolved from the bundle's `seq`-based scheme in a
    /// second pass after every event has a new id — a single forward pass
    /// cannot always resolve it inline, since a `tool_result` can
    /// legitimately correlate to a `tool_call` that sorts *after* it by
    /// `ts_ms` (a concurrent source; see `timeline.rs`'s "out of order
    /// result" test). `step`/`ts_ms` are reused verbatim from the bundle —
    /// they were already valid ordinals for this exact event sequence, so
    /// there is no need to re-run [`Self::assign_steps`].
    ///
    /// Blob writes happen before the transaction (content-addressed and
    /// idempotent — safe even if the import is retried), then every event
    /// insert runs in one transaction on the store's own connection, the
    /// same pattern [`Self::redact_session`]/[`Self::delete_session`] use:
    /// this is a one-shot batch operation, not concurrent live recording, so
    /// the writer-thread/mpsc machinery ([`Self::event_writer`]) is
    /// unnecessary.
    pub fn import(&self, bundle: &crate::bundle::Bundle) -> Result<CreatedSession> {
        let new = bundle_new_session(bundle);
        let created = self.create_session(&new)?;

        for content in bundle.blobs.values() {
            self.blobs.put(content)?;
        }

        let tx = self.conn.unchecked_transaction()?;
        let mut seq_to_id: std::collections::HashMap<u64, i64> = std::collections::HashMap::new();
        let mut pending_correlates: Vec<(i64, u64)> = Vec::new();
        for ev in &bundle.events {
            let seq = ev
                .get("seq")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let event = bundle_event_to_event(&created.id, ev);
            let new_id = insert_event(&tx, &event)?;
            seq_to_id.insert(seq, new_id);
            if let Some(correlates_seq) =
                ev.get("correlates_seq").and_then(serde_json::Value::as_u64)
            {
                pending_correlates.push((new_id, correlates_seq));
            }
            if let Some(fc) = bundle_event_file_change(ev) {
                insert_file_change_row(&tx, new_id, &fc)?;
            }
        }
        for (new_id, correlates_seq) in pending_correlates {
            if let Some(&target_id) = seq_to_id.get(&correlates_seq) {
                tx.execute(
                    "UPDATE events SET correlates = ?1 WHERE id = ?2",
                    params![target_id, new_id],
                )?;
            }
        }
        tx.commit()?;

        let status: SessionStatus = bundle
            .session
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse().ok())
            .unwrap_or(SessionStatus::Ok);
        let exit_code = bundle
            .session
            .get("exit_code")
            .and_then(serde_json::Value::as_i64)
            .and_then(|n| i32::try_from(n).ok());
        if let Some(ended_at) = bundle
            .session
            .get("ended_at")
            .and_then(serde_json::Value::as_i64)
        {
            self.finalize_session(&created.id, ended_at, exit_code, status)?;
        }
        let original_id = bundle
            .session
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        self.conn.execute(
            "UPDATE sessions SET imported_from = ?1 WHERE id = ?2",
            params![original_id, created.id],
        )?;

        Ok(created)
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

    /// Count the distinct stored step ordinals for a session (FR-3.4). A
    /// correlated `tool_result` shares its `tool_call`'s step, so this is
    /// `COUNT(DISTINCT step)` over non-null steps — not a count of semantic
    /// events. Step ordinals are stored at finalize (ADR-0002; see
    /// [`Self::assign_steps`]) and self-healed on [`Store::open`].
    pub fn session_step_count(&self, id: &str) -> Result<i64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT step) FROM events WHERE session_id = ?1 AND step IS NOT NULL",
            params![id],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(count)
    }

    /// Read all events for a session as [`EventRow`]s, ordered by `(ts_ms, id)`
    /// (the order the FR-3.4 step pass assigns in). Used by [`Self::assign_steps`]
    /// and by replay/inspect (FR-3/FR-4, future).
    pub fn list_events(&self, session_id: &str) -> Result<Vec<EventRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, ts_ms, kind, step, correlates FROM events
             WHERE session_id = ?1 ORDER BY ts_ms, id",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            let kind_str: String = r.get(3)?;
            // Unknown kinds should not occur for data we wrote; if one does,
            // treat it as a semantic step event (AgentMessage) so it still gets
            // a step rather than being silently dropped from the pass.
            let kind = kind_str.parse().unwrap_or(EventKind::AgentMessage);
            Ok(EventRow {
                id: r.get(0)?,
                session_id: r.get(1)?,
                ts_ms: r.get(2)?,
                kind,
                step: r.get(4)?,
                correlates: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Fetch one session's full row by its (already-resolved) full id
    /// (FR-3.2 header; FR-4). Call [`Self::resolve_session`] first to turn a
    /// short id / `last` into the full id this expects.
    pub fn get_session(&self, id: &str) -> Result<SessionRow> {
        self.conn
            .query_row(
                "SELECT s.id, s.short_id, s.started_at, s.ended_at, s.exit_code, s.status,
                        s.agent_kind, s.adapter_status, s.command, s.cwd,
                        (SELECT COUNT(DISTINCT e.step) FROM events e
                           WHERE e.session_id = s.id AND e.step IS NOT NULL) AS step_count,
                        (SELECT COUNT(DISTINCT fc.path) FROM file_changes fc
                           JOIN events e ON e.id = fc.event_id
                           WHERE e.session_id = s.id) AS files_changed,
                        s.imported_from
                 FROM sessions s WHERE s.id = ?1",
                params![id],
                map_session_row,
            )
            .optional()?
            .ok_or_else(|| StorageError::NotFound(id.to_string()).into())
    }

    /// Read the full per-event index for a session (FR-3.5 eager index load):
    /// every event (including `terminal_output`) with its one-line summary,
    /// but not the (potentially large) `body_json`/blob payload — that is
    /// fetched lazily per selected row via [`Self::get_event_detail`].
    /// Ordered by `(ts_ms, id)`.
    pub fn list_event_index(&self, session_id: &str) -> Result<Vec<EventIndexRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts_ms, kind, step, correlates, summary FROM events
             WHERE session_id = ?1 ORDER BY ts_ms, id",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            let kind_str: String = r.get(2)?;
            let kind = kind_str.parse().unwrap_or(EventKind::AgentMessage);
            Ok(EventIndexRow {
                id: r.get(0)?,
                ts_ms: r.get(1)?,
                kind,
                step: r.get(3)?,
                correlates: r.get(4)?,
                summary: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Fetch one event's full detail by row id (FR-3.5 lazy body load):
    /// resolves a blob-overflowed `body_json` transparently (fetches and
    /// decompresses the blob, returning its parsed content in place of the
    /// `{"overflow": true, ...}` envelope) and attaches the `file_changes` row
    /// for `FileChange` events. Errors if the id does not exist.
    pub fn get_event_detail(&self, id: i64) -> Result<EventDetail> {
        let row: EventRawRow = self
            .conn
            .query_row(
                "SELECT session_id, ts_ms, kind, step, correlates, summary, body_json, blob_hash
                 FROM events WHERE id = ?1",
                params![id],
                |r| {
                    Ok(EventRawRow {
                        session_id: r.get(0)?,
                        ts_ms: r.get(1)?,
                        kind_str: r.get(2)?,
                        step: r.get(3)?,
                        correlates: r.get(4)?,
                        summary: r.get(5)?,
                        body_str: r.get(6)?,
                        blob_hash: r.get(7)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StorageError::NotFound(id.to_string()))?;
        let kind = row.kind_str.parse().unwrap_or(EventKind::AgentMessage);
        let inline_body: Option<serde_json::Value> = row
            .body_str
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        let body_json = self.resolve_body(inline_body, row.blob_hash.as_deref());
        let file_change = if kind == EventKind::FileChange {
            self.get_file_change(id)?
        } else {
            None
        };
        Ok(EventDetail {
            id,
            session_id: row.session_id,
            ts_ms: row.ts_ms,
            kind,
            step: row.step,
            correlates: row.correlates,
            summary: row.summary,
            body_json,
            file_change,
        })
    }

    /// Fetch the event correlated with `event_id` (FR-3.2 MCP/tool
    /// call+result pairing): if `correlates` is `Some`, that is the
    /// request/call this event points at; otherwise look for an event that
    /// points *at* `event_id` (its response/result). Returns `None` if there
    /// is no correlated event either way.
    pub fn get_correlated_event(
        &self,
        event_id: i64,
        correlates: Option<i64>,
    ) -> Result<Option<EventDetail>> {
        if let Some(cid) = correlates {
            return self.get_event_detail(cid).map(Some);
        }
        let other_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM events WHERE correlates = ?1 LIMIT 1",
                params![event_id],
                |r| r.get(0),
            )
            .optional()?;
        match other_id {
            Some(oid) => self.get_event_detail(oid).map(Some),
            None => Ok(None),
        }
    }

    /// Stream every event detail of `session_id`, in `(ts_ms, id)` order,
    /// calling `emit` once per event. This is the constant-memory path for
    /// `hh inspect --json` (FR-4): a single SQL cursor with a `LEFT JOIN` to
    /// `file_changes` walks the session row by row, and any blob-overflow
    /// body is resolved inline (the same overflow-envelope path as
    /// [`Self::get_event_detail`]), so the whole
    /// session is never collected into RAM — unlike [`Self::list_event_index`]
    /// followed by per-row [`Self::get_event_detail`], which is the right path
    /// for the interactive timeline but not for streaming a 100k-event session
    /// to NDJSON (Area 2).
    ///
    /// Each [`EventDetail`] is built, emitted, and dropped before the next
    /// row is fetched, so peak memory is O(1) regardless of session size. If
    /// `emit` returns `Err`, iteration stops immediately and the error
    /// propagates; a row-decode error likewise short-circuits.
    pub fn for_each_event_detail(
        &self,
        session_id: &str,
        mut emit: impl FnMut(EventDetail) -> Result<()>,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.session_id, e.ts_ms, e.kind, e.step, e.correlates,
                    e.summary, e.body_json, e.blob_hash,
                    fc.event_id, fc.path, fc.change_kind, fc.before_hash,
                    fc.after_hash, fc.is_binary
             FROM events e
             LEFT JOIN file_changes fc ON fc.event_id = e.id
             WHERE e.session_id = ?1
             ORDER BY e.ts_ms, e.id",
        )?;
        let mut rows = stmt.query(params![session_id])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let session_id_row: String = row.get(1)?;
            let ts_ms: i64 = row.get(2)?;
            let kind_str: String = row.get(3)?;
            let step: Option<i64> = row.get(4)?;
            let correlates: Option<i64> = row.get(5)?;
            let summary: String = row.get(6)?;
            let body_str: Option<String> = row.get(7)?;
            let blob_hash: Option<String> = row.get(8)?;
            let kind = kind_str.parse().unwrap_or(EventKind::AgentMessage);
            let inline_body: Option<serde_json::Value> = body_str
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            let body_json = self.resolve_body(inline_body, blob_hash.as_deref());
            // LEFT JOIN: fc.event_id is NULL when the event has no
            // file_changes row. Use it as the "is there a change?" sentinel.
            let fc_event_id: Option<i64> = row.get(9)?;
            let file_change = if fc_event_id.is_some() {
                let change_kind_str: String = row.get(11)?;
                let change_kind = change_kind_str.parse().unwrap_or(ChangeKind::Modified);
                let is_binary: i64 = row.get(14)?;
                Some(FileChange {
                    event_id: id,
                    path: row.get(10)?,
                    change_kind,
                    before_hash: row.get(12)?,
                    after_hash: row.get(13)?,
                    is_binary: is_binary != 0,
                })
            } else {
                None
            };
            emit(EventDetail {
                id,
                session_id: session_id_row,
                ts_ms,
                kind,
                step,
                correlates,
                summary,
                body_json,
                file_change,
            })?;
        }
        Ok(())
    }

    /// Stream every event of `session_id` exactly as stored — no blob-
    /// overflow resolution, plus the referenced blob's size (`for_each_event_detail`'s
    /// resolution silently loses the association between an event and a
    /// binary/non-JSON blob it references — see
    /// [`crate::event::RawEventRow`]). Used by `hh-core::bundle` (`hh export
    /// --bundle`), which must carry every referenced blob byte-for-byte, not
    /// just the ones that happen to resolve as JSON.
    pub fn for_each_event_raw(
        &self,
        session_id: &str,
        mut emit: impl FnMut(RawEventRow) -> Result<()>,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.ts_ms, e.kind, e.step, e.correlates,
                    e.summary, e.body_json, e.blob_hash, b.size,
                    fc.event_id, fc.path, fc.change_kind, fc.before_hash,
                    fc.after_hash, fc.is_binary
             FROM events e
             LEFT JOIN file_changes fc ON fc.event_id = e.id
             LEFT JOIN blobs b ON b.hash = e.blob_hash
             WHERE e.session_id = ?1
             ORDER BY e.ts_ms, e.id",
        )?;
        let mut rows = stmt.query(params![session_id])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let ts_ms: i64 = row.get(1)?;
            let kind_str: String = row.get(2)?;
            let step: Option<i64> = row.get(3)?;
            let correlates: Option<i64> = row.get(4)?;
            let summary: String = row.get(5)?;
            let body_str: Option<String> = row.get(6)?;
            let blob_hash: Option<String> = row.get(7)?;
            let blob_size: Option<i64> = row.get(8)?;
            let kind = kind_str.parse().unwrap_or(EventKind::AgentMessage);
            let body_json: Option<serde_json::Value> = body_str
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            let fc_event_id: Option<i64> = row.get(9)?;
            let file_change = if fc_event_id.is_some() {
                let change_kind_str: String = row.get(11)?;
                let change_kind = change_kind_str.parse().unwrap_or(ChangeKind::Modified);
                let is_binary: i64 = row.get(14)?;
                Some(FileChange {
                    event_id: id,
                    path: row.get(10)?,
                    change_kind,
                    before_hash: row.get(12)?,
                    after_hash: row.get(13)?,
                    is_binary: is_binary != 0,
                })
            } else {
                None
            };
            emit(RawEventRow {
                id,
                ts_ms,
                kind,
                step,
                correlates,
                summary,
                body_json,
                blob_hash,
                blob_size: blob_size.map(|n| u64::try_from(n).unwrap_or(0)),
                file_change,
            })?;
        }
        Ok(())
    }

    /// Resolve a possibly blob-overflowed body: if `inline` is the
    /// `{"overflow": true, ...}` envelope and `blob_hash` is set, fetch and
    /// decompress the blob and parse it as the real payload. Falls back to
    /// the inline value (the envelope itself) if the blob is missing or
    /// corrupt — a display concern, not worth failing the whole fetch over.
    fn resolve_body(
        &self,
        inline: Option<serde_json::Value>,
        blob_hash: Option<&str>,
    ) -> Option<serde_json::Value> {
        let Some(hash) = blob_hash else {
            return inline;
        };
        let is_overflow_envelope = inline
            .as_ref()
            .and_then(|v| v.get("overflow"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !is_overflow_envelope {
            return inline;
        }
        match self.blobs.get(hash) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(v) => Some(v),
                Err(_) => inline,
            },
            Err(_) => inline,
        }
    }

    /// Fetch the `file_changes` row attached to `event_id`, if any.
    fn get_file_change(&self, event_id: i64) -> Result<Option<FileChange>> {
        self.conn
            .query_row(
                "SELECT event_id, path, change_kind, before_hash, after_hash, is_binary
                 FROM file_changes WHERE event_id = ?1",
                params![event_id],
                |r| {
                    let change_kind_str: String = r.get(2)?;
                    let change_kind = change_kind_str.parse().unwrap_or(ChangeKind::Modified);
                    let is_binary: i64 = r.get(5)?;
                    Ok(FileChange {
                        event_id: r.get(0)?,
                        path: r.get(1)?,
                        change_kind,
                        before_hash: r.get(3)?,
                        after_hash: r.get(4)?,
                        is_binary: is_binary != 0,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Run the FR-3.4 step-assignment pass for a session and write the
    /// ordinals back to `events.step` in one transaction on the main
    /// connection (ADR-0002). Call after the writer is flushed + joined so
    /// there is no within-process writer contention. Idempotent.
    pub fn assign_steps(&self, session_id: &str) -> Result<()> {
        let mut rows = self.list_events(session_id)?;
        assign_steps_pass(&mut rows);
        let tx = self.conn.unchecked_transaction()?;
        for r in &rows {
            tx.execute(
                "UPDATE events SET step = ?1 WHERE id = ?2",
                params![r.step, r.id],
            )?;
        }
        tx.commit()?;
        Ok(())
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

    /// Re-run [`Self::assign_steps`] for every session that has a semantic
    /// event with a NULL step (ADR-0002 self-heal). Called from [`Self::open`].
    /// `terminal_output` events legitimately have NULL steps and are excluded
    /// from the "needs heal" probe so a session with only terminal chunks is
    /// not pointlessly rescanned.
    fn heal_steps(&self) -> Result<()> {
        let ids: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT session_id FROM events
                 WHERE step IS NULL AND kind != 'terminal_output'",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            v
        };
        for id in &ids {
            self.assign_steps(id)?;
        }
        Ok(())
    }

    /// Spawn the single-writer task and return a handle for appending events.
    /// The writer opens its own [`Connection`] (never shared with the store's).
    pub fn event_writer(&self) -> Result<EventWriter> {
        self.event_writer_with_redactor(None)
    }

    /// Like [`Self::event_writer`], but with an optional record-time
    /// redactor: every event's `summary` and `body_json` are redacted on the
    /// writer thread *before* the `INSERT` (docs/redaction-design.md
    /// enforcement point 1 — nothing hits disk unredacted). Blob content is
    /// covered by the other chokepoint, [`crate::blob::BlobStore::put`] on a
    /// [`crate::blob::BlobStore::with_redactor`] store.
    pub fn event_writer_with_redactor(
        &self,
        redactor: Option<std::sync::Arc<crate::redact::Detectors>>,
    ) -> Result<EventWriter> {
        let (tx, rx) = mpsc::channel::<WriterReq>();
        let db_path_for_thread = self.db_path.clone();
        let handle = std::thread::Builder::new()
            .name("hh-writer".into())
            .spawn(move || writer_run(&db_path_for_thread, rx, redactor.as_deref()))
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

fn writer_run(
    db_path: &Path,
    rx: Receiver<WriterReq>,
    redactor: Option<&crate::redact::Detectors>,
) {
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
    if let Err(e) = conn.busy_timeout(Duration::from_secs(5)) {
        eprintln!("hh: warning: writer thread could not set busy_timeout: {e}");
    }
    // Unlike `Store::open` (which propagates this failure), the writer thread
    // cannot fail its own construction — it has already been spawned and the
    // caller is blocked waiting for the channel, not a `Result`. A failure
    // here would silently disable the `correlates`/`file_changes` FK checks
    // for the rest of the session, so at least surface it on stderr.
    if let Err(e) = conn.execute("PRAGMA foreign_keys = ON", []) {
        eprintln!("hh: warning: writer thread could not enable foreign_keys: {e}");
    }
    // NFR-1 / NFR-3: see `Store::open`. The writer connection owns every commit,
    // so this is the connection whose `synchronous` setting actually gates
    // ingest throughput. NORMAL keeps per-event commits off the fsync path
    // (fsync happens at the finalize checkpoint); FULL here would fsync every
    // event and cap ingest near ~300/s. A failure to set it is non-fatal but
    // silently leaves ingest fsync-bound, so surface it like foreign_keys.
    if let Err(e) = conn.execute("PRAGMA synchronous = NORMAL", []) {
        eprintln!("hh: warning: writer thread could not set synchronous=NORMAL: {e}");
    }
    for req in rx {
        match req {
            WriterReq::Append(mut event, reply) => {
                if let Some(d) = redactor {
                    redact_event_in_place(&mut event, d);
                }
                let res = insert_event(&conn, &event);
                let _ = reply.send(res);
            }
            WriterReq::AppendFileChange(mut event, change, reply) => {
                if let Some(d) = redactor {
                    redact_event_in_place(&mut event, d);
                }
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

/// Record-time redaction of one event (writer thread, enforcement point 1).
/// The summary is redacted as text and re-truncated (the replacement token
/// can lengthen it past the 120-char limit); the body is redacted as a JSON
/// tree. `blob_hash` is left alone — blob content was already redacted by
/// [`BlobStore::put`] on the recorder's redacting store before this event
/// referenced it.
fn redact_event_in_place(event: &mut Event, d: &crate::redact::Detectors) {
    if let Some(r) = d.redact_text(&event.summary) {
        event.summary = crate::event::truncate_summary(&r.text);
    }
    if let Some(body) = event.body_json.as_mut() {
        let _ = d.redact_json(body);
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
    let new_id = conn.last_insert_rowid();
    // Maintain the FTS5 index: insert a row for every new event.
    let body_text = extract_fts_text(event.body_json.as_ref());
    let _ = conn.execute(
        "INSERT INTO events_fts (rowid, session_id, summary, body_text, kind, step)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            new_id,
            event.session_id,
            event.summary,
            body_text,
            event.kind.to_string(),
            event.step,
        ],
    );
    Ok(new_id)
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
    // Maintain the FTS5 index: insert a row for every new event.
    let body_text = extract_fts_text(event.body_json.as_ref());
    let _ = tx.execute(
        "INSERT INTO events_fts (rowid, session_id, summary, body_text, kind, step)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            event_id,
            event.session_id,
            event.summary,
            body_text,
            event.kind.to_string(),
            event.step,
        ],
    );
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

/// Insert one `file_changes` row for an already-inserted event (used by
/// [`Store::import`], which — unlike [`insert_event_with_file_change`] —
/// runs every event of a session through one outer transaction rather than
/// one nested transaction per event, so it needs the plain-INSERT half of
/// that function without the transaction-starting half).
fn insert_file_change_row(
    conn: &Connection,
    event_id: i64,
    change: &FileChange,
) -> std::result::Result<(), StorageError> {
    conn.execute(
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
    Ok(())
}

/// Build the [`NewSession`] for [`Store::import`] from a bundle's `session`
/// block. Fields the bundle format does not carry (`hostname`/`model`/
/// `git_*`) are `None` — [`SessionRow`], which `bundle::export` reads from,
/// does not expose them either (a pre-existing gap in the read model, not
/// something this feature narrows further).
fn bundle_new_session(bundle: &crate::bundle::Bundle) -> NewSession {
    let s = &bundle.session;
    let agent_kind = s
        .get("agent_kind")
        .and_then(serde_json::Value::as_str)
        .and_then(|x| x.parse().ok())
        .unwrap_or(AgentKind::Generic);
    let adapter_status = match s.get("adapter_status").and_then(serde_json::Value::as_str) {
        Some("active") => AdapterStatus::Active,
        Some("degraded") => AdapterStatus::Degraded,
        _ => AdapterStatus::None,
    };
    let started_at = s
        .get("started_at")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let command = s
        .get("command")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let cwd = s
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_default();
    NewSession {
        id: crate::event::now_v7(),
        started_at,
        agent_kind,
        adapter_status,
        command,
        cwd,
        hostname: None,
        hh_version: bundle.hh_version.clone(),
        model: None,
        git_branch: None,
        git_sha: None,
        git_dirty: None,
    }
}

/// Build the [`Event`] to insert for one bundle event (`correlates` is left
/// `None` — [`Store::import`] resolves it in a second pass once every event
/// has a new row id).
fn bundle_event_to_event(session_id: &str, ev: &serde_json::Value) -> Event {
    let kind = ev
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .parse()
        .unwrap_or(EventKind::AgentMessage);
    Event {
        session_id: session_id.to_string(),
        ts_ms: ev
            .get("ts_ms")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        kind,
        step: ev.get("step").and_then(serde_json::Value::as_i64),
        summary: ev
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        body_json: ev.get("body").filter(|v| !v.is_null()).cloned(),
        blob_hash: ev
            .get("blob_hash")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        blob_size: ev.get("blob_size").and_then(serde_json::Value::as_u64),
        correlates: None,
    }
}

/// Extract the `file_change` sub-object of a bundle event, if present.
fn bundle_event_file_change(ev: &serde_json::Value) -> Option<FileChange> {
    let fc = ev.get("file_change")?;
    if fc.is_null() {
        return None;
    }
    let change_kind = fc
        .get("change_kind")
        .and_then(serde_json::Value::as_str)?
        .parse()
        .ok()?;
    Some(FileChange {
        event_id: 0, // overwritten by the caller once the owning event's id is known.
        path: fc
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        change_kind,
        before_hash: fc
            .get("before_hash")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        after_hash: fc
            .get("after_hash")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        is_binary: fc
            .get("is_binary")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

/// Group raw detector findings by `(secret kind, hash8)` and append one
/// [`ScanFinding`] per group at the given location (`hh scan` reports
/// occurrence counts, not per-offset rows — offsets would say nothing useful
/// without printing the secret's surroundings).
fn push_grouped(
    out: &mut Vec<ScanFinding>,
    findings: Vec<crate::redact::Finding>,
    event_id: Option<i64>,
    step: Option<i64>,
    event_kind: Option<EventKind>,
    location: &FindingLocation,
) {
    let mut grouped: std::collections::BTreeMap<(crate::redact::SecretKind, String), u64> =
        std::collections::BTreeMap::new();
    for f in findings {
        *grouped.entry((f.kind, f.hash8)).or_insert(0) += 1;
    }
    for ((secret, hash8), count) in grouped {
        out.push(ScanFinding {
            event_id,
            step,
            event_kind,
            location: location.clone(),
            secret,
            hash8,
            count,
        });
    }
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
        imported_from: r.get(12)?,
    })
}

/// Apply embedded migrations idempotently (DR-1).
///
/// The first migration's DDL creates `schema_migrations` (it is part of the
/// public schema per DR-2), so we cannot assume the table exists before the
/// first migration runs. We probe `sqlite_master` instead: if the table is
/// absent the database is fresh (`applied = 0`) and every migration runs in
/// order; otherwise we skip up to and including the recorded `MAX(version)`.
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
    let now = unix_ms();
    for &(version, sql) in crate::migrations::MIGRATIONS {
        if version <= applied {
            continue;
        }
        // Each migration's DDL may include PRAGMAs (0001 does) which cannot run
        // inside a transaction, so execute the batch outside a tx, then record
        // its version. `0002` is a single additive `CREATE INDEX IF NOT EXISTS`
        // (see migrations/0002_events_heal_index.sql).
        conn.execute_batch(sql)
            .map_err(|e| StorageError::Migration { version, source: e })?;
        conn.execute(
            "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, now],
        )?;
    }
    Ok(())
}

/// The size of `path` in bytes, or `0` if it does not exist or its length is
/// unreadable (best-effort footprint accounting for `hh stats`).
fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| m.len())
}

/// Append `suffix` (e.g. `"-wal"`, `"-shm"`) to `db_path` to name a SQLite
/// sidecar file. SQLite names the WAL/SHM as `<db>-wal`/`<db>-shm` regardless
/// of the db's extension, so this appends rather than `with_extension` (which
/// would mishandle an extension-less db name).
fn sidecar(db_path: &Path, suffix: &str) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
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
            adapter_status: AdapterStatus::None,
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
            // Sanity: schema_migrations recorded the latest migration version.
            let v: i64 = store
                .conn
                .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(v, crate::migrations::LATEST_VERSION);
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
            assert_eq!(v, crate::migrations::LATEST_VERSION);
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

    /// Migration 0002 installs the partial index that makes `heal_steps` an
    /// O(needs-heal) probe rather than an O(all-events) scan (Area 4). A fresh
    /// store must carry it, and reopening must not duplicate it.
    #[test]
    fn heal_partial_index_exists_after_migration_0002() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("hh.db");
        let blobs = tmp.path().join("blobs");
        {
            let store = Store::open(&db, &blobs).unwrap();
            let count: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type='index' AND name='idx_events_needs_heal'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "partial index idx_events_needs_heal must exist");
        }
        // Reopen is a no-op migration (applied == LATEST) — index still exactly one.
        let store = Store::open(&db, &blobs).unwrap();
        let count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='index' AND name='idx_events_needs_heal'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "reopening must not duplicate the index");
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

    /// NFR-3 / Area 3: `finalize_session` runs `PRAGMA wal_checkpoint(TRUNCATE)`,
    /// so the main `hh.db` file holds every committed page at rest. Copying
    /// *only* `hh.db` (no `-wal`/`-shm`) into a fresh data dir and reopening
    /// must yield the finalized session intact — i.e. `hh.db` is copy-safe.
    #[test]
    fn finalize_checkpoint_makes_db_copy_safe() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("hh.db");
        let blobs = tmp.path().join("blobs");
        let created = {
            let store = Store::open(&db, &blobs).unwrap();
            let c = store.create_session(&new_session()).unwrap();
            store
                .finalize_session(&c.id, 1_700_000_060_000, Some(0), SessionStatus::Ok)
                .unwrap();
            // checkpoint(TRUNCATE) ran in finalize: the WAL is 0 bytes (it is
            // removed on close, so only assert if still present).
            let wal = sidecar(&db, "-wal");
            if wal.exists() {
                assert_eq!(
                    fs::metadata(&wal).unwrap().len(),
                    0,
                    "WAL must be truncated to 0 bytes after finalize checkpoint"
                );
            }
            c
        };
        // Copy only hh.db (no sidecars) into a fresh data dir and reopen.
        let copy_dir = TempDir::new().unwrap();
        let copy_db = copy_dir.path().join("hh.db");
        let copy_blobs = copy_dir.path().join("blobs");
        fs::create_dir_all(&copy_blobs).unwrap();
        fs::copy(&db, &copy_db).unwrap();
        let store = Store::open(&copy_db, &copy_blobs).unwrap();
        let row = store.get_session(&created.id).unwrap();
        assert_eq!(row.status, SessionStatus::Ok);
        assert_eq!(row.ended_at, Some(1_700_000_060_000));
        assert_eq!(row.exit_code, Some(0));
    }

    /// `prune_orphan_blobs` (Area 3 / `hh gc`) removes an orphan file (on
    /// disk, no `blobs` row), removes a dangling row (refcount > 0, file
    /// missing), and leaves a live referenced blob untouched.
    #[test]
    fn prune_removes_orphan_files_and_dangling_rows() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();

        // 1) Orphan file: written to disk, never referenced (no blobs row).
        let orphan = store.blobs().put(b"orphan file content").unwrap();
        let orphan_path = store.blobs().blob_path(&orphan.hash);
        assert!(orphan_path.exists());

        // 2) Live referenced blob: must survive prune.
        let live = store.blobs().put(b"live referenced content").unwrap();
        let live_path = store.blobs().blob_path(&live.hash);
        {
            let writer = store.event_writer().unwrap();
            writer
                .append_event(Event {
                    kind: EventKind::FileChange,
                    blob_hash: Some(live.hash.clone()),
                    blob_size: Some(live.size),
                    summary: "live".into(),
                    ..event(&created.id, 1, Some(1))
                })
                .unwrap();
            writer.finish().unwrap();
        }
        assert!(live_path.exists());

        // 3) Dangling row: refcount > 0, but the file is deleted out of band.
        let dangling = store.blobs().put(b"dangling row content").unwrap();
        let dangling_path = store.blobs().blob_path(&dangling.hash);
        {
            let writer = store.event_writer().unwrap();
            writer
                .append_event(Event {
                    kind: EventKind::FileChange,
                    blob_hash: Some(dangling.hash.clone()),
                    blob_size: Some(dangling.size),
                    summary: "dangling".into(),
                    ..event(&created.id, 2, Some(2))
                })
                .unwrap();
            writer.finish().unwrap();
        }
        fs::remove_file(&dangling_path).unwrap();

        let stats = store.prune_orphan_blobs().unwrap();
        assert_eq!(stats.orphan_files_removed, 1, "the orphan file was removed");
        assert!(stats.orphan_bytes_reclaimed > 0);
        assert!(!orphan_path.exists(), "orphan file gone");
        assert_eq!(
            stats.orphan_rows_removed, 1,
            "the dangling (file-missing) row was removed"
        );
        assert!(live_path.exists(), "the live referenced blob survives");
        let live_rc: Option<i64> = store
            .conn
            .query_row(
                "SELECT refcount FROM blobs WHERE hash = ?1",
                params![live.hash],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(live_rc, Some(1));
        let dangling_rc: Option<i64> = store
            .conn
            .query_row(
                "SELECT refcount FROM blobs WHERE hash = ?1",
                params![dangling.hash],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert!(dangling_rc.is_none(), "dangling row removed");
    }

    /// `store_stats` (Area 3 / `hh stats`) reports session/event/blob counts,
    /// a non-empty DB footprint, and the largest sessions in descending order.
    #[test]
    fn store_stats_reports_counts_and_largest() {
        let (_tmp, store) = open_store();
        let a = store.create_session(&new_session()).unwrap();
        let mut b = new_session();
        b.started_at += 1000;
        let b = store.create_session(&b).unwrap();
        {
            let writer = store.event_writer().unwrap();
            for ts in 0..3 {
                writer.append_event(event(&a.id, ts, Some(ts + 1))).unwrap();
            }
            writer.append_event(event(&b.id, 0, Some(1))).unwrap();
            // A referenced blob on `a` (a 4th event).
            let live = store.blobs().put(b"stats live blob").unwrap();
            writer
                .append_event(Event {
                    kind: EventKind::FileChange,
                    blob_hash: Some(live.hash.clone()),
                    blob_size: Some(live.size),
                    summary: "blob".into(),
                    ..event(&a.id, 5, Some(4))
                })
                .unwrap();
            writer.finish().unwrap();
        }
        // An orphan blob file on disk (no row) so blobs_dir_bytes > blobs row.
        let _orphan = store.blobs().put(b"stats orphan blob").unwrap();

        let stats = store.store_stats(5).unwrap();
        assert_eq!(stats.sessions, 2);
        assert_eq!(stats.events, 5, "a has 4 (3 + blob event), b has 1");
        assert_eq!(stats.blobs_count, 1, "only the referenced blob has a row");
        assert!(stats.blobs_uncompressed_bytes > 0);
        assert!(stats.db_bytes > 0);
        assert!(stats.blobs_dir_bytes > 0, "two blob files on disk");
        assert_eq!(stats.largest_sessions.len(), 2);
        assert_eq!(stats.largest_sessions[0].id, a.id);
        assert_eq!(stats.largest_sessions[0].event_count, 4);
        assert_eq!(stats.largest_sessions[1].event_count, 1);
    }

    /// `for_each_event_detail` (Area 2) streams a session's events in
    /// `(ts_ms, id)` order, resolves a blob-overflow body inline, and attaches
    /// the `file_changes` row — the constant-memory primitive behind
    /// `hh inspect --json`.
    #[test]
    fn for_each_event_detail_streams_in_order_and_resolves_blob() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        let real_body = serde_json::json!({"text": "resolved payload"});
        let out = store
            .blobs()
            .put(serde_json::to_vec(&real_body).unwrap().as_slice())
            .unwrap();
        let envelope = serde_json::json!({
            "overflow": true,
            "size": out.size,
            "blob_hash": out.hash,
            "encoding": "blob",
        });
        let writer = store.event_writer().unwrap();
        let id_plain = writer.append_event(event(&created.id, 0, Some(1))).unwrap();
        let id_blob = writer
            .append_event(Event {
                body_json: Some(envelope),
                blob_hash: Some(out.hash.clone()),
                blob_size: Some(out.size),
                ..event(&created.id, 1, Some(2))
            })
            .unwrap();
        let id_fc = writer
            .append_file_change(
                Event {
                    kind: EventKind::FileChange,
                    summary: "file changed".into(),
                    ..event(&created.id, 2, Some(3))
                },
                FileChange {
                    event_id: 0,
                    path: "src/lib.rs".into(),
                    change_kind: ChangeKind::Modified,
                    before_hash: None,
                    after_hash: None,
                    is_binary: false,
                },
            )
            .unwrap();
        writer.finish().unwrap();

        let mut got: Vec<EventDetail> = Vec::new();
        store
            .for_each_event_detail(&created.id, |d| {
                got.push(d);
                Ok(())
            })
            .unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].id, id_plain);
        assert_eq!(got[0].body_json.as_ref().unwrap()["text"], "hi");
        assert!(got[0].file_change.is_none());
        assert_eq!(got[1].id, id_blob);
        assert_eq!(
            got[1].body_json.as_ref().unwrap(),
            &real_body,
            "blob overflow resolved inline"
        );
        assert_eq!(got[2].id, id_fc);
        let fc = got[2].file_change.as_ref().expect("file_change attached");
        assert_eq!(fc.path, "src/lib.rs");
        assert_eq!(fc.event_id, id_fc);
    }

    /// Area 2 big-session hardening: a synthetic 100k-event session must
    /// stream through `for_each_event_detail` (the `hh inspect --json` path)
    /// and load its event index (the `hh replay` open path) at this scale
    /// without collecting every row in RAM. `#[ignore]`d because 100k rows are
    /// too heavy for the default per-PR suite — run with `cargo test -p hh-core
    /// -- --ignored for_each_event_detail_streams_100k` to re-verify (the
    /// manual scale check documented in `docs/performance.md`). Events are
    /// bulk-inserted in one transaction for speed; the system under test is
    /// the *read* path, which is unaffected by how the rows got there.
    #[test]
    #[ignore = "100k-row scale check; run with --ignored (docs/performance.md)"]
    fn for_each_event_detail_streams_100k_session() {
        let (tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        let sid = created.id.clone();
        let db_path = tmp.path().join("hh.db");
        let blobs_path = tmp.path().join("blobs");
        // Release the store's connection before bulk-inserting so a single
        // connection owns the write transaction.
        drop(store);

        // Bulk-insert 100k events in one transaction. The writer thread's
        // per-event channel round-trip would make this test unacceptably slow;
        // this is fixture setup, not the system under test.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute("BEGIN", []).unwrap();
            let mut stmt = conn
                .prepare(
                    "INSERT INTO events
                       (session_id, ts_ms, kind, step, summary, body_json,
                        blob_hash, correlates)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL)",
                )
                .unwrap();
            for i in 0i64..100_000 {
                stmt.execute(params![
                    &sid,
                    i,
                    "agent_message",
                    Some((i / 4) + 1),
                    format!("event {i}"),
                    r#"{"text":"x"}"#,
                ])
                .unwrap();
            }
            drop(stmt);
            conn.execute("COMMIT", []).unwrap();
        }

        // Reopen so the read path starts from a clean snapshot of all 100k
        // committed rows (and so `Store::open` re-runs `heal_steps`, which the
        // migration 0002 partial index keeps O(rows-needing-heal)=O(0) here:
        // every event has a non-null step).
        let store = Store::open(&db_path, &blobs_path).unwrap();

        // The replay-open path: the event index + timeline over 100k rows.
        let index = store.list_event_index(&sid).unwrap();
        assert_eq!(index.len(), 100_000, "index loads all 100k rows");
        let timeline = crate::build_timeline(&index, false);
        assert!(!timeline.is_empty(), "timeline builds over 100k rows");

        // The `hh inspect --json` path: stream 100k events in constant memory
        // — the closure touches one EventDetail at a time; nothing accumulates.
        // A regression that re-introduced a `Vec::with_capacity(index.len())`
        // would still pass the count but balloon memory; the ordered-stream
        // assertion plus the closure's O(1) shape is the structural guard.
        let mut count = 0u64;
        let mut last_ts = i64::MIN;
        store
            .for_each_event_detail(&sid, |d| {
                count += 1;
                assert!(d.ts_ms >= last_ts, "stream is ordered by ts_ms, id");
                last_ts = d.ts_ms;
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 100_000, "stream emits all 100k events in order");
    }

    #[test]
    fn create_session_honors_adapter_status() {
        let (_tmp, store) = open_store();
        let mut ns = new_session();
        ns.adapter_status = AdapterStatus::Active;
        let created = store.create_session(&ns).unwrap();
        let status: String = store
            .conn
            .query_row(
                "SELECT adapter_status FROM sessions WHERE id = ?1",
                params![&created.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "active");
    }

    #[test]
    fn set_session_adapter_meta_updates_and_preserves() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        store
            .set_session_adapter_meta(
                &created.id,
                Some("claude-sonnet-5"),
                Some(&serde_json::json!({"input_tokens": 42, "output_tokens": 7})),
                AdapterStatus::Active,
            )
            .unwrap();
        let (model, usage, status): (Option<String>, Option<String>, String) = store
            .conn
            .query_row(
                "SELECT model, usage_json, adapter_status FROM sessions WHERE id = ?1",
                params![&created.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(model.as_deref(), Some("claude-sonnet-5"));
        let usage_val: serde_json::Value = serde_json::from_str(&usage.unwrap()).unwrap();
        assert_eq!(usage_val["input_tokens"], 42);
        assert_eq!(usage_val["output_tokens"], 7);
        assert_eq!(status, "active");

        // Passing None for model/usage must not clobber (COALESCE); status is
        // always overwritten.
        store
            .set_session_adapter_meta(&created.id, None, None, AdapterStatus::Degraded)
            .unwrap();
        let (model, status): (Option<String>, String) = store
            .conn
            .query_row(
                "SELECT model, adapter_status FROM sessions WHERE id = ?1",
                params![&created.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            model.as_deref(),
            Some("claude-sonnet-5"),
            "model survives a None update"
        );
        assert_eq!(status, "degraded", "status is always overwritten");
    }

    #[test]
    fn assign_steps_shares_call_and_result_step() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        {
            let writer = store.event_writer().unwrap();
            let call_id = writer
                .append_event(Event {
                    kind: EventKind::ToolCall,
                    body_json: Some(serde_json::json!({"correlate_key": "tu_1"})),
                    summary: "call".into(),
                    ..event(&created.id, 0, None)
                })
                .unwrap();
            writer
                .append_event(Event {
                    kind: EventKind::ToolResult,
                    correlates: Some(call_id),
                    body_json: Some(serde_json::json!({"correlate_key": "tu_1"})),
                    summary: "result".into(),
                    ..event(&created.id, 5, None)
                })
                .unwrap();
            writer.finish().unwrap();
        }
        store.assign_steps(&created.id).unwrap();
        let steps: Vec<Option<i64>> = store
            .conn
            .prepare("SELECT step FROM events WHERE session_id = ?1 ORDER BY ts_ms, id")
            .unwrap()
            .query_map(params![&created.id], |r| r.get::<_, Option<i64>>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(steps, vec![Some(1), Some(1)], "call+result share step 1");
    }

    #[test]
    fn assign_steps_self_heals_on_open() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("hh.db");
        let blobs = tmp.path().join("blobs");
        let created = {
            let store = Store::open(&db, &blobs).unwrap();
            let created = store.create_session(&new_session()).unwrap();
            {
                let writer = store.event_writer().unwrap();
                let call_id = writer
                    .append_event(Event {
                        kind: EventKind::ToolCall,
                        summary: "call".into(),
                        ..event(&created.id, 0, None)
                    })
                    .unwrap();
                writer
                    .append_event(Event {
                        kind: EventKind::ToolResult,
                        correlates: Some(call_id),
                        summary: "result".into(),
                        ..event(&created.id, 5, None)
                    })
                    .unwrap();
                writer.finish().unwrap();
            }
            // Do NOT call assign_steps: steps stay NULL.
            created.id
        };
        // Reopen: Store::open self-heals the NULL steps (ADR-0002).
        let store = Store::open(&db, &blobs).unwrap();
        let steps: Vec<Option<i64>> = store
            .conn
            .prepare("SELECT step FROM events WHERE session_id = ?1 ORDER BY ts_ms, id")
            .unwrap()
            .query_map(params![created], |r| r.get::<_, Option<i64>>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            steps,
            vec![Some(1), Some(1)],
            "self-heal assigned shared step 1"
        );
    }

    #[test]
    fn list_sessions_counts_distinct_steps() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        {
            let writer = store.event_writer().unwrap();
            // call + result share step 1; agent_message is step 2. The old
            // COUNT(*) WHERE kind != 'terminal_output' would report 3; the new
            // COUNT(DISTINCT step) reports 2.
            writer
                .append_event(Event {
                    kind: EventKind::ToolCall,
                    summary: "c".into(),
                    ..event(&created.id, 0, Some(1))
                })
                .unwrap();
            writer
                .append_event(Event {
                    kind: EventKind::ToolResult,
                    summary: "r".into(),
                    ..event(&created.id, 5, Some(1))
                })
                .unwrap();
            writer
                .append_event(Event {
                    kind: EventKind::AgentMessage,
                    summary: "m".into(),
                    ..event(&created.id, 10, Some(2))
                })
                .unwrap();
            writer.finish().unwrap();
        }
        let rows = store.list_sessions(10).unwrap();
        assert_eq!(
            rows[0].step_count, 2,
            "distinct steps should be 2 (shared 1 + 2), not 3"
        );
    }

    #[test]
    fn delete_session_with_correlated_events() {
        // R7: events.correlates is a self-FK with no ON DELETE clause. Deleting
        // a session whose events are correlated must not trip RESTRICT.
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        {
            let writer = store.event_writer().unwrap();
            let call_id = writer
                .append_event(Event {
                    kind: EventKind::ToolCall,
                    summary: "call".into(),
                    ..event(&created.id, 0, Some(1))
                })
                .unwrap();
            writer
                .append_event(Event {
                    kind: EventKind::ToolResult,
                    correlates: Some(call_id),
                    summary: "result".into(),
                    ..event(&created.id, 5, Some(1))
                })
                .unwrap();
            writer.finish().unwrap();
        }
        store.delete_session(&created.id).unwrap();
        let count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                params![&created.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "all events removed on session delete");
    }

    #[test]
    fn get_session_returns_full_row() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        let row = store.get_session(&created.id).unwrap();
        assert_eq!(row.id, created.id);
        assert_eq!(row.short_id, created.short_id);
        assert_eq!(row.status, SessionStatus::Recording);
    }

    #[test]
    fn get_session_not_found() {
        let (_tmp, store) = open_store();
        let err = store.get_session("does-not-exist").unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Storage(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn list_event_index_includes_terminal_output_with_summary() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        {
            let writer = store.event_writer().unwrap();
            writer
                .append_event(Event {
                    summary: "hi".into(),
                    ..event(&created.id, 0, Some(1))
                })
                .unwrap();
            writer
                .append_event(Event {
                    kind: EventKind::TerminalOutput,
                    step: None,
                    summary: "chunk".into(),
                    ..event(&created.id, 1, None)
                })
                .unwrap();
            writer.finish().unwrap();
        }
        let idx = store.list_event_index(&created.id).unwrap();
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[0].summary, "hi");
        assert_eq!(idx[1].kind, EventKind::TerminalOutput);
        assert_eq!(idx[1].summary, "chunk");
        assert!(idx[1].step.is_none());
    }

    #[test]
    fn get_event_detail_returns_inline_body() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        let writer = store.event_writer().unwrap();
        let id = writer.append_event(event(&created.id, 0, Some(1))).unwrap();
        writer.finish().unwrap();
        let detail = store.get_event_detail(id).unwrap();
        assert_eq!(detail.id, id);
        assert_eq!(detail.summary, "hello");
        assert_eq!(detail.body_json.unwrap()["text"], "hi");
        assert!(detail.file_change.is_none());
    }

    #[test]
    fn get_event_detail_not_found() {
        let (_tmp, store) = open_store();
        let err = store.get_event_detail(999_999).unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Storage(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn get_event_detail_resolves_blob_overflow() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        let real_body = serde_json::json!({"text": "the real payload"});
        let out = store
            .blobs()
            .put(serde_json::to_vec(&real_body).unwrap().as_slice())
            .unwrap();
        let envelope = serde_json::json!({
            "overflow": true,
            "size": out.size,
            "blob_hash": out.hash,
            "encoding": "blob",
        });
        let writer = store.event_writer().unwrap();
        let id = writer
            .append_event(Event {
                body_json: Some(envelope),
                blob_hash: Some(out.hash.clone()),
                blob_size: Some(out.size),
                ..event(&created.id, 0, Some(1))
            })
            .unwrap();
        writer.finish().unwrap();
        let detail = store.get_event_detail(id).unwrap();
        assert_eq!(
            detail.body_json.unwrap(),
            real_body,
            "overflow envelope must be transparently resolved to the real payload"
        );
    }

    #[test]
    fn get_event_detail_includes_file_change() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        let writer = store.event_writer().unwrap();
        let id = writer
            .append_file_change(
                Event {
                    kind: EventKind::FileChange,
                    summary: "file changed".into(),
                    ..event(&created.id, 0, Some(1))
                },
                FileChange {
                    event_id: 0,
                    path: "src/main.rs".into(),
                    change_kind: ChangeKind::Modified,
                    before_hash: None,
                    after_hash: None,
                    is_binary: false,
                },
            )
            .unwrap();
        writer.finish().unwrap();
        let detail = store.get_event_detail(id).unwrap();
        let fc = detail.file_change.expect("file_change must be attached");
        assert_eq!(fc.path, "src/main.rs");
        assert_eq!(fc.change_kind, ChangeKind::Modified);
    }

    #[test]
    fn get_correlated_event_resolves_both_directions() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        let writer = store.event_writer().unwrap();
        let call_id = writer
            .append_event(Event {
                kind: EventKind::ToolCall,
                summary: "call".into(),
                ..event(&created.id, 0, Some(1))
            })
            .unwrap();
        let result_id = writer
            .append_event(Event {
                kind: EventKind::ToolResult,
                correlates: Some(call_id),
                summary: "result".into(),
                ..event(&created.id, 5, Some(1))
            })
            .unwrap();
        writer.finish().unwrap();

        // From the result (has `correlates` set): resolves to the call.
        let from_result = store
            .get_correlated_event(result_id, Some(call_id))
            .unwrap()
            .expect("result must resolve to its call");
        assert_eq!(from_result.id, call_id);

        // From the call (no `correlates`): resolves via reverse lookup to the result.
        let from_call = store
            .get_correlated_event(call_id, None)
            .unwrap()
            .expect("call must resolve to its result");
        assert_eq!(from_call.id, result_id);
    }

    #[test]
    fn get_correlated_event_none_when_uncorrelated() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();
        let writer = store.event_writer().unwrap();
        let id = writer.append_event(event(&created.id, 0, Some(1))).unwrap();
        writer.finish().unwrap();
        assert!(store.get_correlated_event(id, None).unwrap().is_none());
    }

    /// FR-3.5/NFR-1: the replay TUI loads the full per-event index eagerly at
    /// startup. Benchmark-ish: a synthetic 10k-event session (half plain
    /// messages, half correlated tool_call/tool_result pairs) must load and
    /// group into a timeline well within a generous CI bound. Not a tight
    /// perf assertion (CI machines vary widely) — it exists to catch an
    /// accidental O(n²) regression (e.g. a linear scan per row), not to police
    /// exact timings.
    #[test]
    fn list_event_index_loads_10k_events_within_generous_bound() {
        let (_tmp, store) = open_store();
        let created = store.create_session(&new_session()).unwrap();

        // Seed 10k events (5k correlated tool_call/tool_result pairs) directly
        // via one transaction on the store's own connection, bypassing the
        // single-writer channel's per-event round trip: that round trip (one
        // mpsc send/recv + one autocommit per call) is real and correct for a
        // live recorder, but would make *this* test measure writer throughput
        // instead of the eager-index-load path FR-3.5 actually cares about.
        {
            let tx = store.conn.unchecked_transaction().unwrap();
            for i in 0..5_000i64 {
                let call_ts = i * 2;
                let result_ts = call_ts + 1;
                tx.execute(
                    "INSERT INTO events (session_id, ts_ms, kind, summary) VALUES (?1, ?2, 'tool_call', ?3)",
                    params![created.id, call_ts, format!("tool_call #{i}")],
                )
                .unwrap();
                let call_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO events (session_id, ts_ms, kind, summary, correlates) VALUES (?1, ?2, 'tool_result', ?3, ?4)",
                    params![created.id, result_ts, format!("tool_result #{i}"), call_id],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        store.assign_steps(&created.id).unwrap();

        let start = std::time::Instant::now();
        let index = store.list_event_index(&created.id).unwrap();
        let rows = crate::timeline::build_timeline(&index, false);
        let elapsed = start.elapsed();

        assert_eq!(index.len(), 10_000, "5k call+result pairs = 10k events");
        assert_eq!(rows.len(), 5_000, "each pair collapses to one step row");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "loading + grouping 10k events took {elapsed:?}, expected well under 2s"
        );
    }

    /// Property tests for blob refcounting + GC (`delete_session`'s
    /// decrement-then-GC logic). For any sequence of session creates, blob-
    /// referencing appends (from a small shared content pool, so blobs are
    /// referenced by multiple events/sessions), and session deletes: the DB
    /// refcount for a blob always equals its live reference count, a blob with
    /// live references is never GC'd, and a blob with none is always GC'd
    /// (both its `blobs` row and its on-disk file).
    mod blob_refcount_prop {
        use super::*;
        use proptest::prelude::*;
        use std::collections::HashMap;

        /// A small shared pool of blob contents so appends across sessions
        /// collide on the same hash (exercising the shared-refcount path).
        const BLOB_CONTENTS: [&[u8]; 3] = [b"blob-a-content", b"blob-b-content", b"blob-c-content"];

        /// A small pool of session "slots" (0..4) so creates/deletes/appends
        /// interleave against a bounded, reusable set of sessions.
        #[derive(Debug, Clone)]
        enum Op {
            Create,
            Append { session: u8, blob: u8 },
            Delete { session: u8 },
        }

        fn op_strategy() -> impl Strategy<Value = Op> {
            prop_oneof![
                Just(Op::Create),
                (0u8..4, 0u8..3).prop_map(|(session, blob)| Op::Append { session, blob }),
                (0u8..4).prop_map(|session| Op::Delete { session }),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

            #[test]
            fn refcounts_match_live_refs_and_gc_is_exact(
                ops in prop::collection::vec(op_strategy(), 1..40)
            ) {
                let (_tmp, store) = open_store();
                let writer = store.event_writer().unwrap();
                // slot -> live session id (absent = not created yet / deleted).
                let mut sessions: HashMap<u8, String> = HashMap::new();
                // Our own ground-truth: hash -> count of *live* events referencing it.
                let mut expected_refs: HashMap<String, i64> = HashMap::new();
                let mut next_started_at = 1_700_000_000_000i64;
                let mut ts = 0i64;

                for op in ops {
                    match op {
                        Op::Create => {
                            if let Some(slot) = (0u8..4).find(|s| !sessions.contains_key(s)) {
                                let mut ns = new_session();
                                ns.started_at = next_started_at;
                                next_started_at += 1;
                                let created = store.create_session(&ns).unwrap();
                                sessions.insert(slot, created.id);
                            }
                        }
                        Op::Append { session, blob } => {
                            let Some(sid) = sessions.get(&session) else { continue };
                            let content = BLOB_CONTENTS[usize::from(blob % 3)];
                            let out = store.blobs().put(content).unwrap();
                            ts += 1;
                            writer
                                .append_event(Event {
                                    kind: EventKind::FileChange,
                                    blob_hash: Some(out.hash.clone()),
                                    blob_size: Some(out.size),
                                    summary: "file changed".into(),
                                    ..event(sid, ts, Some(1))
                                })
                                .unwrap();
                            *expected_refs.entry(out.hash).or_insert(0) += 1;
                        }
                        Op::Delete { session } => {
                            let Some(sid) = sessions.remove(&session) else { continue };
                            // This session's per-blob reference counts, read before the
                            // delete cascades (mirrors delete_session's own query).
                            let refs: Vec<(String, i64)> = {
                                let mut stmt = store
                                    .conn
                                    .prepare(
                                        "SELECT blob_hash, COUNT(*) FROM events \
                                         WHERE session_id = ?1 AND blob_hash IS NOT NULL \
                                         GROUP BY blob_hash",
                                    )
                                    .unwrap();
                                stmt.query_map(params![sid], |r| {
                                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                                })
                                .unwrap()
                                .collect::<std::result::Result<_, _>>()
                                .unwrap()
                            };
                            store.delete_session(&sid).unwrap();
                            for (hash, count) in refs {
                                if let Some(e) = expected_refs.get_mut(&hash) {
                                    *e -= count;
                                }
                            }
                        }
                    }

                    for (hash, expected) in &expected_refs {
                        prop_assert!(
                            *expected >= 0,
                            "test bookkeeping went negative — bug in the test itself"
                        );
                        let row: Option<i64> = store
                            .conn
                            .query_row(
                                "SELECT refcount FROM blobs WHERE hash = ?1",
                                params![hash],
                                |r| r.get(0),
                            )
                            .optional()
                            .unwrap();
                        let path = store.blobs().blob_path(hash);
                        if *expected > 0 {
                            prop_assert_eq!(
                                row, Some(*expected),
                                "DB refcount must equal the live reference count"
                            );
                            prop_assert!(path.exists(), "a referenced blob must stay on disk");
                        } else {
                            prop_assert_eq!(row, None, "an unreferenced blob's row must be GC'd");
                            prop_assert!(!path.exists(), "an unreferenced blob's file must be GC'd");
                        }
                    }
                }
                writer.finish().unwrap();
            }
        }
    }

    /// Property test for migration idempotency (DR-1): applying the migration
    /// set to a fresh DB, then reopening (which re-runs `run_migrations` and is
    /// a no-op past `LATEST_VERSION`) any number of extra times, always yields
    /// byte-identical schema.
    mod migration_idempotency_prop {
        use proptest::prelude::*;
        use tempfile::TempDir;

        fn schema_dump(conn: &rusqlite::Connection) -> Vec<String> {
            let mut stmt = conn
                .prepare(
                    "SELECT sql FROM sqlite_master \
                     WHERE sql IS NOT NULL ORDER BY type, name",
                )
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        }

        proptest! {
            #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

            #[test]
            fn reopening_any_number_of_times_is_a_schema_no_op(extra_opens in 1usize..6) {
                let tmp = TempDir::new().unwrap();
                let db = tmp.path().join("hh.db");
                let blobs = tmp.path().join("blobs");

                let first = crate::store::Store::open(&db, &blobs).unwrap();
                let baseline = schema_dump(&first.conn);
                drop(first);

                for _ in 0..extra_opens {
                    let store = crate::store::Store::open(&db, &blobs).unwrap();
                    let dump = schema_dump(&store.conn);
                    prop_assert_eq!(&dump, &baseline, "schema drifted after re-opening");
                    drop(store);
                }
            }
        }
    }
}
