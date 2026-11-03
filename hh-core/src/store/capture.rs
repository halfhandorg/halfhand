//! Capture-fidelity facts for `hh doctor --capture`.
//!
//! Everything here is a read-only aggregate over data the recorder already
//! persists — `sessions` and `file_changes` (with migration 0006's
//! `before_source` / `renamed_from` provenance). No column is added: facts
//! the recorder does not persist (which events the watcher saw live versus
//! which the shutdown backstop recovered, permission-denied watches, a
//! backend overflow) are reported as unknown by the caller, never guessed.

use rusqlite::{params, OptionalExtension};

use super::{DroppedEvents, Store};
use crate::error::Result;

/// File-change capture facts for one recorded session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionCapture {
    /// Full session id.
    pub session_id: String,
    /// Six-hex-char short id.
    pub short_id: String,
    /// The session's working directory — the root the file watcher was
    /// asked to watch (the recorder watches exactly one root, the cwd).
    pub cwd: String,
    /// Session status (`recording`, `ok`, `error`, `interrupted`).
    pub status: String,
    /// The recorded command's exit code, if it finished.
    pub exit_code: Option<i64>,
    /// Number of `file_changes` rows.
    pub file_changes: u64,
    /// Rows with `change_kind = 'created'` (need no before-side).
    pub created: u64,
    /// Rows with `change_kind = 'modified'` (renames included).
    pub modified: u64,
    /// Rows with `change_kind = 'deleted'`.
    pub deleted: u64,
    /// Rows recorded as a rename (`renamed_from` set).
    pub renamed: u64,
    /// Modified or deleted rows whose provenance is known (non-NULL
    /// `before_source`): the denominator of before-side coverage.
    pub needs_before: u64,
    /// Of [`needs_before`](Self::needs_before), rows whose before-side is
    /// real content (`live`, `git` or `baseline`).
    pub with_before: u64,
    /// Modified or deleted rows with NULL `before_source` (written before
    /// migration 0006): their provenance is unknown and they are excluded
    /// from coverage rather than counted as misses.
    pub unknown_provenance: u64,
    /// The session's dropped-event tally.
    pub dropped: DroppedEvents,
}

impl SessionCapture {
    /// Before-side coverage in percent: the share of modified/deleted
    /// changes (with known provenance) whose before-side is real content.
    /// `None` — unknown, rendered as a dash — when there is nothing to
    /// measure; never `Some(0.0)` for "no data".
    #[must_use]
    pub fn before_side_coverage(&self) -> Option<f64> {
        if self.needs_before == 0 {
            return None;
        }
        // Counts are far below 2^52; the cast is exact in practice.
        #[allow(clippy::cast_precision_loss)]
        let pct = self.with_before as f64 * 100.0 / self.needs_before as f64;
        Some(pct)
    }
}

impl Store {
    /// Capture facts for the most recently started session, or `None` when
    /// the store holds no sessions.
    pub fn latest_session_capture(&self) -> Result<Option<SessionCapture>> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM sessions ORDER BY started_at DESC, id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.session_capture(&id).map(Some),
            None => Ok(None),
        }
    }

    /// Capture facts for session `id`. An unknown id is an error
    /// (`QueryReturnedNoRows`); callers resolve the id first.
    pub fn session_capture(&self, id: &str) -> Result<SessionCapture> {
        let (short_id, cwd, status, exit_code) = self.conn.query_row(
            "SELECT short_id, cwd, status, exit_code FROM sessions WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            },
        )?;
        let counts: [i64; 8] = self.conn.query_row(
            "SELECT
                COUNT(*),
                COALESCE(SUM(fc.change_kind = 'created'), 0),
                COALESCE(SUM(fc.change_kind = 'modified'), 0),
                COALESCE(SUM(fc.change_kind = 'deleted'), 0),
                COALESCE(SUM(fc.renamed_from IS NOT NULL), 0),
                COALESCE(SUM(fc.change_kind IN ('modified', 'deleted')
                             AND fc.before_source IS NOT NULL), 0),
                COALESCE(SUM(fc.change_kind IN ('modified', 'deleted')
                             AND fc.before_source IN ('live', 'git', 'baseline')), 0),
                COALESCE(SUM(fc.change_kind IN ('modified', 'deleted')
                             AND fc.before_source IS NULL), 0)
             FROM file_changes fc JOIN events e ON e.id = fc.event_id
             WHERE e.session_id = ?1",
            params![id],
            |r| {
                Ok([
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ])
            },
        )?;
        let [file_changes, created, modified, deleted, renamed, needs_before, with_before, unknown_provenance] =
            counts.map(|n| u64::try_from(n).unwrap_or(0));
        Ok(SessionCapture {
            session_id: id.to_string(),
            short_id,
            cwd,
            status,
            exit_code,
            file_changes,
            created,
            modified,
            deleted,
            renamed,
            needs_before,
            with_before,
            unknown_provenance,
            dropped: self.session_dropped_events(id)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        AdapterStatus, AgentKind, BeforeSource, ChangeKind, Event, EventKind, FileChange,
        FileChangeProvenance, NewSession,
    };
    use tempfile::TempDir;

    fn store() -> (TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("hh.db"), &dir.path().join("blobs")).unwrap();
        (dir, store)
    }

    fn session(store: &Store, started_at: i64) -> String {
        store
            .create_session(&NewSession {
                id: crate::event::now_v7(),
                started_at,
                agent_kind: AgentKind::Generic,
                adapter_status: AdapterStatus::None,
                command: vec!["x".into()],
                cwd: "/work".into(),
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

    fn change(
        w: &crate::store::EventWriter,
        sid: &str,
        path: &str,
        kind: ChangeKind,
        source: Option<BeforeSource>,
        renamed_from: Option<&str>,
    ) {
        let event = Event {
            session_id: sid.into(),
            ts_ms: 1,
            kind: EventKind::FileChange,
            step: None,
            summary: format!("{path} {kind}"),
            body_json: None,
            blob_hash: None,
            blob_size: None,
            correlates: None,
        };
        let fc = FileChange {
            event_id: 0,
            path: path.into(),
            change_kind: kind,
            before_hash: None,
            after_hash: None,
            is_binary: false,
        };
        match source {
            Some(s) => {
                w.append_file_change_with_provenance(
                    event,
                    fc,
                    FileChangeProvenance {
                        before_source: Some(s),
                        before_size: None,
                        renamed_from: renamed_from.map(str::to_string),
                    },
                )
                .unwrap();
            }
            None => {
                w.append_file_change(event, fc).unwrap();
            }
        }
    }

    #[test]
    fn empty_store_has_no_latest_session() {
        let (_d, store) = store();
        assert_eq!(store.latest_session_capture().unwrap(), None);
    }

    #[test]
    fn coverage_counts_only_modified_and_deleted_with_known_provenance() {
        use BeforeSource as B;
        use ChangeKind as K;
        let (_d, store) = store();
        let sid = session(&store, 10);
        let mut w = store.event_writer().unwrap();
        change(&w, &sid, "a", K::Created, Some(B::None), None);
        change(&w, &sid, "b", K::Modified, Some(B::Git), None);
        change(&w, &sid, "c", K::Deleted, Some(B::Baseline), None);
        change(&w, &sid, "d", K::Modified, Some(B::Live), Some("e"));
        change(&w, &sid, "f", K::Modified, Some(B::None), None);
        change(&w, &sid, "g", K::Modified, None, None);
        w.close().unwrap();
        let c = store.latest_session_capture().unwrap().unwrap();
        assert_eq!(c.file_changes, 6);
        assert_eq!((c.created, c.modified, c.deleted, c.renamed), (1, 4, 1, 1));
        assert_eq!(
            (c.needs_before, c.with_before, c.unknown_provenance),
            (4, 3, 1)
        );
        assert_eq!(c.before_side_coverage(), Some(75.0));
        assert_eq!(c.cwd, "/work");
    }

    /// Nothing to measure is unknown (`None`), never 0%.
    #[test]
    fn coverage_is_unknown_without_eligible_changes() {
        let (_d, store) = store();
        let sid = session(&store, 10);
        let mut w = store.event_writer().unwrap();
        change(
            &w,
            &sid,
            "a",
            ChangeKind::Created,
            Some(BeforeSource::None),
            None,
        );
        change(&w, &sid, "old", ChangeKind::Modified, None, None);
        w.close().unwrap();
        let c = store.session_capture(&sid).unwrap();
        assert_eq!(c.before_side_coverage(), None);
        assert_eq!(c.unknown_provenance, 1);
    }

    #[test]
    fn latest_session_is_the_most_recently_started() {
        let (_d, store) = store();
        let older = session(&store, 10);
        let newer = session(&store, 20);
        let mut w = store.event_writer().unwrap();
        change(
            &w,
            &older,
            "a",
            ChangeKind::Modified,
            Some(BeforeSource::Git),
            None,
        );
        w.close().unwrap();
        let c = store.latest_session_capture().unwrap().unwrap();
        assert_eq!(c.session_id, newer);
        assert_eq!(c.file_changes, 0);
    }
}
