//! Dropped-event accounting for the single-writer task (BUG: silent write
//! loss).
//!
//! A recorder that silently stops recording is the one failure mode
//! halfhand cannot have. Every event that fails to reach the database — an
//! `INSERT` the writer task could not commit, a request sent after the writer
//! thread died, or an event a recording source could not even build because
//! its blob could not be stored — is counted here. The tally is owned by the
//! [`EventWriter`](super::EventWriter) (one per recording session) and shared
//! with its writer thread. It is a counter, not a second writer: no
//! `Connection` is involved.
//!
//! The first drop prints exactly one warning line to stderr naming what failed
//! and why; later drops only bump the counter, so a disk-full condition that
//! fails ten thousand appends still produces one line. At finalize the
//! recorder persists the count and the first cause to
//! `sessions.dropped_events` / `sessions.dropped_events_reason`, following
//! the `adapter_degrade_reason` pattern (a nullable column added idempotently
//! in [`Store::open`](super::Store::open)).

use std::fmt::Display;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use rusqlite::{params, Connection};

use super::Store;
use crate::error::Result;

/// A snapshot of how many events a writer failed to persist, and why the
/// first one failed. `count == 0` means the session recorded cleanly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DroppedEvents {
    /// Number of events that were not written to the database.
    pub count: u64,
    /// Human-readable cause of the first failure (what failed and why), or
    /// `None` when nothing was dropped.
    pub first_error: Option<String>,
}

impl DroppedEvents {
    /// `true` when at least one event was dropped.
    #[must_use]
    pub fn any(&self) -> bool {
        self.count > 0
    }

    /// Read the tally from an exported bundle's `session` object. Bundles
    /// from before the field existed (or with a malformed value) read as
    /// clean — never an error, never a panic.
    pub(crate) fn from_bundle_session(session: &serde_json::Value) -> Self {
        let count = session
            .get("dropped_events")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let first_error = session
            .get("dropped_events_reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        Self { count, first_error }
    }
}

/// Where the one-time warning goes. Stderr in production; tests inject a
/// capturing sink to assert the warning is emitted exactly once.
pub(crate) type WarnSink = Arc<dyn Fn(&str) + Send + Sync>;

/// The default sink: one line on stderr. The line starts with `\r` and ends
/// with `\r\n` because the agent usually owns the terminal in raw mode while
/// the writer is running; a bare `\n` there would leave the cursor mid-row
/// and smear the warning across the agent's TUI.
pub(crate) fn stderr_sink() -> WarnSink {
    Arc::new(|line: &str| eprint!("\r{line}\r\n"))
}

/// Shared dropped-event counter (see the module docs).
pub(crate) struct DropTally {
    count: AtomicU64,
    first_error: OnceLock<String>,
    warn: WarnSink,
}

impl DropTally {
    pub(crate) fn new(warn: WarnSink) -> Self {
        Self {
            count: AtomicU64::new(0),
            first_error: OnceLock::new(),
            warn,
        }
    }

    /// Count one dropped event caused by `cause`.
    pub(crate) fn record(&self, cause: impl Display) {
        self.count.fetch_add(1, Ordering::AcqRel);
        self.note_cause(cause);
    }

    /// Retain `cause` as the first failure (and print the one warning) if no
    /// cause has been retained yet, without counting a drop. Used when the
    /// writer thread itself fails before any event is in flight: the events
    /// sent afterwards are counted on the sender side.
    pub(crate) fn note_cause(&self, cause: impl Display) {
        // `OnceLock::set` succeeds for exactly one caller, which makes this
        // the single place the warning can be printed.
        let msg = cause.to_string();
        if self.first_error.set(msg.clone()).is_ok() {
            (self.warn)(&format!(
                "hh: warning: event write failed: {msg} — recording continues, but \
                 events are being dropped; the session will be marked degraded \
                 (check free disk space and permissions on the data dir, then run `hh doctor`)"
            ));
        }
    }

    pub(crate) fn snapshot(&self) -> DroppedEvents {
        let count = self.count.load(Ordering::Acquire);
        DroppedEvents {
            count,
            // A cause noted without any counted drop (writer failed to start,
            // but nothing was sent afterwards) is not a dropped event.
            first_error: if count > 0 {
                self.first_error.get().cloned()
            } else {
                None
            },
        }
    }
}

/// Idempotently add `sessions.dropped_events INTEGER NOT NULL DEFAULT 0` and
/// `sessions.dropped_events_reason TEXT`. Same mechanism as
/// `add_adapter_degrade_reason_column`: SQLite has no `ADD COLUMN IF NOT
/// EXISTS`, so `PRAGMA table_info` is the idempotency check. Deliberately not
/// a numbered migration: the column add runs on every open regardless of
/// `schema_migrations`, so it cannot be skipped by the runner's
/// `MAX(version)` check whatever order sibling migrations land in.
pub(super) fn add_dropped_events_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(std::result::Result::ok)
        .collect();
    if !names.iter().any(|n| n == "dropped_events") {
        conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN dropped_events INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if !names.iter().any(|n| n == "dropped_events_reason") {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN dropped_events_reason TEXT")?;
    }
    Ok(())
}

impl Store {
    /// Add a writer's dropped-event tally to a session (see
    /// [`DroppedEvents`]). Additive, because more than one writer can feed a
    /// session — the recorder and an attached `hh mcp-proxy` each own one —
    /// and the first cause recorded for the session is kept. A zero tally is
    /// a no-op, so a clean session never gains a reason.
    pub fn add_session_dropped_events(&self, id: &str, dropped: &DroppedEvents) -> Result<()> {
        if !dropped.any() {
            return Ok(());
        }
        let count = i64::try_from(dropped.count).unwrap_or(i64::MAX);
        self.conn.execute(
            "UPDATE sessions SET
                dropped_events = dropped_events + ?1,
                dropped_events_reason = COALESCE(dropped_events_reason, ?2)
             WHERE id = ?3",
            params![count, dropped.first_error, id],
        )?;
        Ok(())
    }

    /// Read a session's persisted dropped-event tally. An unknown id reads as
    /// a clean (zero) tally; callers resolve the id first.
    pub fn session_dropped_events(&self, id: &str) -> Result<DroppedEvents> {
        let row: Option<(i64, Option<String>)> =
            rusqlite::OptionalExtension::optional(self.conn.query_row(
                "SELECT dropped_events, dropped_events_reason FROM sessions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            ))?;
        Ok(
            row.map_or_else(DroppedEvents::default, |(n, reason)| DroppedEvents {
                count: u64::try_from(n).unwrap_or(0),
                first_error: reason,
            }),
        )
    }

    /// The most recent `limit` sessions (newest first) with their dropped
    /// tallies, as `(short_id, tally)` — for `hh doctor`.
    pub fn recent_dropped_events(&self, limit: u32) -> Result<Vec<(String, DroppedEvents)>> {
        let mut stmt = self.conn.prepare(
            "SELECT short_id, dropped_events, dropped_events_reason FROM sessions
             ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![i64::from(limit)], |r| {
            Ok((
                r.get::<_, String>(0)?,
                DroppedEvents {
                    count: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                    first_error: r.get(2)?,
                },
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AdapterStatus, AgentKind, Event, EventKind, NewSession};
    use std::sync::Mutex;
    use tempfile::TempDir;

    fn capture() -> (WarnSink, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let l2 = Arc::clone(&lines);
        let sink: WarnSink = Arc::new(move |s: &str| l2.lock().unwrap().push(s.to_string()));
        (sink, lines)
    }

    fn session(store: &Store) -> String {
        store
            .create_session(&NewSession {
                id: crate::event::now_v7(),
                started_at: 1,
                agent_kind: AgentKind::Generic,
                adapter_status: AdapterStatus::None,
                command: vec!["x".into()],
                cwd: "/tmp".into(),
                hostname: None,
                hh_version: "t".into(),
                model: None,
                git_branch: None,
                git_sha: None,
                git_dirty: None,
            })
            .unwrap()
            .id
    }

    fn ev(sid: &str, kind: EventKind, summary: &str) -> Event {
        Event {
            session_id: sid.to_string(),
            ts_ms: 0,
            kind,
            step: None,
            summary: summary.into(),
            body_json: None,
            blob_hash: None,
            blob_size: None,
            correlates: None,
        }
    }

    /// Inject a real SQLite failure (no mocks): a trigger that aborts every
    /// `terminal_output` insert, standing in for disk-full / I/O errors.
    fn inject_terminal_failure(store: &Store) {
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER hh_test_fail BEFORE INSERT ON events
                 WHEN NEW.kind = 'terminal_output'
                 BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
            )
            .unwrap();
    }

    #[test]
    fn failing_writes_are_counted_and_warned_once_and_recording_continues() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let sid = session(&store);
        inject_terminal_failure(&store);
        let (sink, lines) = capture();
        let mut w = store.spawn_event_writer(None, sink).unwrap();
        for i in 0..50 {
            assert!(w
                .append_event(ev(&sid, EventKind::TerminalOutput, &format!("t{i}")))
                .is_err());
        }
        // Recording continues: other events still land after the failures.
        w.append_event(ev(&sid, EventKind::Lifecycle, "still recording"))
            .unwrap();
        let dropped = w.dropped();
        w.close().unwrap();

        assert_eq!(dropped.count, 50);
        let cause = dropped.first_error.clone().unwrap();
        assert!(cause.contains("injected write failure"), "{cause}");
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1, "exactly one warning line: {lines:?}");
        assert!(lines[0].starts_with("hh: warning: event write failed:"));
        assert!(!lines[0].contains('\n'), "warning must be one line");
        let idx = store.list_event_index(&sid).unwrap();
        assert!(idx.iter().any(|e| e.summary == "still recording"));

        store.add_session_dropped_events(&sid, &dropped).unwrap();
        assert_eq!(store.session_dropped_events(&sid).unwrap(), dropped);
        let row = store.get_session(&sid).unwrap();
        assert_eq!(row.dropped_events, 50);
        assert_eq!(row.dropped_events_reason.as_deref(), Some(cause.as_str()));

        // A second writer on the same session (an attached mcp-proxy) adds
        // to the count and keeps the session's first cause.
        let more = DroppedEvents {
            count: 2,
            first_error: Some("later cause".into()),
        };
        store.add_session_dropped_events(&sid, &more).unwrap();
        let after = store.session_dropped_events(&sid).unwrap();
        assert_eq!(after.count, 52);
        assert_eq!(after.first_error.as_deref(), Some(cause.as_str()));
    }

    #[test]
    fn clean_writer_reports_nothing_and_prints_nothing() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let sid = session(&store);
        let (sink, lines) = capture();
        let mut w = store.spawn_event_writer(None, sink).unwrap();
        w.append_event(ev(&sid, EventKind::TerminalOutput, "fine"))
            .unwrap();
        let dropped = w.dropped();
        w.close().unwrap();
        assert_eq!(dropped, DroppedEvents::default());
        assert!(lines.lock().unwrap().is_empty());
        store.add_session_dropped_events(&sid, &dropped).unwrap();
        let row = store.get_session(&sid).unwrap();
        assert_eq!(row.dropped_events, 0);
        assert_eq!(row.dropped_events_reason, None);
    }

    #[test]
    fn appends_after_writer_closed_are_counted() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let sid = session(&store);
        let (sink, lines) = capture();
        let mut w = store.spawn_event_writer(None, sink).unwrap();
        w.close().unwrap();
        for _ in 0..3 {
            assert!(w
                .append_event(ev(&sid, EventKind::Lifecycle, "late"))
                .is_err());
        }
        assert_eq!(w.dropped().count, 3);
        assert_eq!(lines.lock().unwrap().len(), 1);
    }

    #[test]
    fn source_side_drops_share_the_tally() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let (sink, lines) = capture();
        let mut w = store.spawn_event_writer(None, sink).unwrap();
        w.record_dropped("blob put a.txt: No space left on device");
        w.record_dropped("blob put b.txt: No space left on device");
        let d = w.dropped();
        w.close().unwrap();
        assert_eq!(d.count, 2);
        assert_eq!(
            d.first_error.as_deref(),
            Some("blob put a.txt: No space left on device")
        );
        assert_eq!(lines.lock().unwrap().len(), 1);
    }

    #[test]
    fn dropped_columns_are_added_idempotently() {
        let tmp = TempDir::new().unwrap();
        let (db, blobs) = (tmp.path().join("hh.db"), tmp.path().join("blobs"));
        {
            let store = Store::open(&db, &blobs).unwrap();
            // Roll back to the pre-change shape, as on a DB from an older hh.
            store
                .conn
                .execute_batch(
                    "ALTER TABLE sessions DROP COLUMN dropped_events;
                     ALTER TABLE sessions DROP COLUMN dropped_events_reason;",
                )
                .unwrap();
        }
        for _ in 0..3 {
            let store = Store::open(&db, &blobs).unwrap();
            let sid = session(&store);
            assert_eq!(
                store.session_dropped_events(&sid).unwrap(),
                DroppedEvents::default()
            );
        }
    }
}
