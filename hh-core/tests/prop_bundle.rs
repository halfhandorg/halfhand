//! Property test: export → import → export round-trips a session's recorded
//! content (v1.0.0 addendum: "export→import round-trips").
//!
//! `Store::import` always mints a fresh local id, so a literal
//! byte-identical *whole-bundle* comparison is not the right invariant. This
//! test's contract: every event (kind/step/ts_ms/summary/body/file_change,
//! including blob content) round-trips exactly, and the session block
//! matches except for the identity fields (`id`/`short_id`/`imported_from`)
//! that legitimately change on import.

use hh_core::{
    bundle, AdapterStatus, AgentKind, ChangeKind, Event, EventKind, FileChange, NewSession,
    SessionStatus, Store, Uuid,
};
use proptest::prelude::*;
use std::path::PathBuf;
use tempfile::TempDir;

fn new_session(started_at: i64) -> NewSession {
    NewSession {
        id: Uuid::now_v7(),
        started_at,
        agent_kind: AgentKind::Generic,
        adapter_status: AdapterStatus::None,
        command: vec!["agent".into()],
        cwd: PathBuf::from("/tmp/proj"),
        hostname: None,
        hh_version: "test".into(),
        model: None,
        git_branch: None,
        git_sha: None,
        git_dirty: None,
    }
}

/// The event shapes this test generates: plain text events (covering every
/// text-bearing kind), a correlated tool_call/tool_result pair (covering
/// `correlates_seq` remapping — including the "shares one step" case), and a
/// file change with real blob content (covering blob carrying + hash refs).
#[derive(Debug, Clone)]
enum TestEvent {
    Text {
        kind_idx: u8,
        text: String,
    },
    ToolPair {
        call_text: String,
        result_text: String,
        is_error: bool,
    },
    FileChange {
        path: String,
        before: Option<String>,
        after: Option<String>,
    },
}

fn text_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 ._-]{0,24}"
}

fn test_event_strategy() -> impl Strategy<Value = TestEvent> {
    prop_oneof![
        (0u8..4, text_strategy()).prop_map(|(kind_idx, text)| TestEvent::Text { kind_idx, text }),
        (text_strategy(), text_strategy(), any::<bool>()).prop_map(
            |(call_text, result_text, is_error)| TestEvent::ToolPair {
                call_text,
                result_text,
                is_error,
            }
        ),
        (
            text_strategy(),
            prop::option::of(text_strategy()),
            prop::option::of(text_strategy()),
        )
            .prop_map(|(path, before, after)| TestEvent::FileChange {
                path: format!("file-{path}.txt"),
                before,
                after,
            }),
    ]
}

fn text_kind(idx: u8) -> EventKind {
    match idx % 4 {
        0 => EventKind::UserMessage,
        1 => EventKind::AgentMessage,
        2 => EventKind::Thinking,
        _ => EventKind::Error,
    }
}

fn append_text(writer: &hh_core::EventWriter, sid: &str, ts: i64, kind_idx: u8, text: &str) {
    writer
        .append_event(Event {
            session_id: sid.to_string(),
            ts_ms: ts,
            kind: text_kind(kind_idx),
            step: None,
            summary: format!("text: {text}"),
            body_json: Some(serde_json::json!({ "text": text })),
            blob_hash: None,
            blob_size: None,
            correlates: None,
        })
        .unwrap();
}

/// Appends the tool_call then its correlated tool_result; returns the `ts_ms`
/// the result was appended at (the caller's running clock).
fn append_tool_pair(
    writer: &hh_core::EventWriter,
    sid: &str,
    ts: i64,
    call_text: &str,
    result_text: &str,
    is_error: bool,
) -> i64 {
    let call_id = writer
        .append_event(Event {
            session_id: sid.to_string(),
            ts_ms: ts,
            kind: EventKind::ToolCall,
            step: None,
            summary: "tool_call: Bash".into(),
            body_json: Some(
                serde_json::json!({ "name": "Bash", "input": { "command": call_text } }),
            ),
            blob_hash: None,
            blob_size: None,
            correlates: None,
        })
        .unwrap();
    let result_ts = ts + 1;
    writer
        .append_event(Event {
            session_id: sid.to_string(),
            ts_ms: result_ts,
            kind: EventKind::ToolResult,
            step: None,
            summary: "tool_result".into(),
            body_json: Some(serde_json::json!({ "content": result_text, "is_error": is_error })),
            blob_hash: None,
            blob_size: None,
            correlates: Some(call_id),
        })
        .unwrap();
    result_ts
}

fn append_file_change(
    store: &Store,
    writer: &hh_core::EventWriter,
    sid: &str,
    ts: i64,
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
) {
    let before_hash = before.map(|c| store.blobs().put(c.as_bytes()).unwrap().hash);
    let after_out = after.map(|c| store.blobs().put(c.as_bytes()).unwrap());
    let change_kind = match (before, after) {
        (None, Some(_)) => ChangeKind::Created,
        (Some(_), None) => ChangeKind::Deleted,
        _ => ChangeKind::Modified,
    };
    writer
        .append_file_change(
            Event {
                session_id: sid.to_string(),
                ts_ms: ts,
                kind: EventKind::FileChange,
                step: None,
                summary: format!("{path} {change_kind}"),
                body_json: Some(serde_json::json!({ "path": path })),
                blob_hash: after_out.as_ref().map(|o| o.hash.clone()),
                blob_size: after_out.as_ref().map(|o| o.size),
                correlates: None,
            },
            FileChange {
                event_id: 0,
                path: path.to_string(),
                change_kind,
                before_hash,
                after_hash: after_out.map(|o| o.hash),
                is_binary: false,
            },
        )
        .unwrap();
}

/// Record `events` into a brand-new session in `store`, returning its full id.
fn record(store: &Store, events: &[TestEvent]) -> String {
    let created = store
        .create_session(&new_session(1_700_000_000_000))
        .unwrap();
    let sid = created.id.clone();
    let writer = store.event_writer().unwrap();
    let mut ts = 0i64;
    for ev in events {
        ts += 1;
        match ev {
            TestEvent::Text { kind_idx, text } => append_text(&writer, &sid, ts, *kind_idx, text),
            TestEvent::ToolPair {
                call_text,
                result_text,
                is_error,
            } => {
                ts = append_tool_pair(&writer, &sid, ts, call_text, result_text, *is_error);
            }
            TestEvent::FileChange {
                path,
                before,
                after,
            } => append_file_change(
                store,
                &writer,
                &sid,
                ts,
                path,
                before.as_deref(),
                after.as_deref(),
            ),
        }
    }
    writer.finish().unwrap();
    store.assign_steps(&sid).unwrap();
    store
        .finalize_session(&sid, ts + 1, Some(0), SessionStatus::Ok)
        .unwrap();
    sid
}

/// Strip the fields that legitimately differ across an export→import hop (a
/// fresh id/short_id, and `imported_from` set only on the re-exported copy)
/// before comparing two session blocks for equality.
fn normalize_session(mut v: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = v.as_object_mut() {
        obj.remove("id");
        obj.remove("short_id");
        obj.remove("imported_from");
    }
    v
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn export_import_export_round_trips_content(
        events in prop::collection::vec(test_event_strategy(), 0..8)
    ) {
        let source_tmp = TempDir::new().unwrap();
        let source = Store::open(&source_tmp.path().join("hh.db"), &source_tmp.path().join("blobs")).unwrap();
        let sid = record(&source, &events);

        let bytes_a = bundle::export(&source, &sid, "test", None).unwrap();
        let bundle_a = bundle::parse(&bytes_a).unwrap();

        let target_tmp = TempDir::new().unwrap();
        let target = Store::open(&target_tmp.path().join("hh.db"), &target_tmp.path().join("blobs")).unwrap();
        let created = target.import(&bundle_a).unwrap();

        let bytes_b = bundle::export(&target, &created.id, "test", None).unwrap();
        let bundle_b = bundle::parse(&bytes_b).unwrap();

        // Content round-trips exactly: `seq` is purely positional (not tied
        // to session identity), so a straight Vec equality over events is
        // the right check — and every referenced blob's bytes match too.
        prop_assert_eq!(&bundle_a.events, &bundle_b.events);
        prop_assert_eq!(&bundle_a.blobs, &bundle_b.blobs);

        // The session block matches except the identity fields that
        // legitimately differ: a fresh id/short_id, and `imported_from` now
        // recording the hop back to the original id.
        prop_assert_eq!(
            normalize_session(bundle_a.session.clone()),
            normalize_session(bundle_b.session.clone())
        );
        prop_assert_ne!(bundle_a.session["id"].clone(), bundle_b.session["id"].clone());
        prop_assert_eq!(&bundle_b.session["imported_from"], &bundle_a.session["id"]);
    }
}
