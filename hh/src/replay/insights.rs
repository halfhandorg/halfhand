//! Session-wide facts the replay TUI computes once, before the first frame:
//! the files the session touched, which tool calls failed, the largest diff,
//! and the estimated cost for the header.
//!
//! Everything here is bounded by the number of *file changes* and *failed
//! results*, never by the number of events: the queries return small rows
//! (no bodies), and the largest-diff search reads at most
//! [`LARGEST_DIFF_CANDIDATES`] blob pairs of at most [`LARGEST_DIFF_MAX_BYTES`]
//! each. Loading is best-effort: a failed query leaves its part empty (the
//! header shows a dash) and records why in [`Insights::load_error`].

use super::diff::line_stats;
use hh_core::{ChangeKind, FileChangeEntry, SessionRow, Store};
use std::collections::{HashMap, HashSet};

/// How many of the largest file changes (by stored bytes) are diffed exactly
/// when looking for the largest diff.
const LARGEST_DIFF_CANDIDATES: usize = 32;

/// Blobs larger than this are not read for the largest-diff search.
const LARGEST_DIFF_MAX_BYTES: u64 = 1024 * 1024;

/// Every change the session made to one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileGroup {
    /// The path, relative to the session cwd.
    pub path: String,
    /// The `file_change` event ids, in recording order.
    pub event_ids: Vec<i64>,
    /// The path this file was renamed from, if a change renamed it.
    pub renamed_from: Option<String>,
    /// Whether the first change created the file.
    pub created: bool,
    /// Whether the last change deleted the file.
    pub deleted: bool,
    /// Whether the file was binary.
    pub binary: bool,
}

/// The file change with the most added + removed lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargestDiff {
    /// Its `file_change` event id.
    pub event_id: i64,
    /// Its path.
    pub path: String,
    /// Lines added.
    pub added: usize,
    /// Lines removed.
    pub removed: usize,
}

/// Precomputed session-wide facts (see the module docs).
#[derive(Debug, Clone, Default)]
pub struct Insights {
    /// List-price cost estimate, nano-USD; `None` when unknown.
    pub cost_nano_usd: Option<u128>,
    /// Files touched, in first-touch order.
    pub files: Vec<FileGroup>,
    /// `tool_result` event ids whose body marks the call as failed.
    pub failed_tool_results: HashSet<i64>,
    /// The largest diff, when any text change has both sides readable.
    pub largest_diff: Option<LargestDiff>,
    /// Why part of the load failed, if it did.
    pub load_error: Option<String>,
}

impl Insights {
    /// Load the insights for `session`. Never fails; see the module docs.
    pub fn load(store: &Store, session: &SessionRow) -> Self {
        let mut out = Self::default();
        let mut errors = Vec::new();
        match store.session_context(&session.id) {
            Ok(ctx) => out.cost_nano_usd = ctx.cost_nano_usd,
            Err(e) => errors.push(format!("cost: {e}")),
        }
        match store.failed_tool_result_ids(&session.id) {
            Ok(ids) => out.failed_tool_results = ids.into_iter().collect(),
            Err(e) => errors.push(format!("failed tool calls: {e}")),
        }
        match store.list_session_file_changes(&session.id) {
            Ok(changes) => {
                out.files = group_files(&changes);
                out.largest_diff = largest_diff(store, &changes);
            }
            Err(e) => errors.push(format!("file changes: {e}")),
        }
        if !errors.is_empty() {
            out.load_error = Some(format!(
                "could not load {} — run `hh doctor` to check the database",
                errors.join("; ")
            ));
        }
        out
    }
}

/// Group file changes by path, in first-touch order.
#[must_use]
pub fn group_files(changes: &[FileChangeEntry]) -> Vec<FileGroup> {
    let mut groups: Vec<FileGroup> = Vec::new();
    let mut by_path: HashMap<&str, usize> = HashMap::new();
    for c in changes {
        let idx = *by_path.entry(c.path.as_str()).or_insert_with(|| {
            groups.push(FileGroup {
                path: c.path.clone(),
                event_ids: Vec::new(),
                renamed_from: None,
                created: c.change_kind == ChangeKind::Created,
                deleted: false,
                binary: false,
            });
            groups.len() - 1
        });
        let g = &mut groups[idx];
        g.event_ids.push(c.event_id);
        if c.renamed_from.is_some() {
            g.renamed_from.clone_from(&c.renamed_from);
        }
        g.deleted = c.change_kind == ChangeKind::Deleted;
        g.binary |= c.is_binary;
    }
    groups
}

/// Find the change with the most changed lines among the largest
/// candidates by stored size (see the module docs for the bounds). Changes
/// with an unknown before-side are skipped: their line counts are unknown.
fn largest_diff(store: &Store, changes: &[FileChangeEntry]) -> Option<LargestDiff> {
    let mut candidates: Vec<&FileChangeEntry> = changes.iter().filter(|c| !c.is_binary).collect();
    candidates
        .sort_by_key(|c| std::cmp::Reverse(c.before_size.unwrap_or(0) + c.after_size.unwrap_or(0)));
    let read = |hash: Option<&str>, size: Option<u64>| -> Option<String> {
        if size.is_some_and(|s| s > LARGEST_DIFF_MAX_BYTES) {
            return None;
        }
        let bytes = store.blobs().get(hash?).ok()?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    };
    let mut best: Option<LargestDiff> = None;
    for c in candidates.into_iter().take(LARGEST_DIFF_CANDIDATES) {
        let before = read(c.before_hash.as_deref(), c.before_size);
        let after = read(c.after_hash.as_deref(), c.after_size);
        let (added, removed) = match (c.change_kind, before.as_deref(), after.as_deref()) {
            (_, Some(b), Some(a)) => line_stats(b, a),
            (ChangeKind::Created, None, Some(a)) => line_stats("", a),
            (ChangeKind::Deleted, Some(b), None) => line_stats(b, ""),
            _ => continue,
        };
        if added + removed == 0 {
            continue;
        }
        let better = best.as_ref().is_none_or(|b| {
            let (n, o) = (added + removed, b.added + b.removed);
            n > o || (n == o && c.event_id < b.event_id)
        });
        if better {
            best = Some(LargestDiff {
                event_id: c.event_id,
                path: c.path.clone(),
                added,
                removed,
            });
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, path: &str, kind: ChangeKind) -> FileChangeEntry {
        // `FileChangeEntry` is `#[non_exhaustive]`; build one through the
        // store in integration-style tests, or through this helper which
        // round-trips a real row.
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let sid = store
            .create_session(&hh_core::NewSession {
                id: hh_core::event::now_v7(),
                started_at: 0,
                agent_kind: hh_core::AgentKind::Generic,
                adapter_status: hh_core::AdapterStatus::None,
                command: vec!["sh".into()],
                cwd: "/tmp".into(),
                hostname: None,
                hh_version: "t".into(),
                model: None,
                git_branch: None,
                git_sha: None,
                git_dirty: None,
            })
            .unwrap()
            .id;
        let writer = store.event_writer().unwrap();
        writer
            .append_file_change(
                hh_core::Event {
                    session_id: sid.clone(),
                    ts_ms: id,
                    kind: hh_core::EventKind::FileChange,
                    step: None,
                    summary: "x".into(),
                    body_json: None,
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                },
                hh_core::FileChange {
                    event_id: 0,
                    path: path.into(),
                    change_kind: kind,
                    before_hash: None,
                    after_hash: None,
                    is_binary: false,
                },
            )
            .unwrap();
        writer.finish().unwrap();
        let mut row = store.list_session_file_changes(&sid).unwrap().remove(0);
        row.event_id = id;
        row
    }

    #[test]
    fn groups_by_path_in_first_touch_order() {
        let changes = vec![
            entry(1, "b.rs", ChangeKind::Created),
            entry(2, "a.rs", ChangeKind::Modified),
            entry(3, "b.rs", ChangeKind::Modified),
            entry(4, "a.rs", ChangeKind::Deleted),
        ];
        let groups = group_files(&changes);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].path, "b.rs");
        assert_eq!(groups[0].event_ids, vec![1, 3]);
        assert!(groups[0].created);
        assert!(!groups[0].deleted);
        assert_eq!(groups[1].event_ids, vec![2, 4]);
        assert!(groups[1].deleted);
    }
}
