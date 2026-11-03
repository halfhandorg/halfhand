//! Read-only queries the replay TUI needs beyond the per-event index:
//! every file change a session made (with sizes and provenance, but no blob
//! content), and which tool results the agent reported as failed.
//!
//! Both are single indexed queries over one session, returning small rows
//! (no bodies, no blob content), so a session with tens of thousands of
//! events costs one pass of SQLite and a few bytes per matching row.

use rusqlite::params;

use super::Store;
use crate::error::Result;
use crate::event::{BeforeSource, ChangeKind};

/// One `file_changes` row of a session, without its content: what the
/// replay TUI's files-touched view and "largest diff" moment need.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileChangeEntry {
    /// The `file_change` event this row belongs to.
    pub event_id: i64,
    /// Event timestamp, ms since session start.
    pub ts_ms: i64,
    /// Path relative to the session cwd (the new path, for a rename).
    pub path: String,
    /// Created, modified or deleted. A rename is `Modified` with
    /// [`Self::renamed_from`] set.
    pub change_kind: ChangeKind,
    /// For a rename, the old path (migration 0006).
    pub renamed_from: Option<String>,
    /// Where the before-side came from (migration 0006). `None` for rows
    /// written before that migration.
    pub before_source: Option<BeforeSource>,
    /// Content hash of the before-side blob, if any.
    pub before_hash: Option<String>,
    /// Content hash of the after-side blob, if any.
    pub after_hash: Option<String>,
    /// Uncompressed size of the before-side blob, when one is stored.
    pub before_size: Option<u64>,
    /// Uncompressed size of the after-side blob, when one is stored.
    pub after_size: Option<u64>,
    /// Whether the file was detected as binary.
    pub is_binary: bool,
}

impl Store {
    /// Every file change of a session in recording order (event id), with
    /// blob sizes and provenance but no content. Used by `hh replay`'s
    /// files-touched view. A malformed `change_kind` or `before_source`
    /// reads as `modified` / unknown rather than failing the whole list.
    pub fn list_session_file_changes(&self, session_id: &str) -> Result<Vec<FileChangeEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT fc.event_id, e.ts_ms, fc.path, fc.change_kind, fc.renamed_from,
                    fc.before_source, bb.size, ab.size, fc.is_binary,
                    fc.before_hash, fc.after_hash
             FROM file_changes fc
             JOIN events e ON e.id = fc.event_id
             LEFT JOIN blobs bb ON bb.hash = fc.before_hash
             LEFT JOIN blobs ab ON ab.hash = fc.after_hash
             WHERE e.session_id = ?1
             ORDER BY fc.event_id",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            let kind: String = r.get(3)?;
            let source: Option<String> = r.get(5)?;
            let size = |v: Option<i64>| v.and_then(|n| u64::try_from(n).ok());
            Ok(FileChangeEntry {
                event_id: r.get(0)?,
                ts_ms: r.get(1)?,
                path: r.get(2)?,
                change_kind: kind.parse().unwrap_or(ChangeKind::Modified),
                renamed_from: r.get(4)?,
                before_source: source.and_then(|s| s.parse().ok()),
                before_size: size(r.get(6)?),
                after_size: size(r.get(7)?),
                is_binary: r.get::<_, i64>(8)? != 0,
                before_hash: r.get(9)?,
                after_hash: r.get(10)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Ids of the session's `tool_result` events whose body marks the call
    /// as failed (`"is_error": true`, as the Claude Code adapter writes it),
    /// in id order. `json_valid` guards `json_extract`, so a malformed body
    /// is skipped rather than failing the query.
    pub fn failed_tool_result_ids(&self, session_id: &str) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM events
             WHERE session_id = ?1 AND kind = 'tool_result' AND body_json IS NOT NULL
               AND json_valid(body_json)
               AND json_extract(body_json, '$.is_error') IN (1, 'true')
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session_id], |r| r.get::<_, i64>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        AdapterStatus, AgentKind, Event, EventKind, FileChange, FileChangeProvenance, NewSession,
    };
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn open() -> (TempDir, Store, String) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let created = store
            .create_session(&NewSession {
                id: crate::event::now_v7(),
                started_at: 0,
                agent_kind: AgentKind::Generic,
                adapter_status: AdapterStatus::None,
                command: vec!["sh".into()],
                cwd: PathBuf::from("/tmp"),
                hostname: None,
                hh_version: "test".into(),
                model: None,
                git_branch: None,
                git_sha: None,
                git_dirty: None,
            })
            .unwrap();
        (tmp, store, created.id)
    }

    fn ev(sid: &str, ts: i64, kind: EventKind, body: Option<serde_json::Value>) -> Event {
        Event {
            session_id: sid.into(),
            ts_ms: ts,
            kind,
            step: None,
            summary: "s".into(),
            body_json: body,
            blob_hash: None,
            blob_size: None,
            correlates: None,
        }
    }

    #[test]
    fn file_changes_carry_sizes_rename_and_provenance() {
        let (_tmp, store, sid) = open();
        let before = store.blobs().put(b"old\n").unwrap().hash;
        let after = store.blobs().put(b"new content\n").unwrap();
        let writer = store.event_writer().unwrap();
        // The watcher records the after-side as the event's own blob.
        let mut event = ev(&sid, 10, EventKind::FileChange, None);
        event.blob_hash = Some(after.hash.clone());
        event.blob_size = Some(after.size);
        writer
            .append_file_change_with_provenance(
                event,
                FileChange {
                    event_id: 0,
                    path: "b.txt".into(),
                    change_kind: ChangeKind::Modified,
                    before_hash: Some(before),
                    after_hash: Some(after.hash),
                    is_binary: false,
                },
                FileChangeProvenance::new(BeforeSource::Git)
                    .with_before_size(Some(4))
                    .with_renamed_from(Some("a.txt".into())),
            )
            .unwrap();
        writer
            .append_file_change(
                ev(&sid, 20, EventKind::FileChange, None),
                FileChange {
                    event_id: 0,
                    path: "c.txt".into(),
                    change_kind: ChangeKind::Modified,
                    before_hash: None,
                    after_hash: None,
                    is_binary: false,
                },
            )
            .unwrap();
        writer.finish().unwrap();

        let list = store.list_session_file_changes(&sid).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].path, "b.txt");
        assert_eq!(list[0].renamed_from.as_deref(), Some("a.txt"));
        assert_eq!(list[0].before_source, Some(BeforeSource::Git));
        assert_eq!(list[0].before_size, Some(4));
        assert_eq!(list[0].after_size, Some(12));
        assert_eq!(list[1].before_size, None, "unknown size is None, not 0");
        assert_eq!(list[1].before_source, None);
    }

    #[test]
    fn failed_tool_results_match_is_error_only() {
        let (_tmp, store, sid) = open();
        let writer = store.event_writer().unwrap();
        let ok = writer
            .append_event(ev(
                &sid,
                1,
                EventKind::ToolResult,
                Some(serde_json::json!({"is_error": false})),
            ))
            .unwrap();
        let bad = writer
            .append_event(ev(
                &sid,
                2,
                EventKind::ToolResult,
                Some(serde_json::json!({"is_error": true, "content": "boom"})),
            ))
            .unwrap();
        writer
            .append_event(ev(&sid, 3, EventKind::ToolResult, None))
            .unwrap();
        writer.finish().unwrap();
        let failed = store.failed_tool_result_ids(&sid).unwrap();
        assert_eq!(failed, vec![bad]);
        assert!(!failed.contains(&ok));
    }
}
