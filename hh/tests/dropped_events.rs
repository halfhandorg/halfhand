//! End-to-end tests for dropped-event accounting.
//!
//! The bug these guard: a failed event write was discarded (`let _ =`), so a
//! recorder that stopped recording printed nothing, recorded nothing, and
//! finalized the session as a clean `ok`.
//!
//! Failure is injected for real (no mocks, CLAUDE.md): after `hh` has created
//! the schema, the test installs a SQLite trigger that aborts every
//! `terminal_output` insert — the same error path a disk-full or I/O error
//! takes through the writer task. File-change events are unaffected, which
//! is how the test proves recording continues after the failures.
//!
//! No `thread::sleep`: `hh run` returns only after finalize, so the session
//! row is complete when we read it. Isolation follows `cli.rs`: temp
//! `HH_DATA_DIR`, `HOME` and `XDG_CONFIG_HOME`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use rusqlite::Connection;

const FIRST_WARNING: &str = "hh: warning: event write failed:";

struct Env {
    _root: tempfile::TempDir,
    data: PathBuf,
    work: PathBuf,
    home: PathBuf,
}

impl Env {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let (data, work, home) = (
            root.path().join("data"),
            root.path().join("work"),
            root.path().join("home"),
        );
        for d in [&data, &work, &home] {
            std::fs::create_dir_all(d).unwrap();
        }
        Self {
            _root: root,
            data,
            work,
            home,
        }
    }

    fn hh(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hh"));
        cmd.env("HH_DATA_DIR", &self.data)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.home)
            .env("NO_COLOR", "1")
            .current_dir(&self.work)
            .stdin(Stdio::null());
        cmd
    }

    /// Create the schema (any `hh` command that opens the store does).
    fn init(&self) {
        let out = self.hh().args(["list"]).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn db(&self) -> Connection {
        Connection::open(self.data.join("hh.db")).unwrap()
    }

    fn record(&self) -> Output {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dropped_events_agent.sh");
        self.hh()
            .args(["run", "--", "sh"])
            .arg(fixture)
            .output()
            .unwrap()
    }
}

fn session_counts(conn: &Connection) -> (String, String, i64, Option<String>) {
    conn.query_row(
        "SELECT short_id, status, dropped_events, dropped_events_reason FROM sessions",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

fn count_kind(conn: &Connection, kind: &str) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM events WHERE kind = ?1", [kind], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn failing_writes_warn_once_count_continue_and_mark_degraded() {
    let env = Env::new();
    env.init();
    env.db()
        .execute_batch(
            "CREATE TRIGGER hh_test_fail BEFORE INSERT ON events
             WHEN NEW.kind = 'terminal_output'
             BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
        )
        .unwrap();

    let out = env.record();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The child exited 0 and hh propagates it: dropping events is a
    // degraded recording, not a failed command.
    assert!(out.status.success(), "stderr: {stderr}");

    // Exactly one warning line for the first failure, however many fail.
    assert_eq!(
        stderr.matches(FIRST_WARNING).count(),
        1,
        "the first-failure warning must print exactly once; stderr: {stderr}"
    );
    assert!(stderr.contains("injected write failure"), "{stderr}");

    let conn = env.db();
    let (sid, status, dropped, reason) = session_counts(&conn);
    // Every terminal chunk was dropped, and every one was counted.
    assert_eq!(count_kind(&conn, "terminal_output"), 0);
    assert!(
        dropped >= 2,
        ">16 KiB of output is several chunks, all dropped; got {dropped}"
    );
    let reason = reason.expect("the first cause is persisted");
    assert!(reason.contains("injected write failure"), "{reason}");
    // Recording continued: the file written after the failures is recorded.
    assert_eq!(count_kind(&conn, "file_change"), 1);
    assert_eq!(status, "ok", "status still reflects the child's exit");

    // Never reported as a clean ok: the epilogue and the summary line.
    // stdout carries the agent's passthrough output; the epilogue is last.
    let epilogue = stdout.lines().last().unwrap();
    assert!(epilogue.starts_with("⚠ Recorded session"), "{epilogue}");
    assert!(
        epilogue.contains(&format!("· {dropped} events dropped ·")),
        "{epilogue}"
    );
    assert!(
        stderr.contains(&format!(
            "hh: warning: {dropped} events could not be written"
        )),
        "{stderr}"
    );

    // `hh list` marks the row; `--json` carries the count and cause.
    let list = env.hh().args(["list"]).output().unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    let row = list.lines().find(|l| l.starts_with(&sid)).unwrap();
    assert!(row.contains("✓ ok ⚠"), "{row}");
    let json = env.hh().args(["list", "--json"]).output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(json[0]["dropped_events"], dropped);
    assert_eq!(json[0]["dropped_events_reason"], reason.as_str());

    // `hh doctor` reports it.
    let doctor = env.hh().args(["doctor", "--json"]).output().unwrap();
    let doctor: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let check = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "dropped events")
        .expect("doctor has a dropped events check");
    assert_eq!(check["status"], "fail");
    let detail = check["detail"].as_str().unwrap();
    assert!(detail.contains(&format!("{sid}: {dropped}")), "{detail}");
}

#[test]
fn clean_session_has_no_warning_and_no_marker() {
    let env = Env::new();
    let out = env.record();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(!stderr.contains(FIRST_WARNING), "{stderr}");
    assert!(!stderr.contains("could not be written"), "{stderr}");
    let epilogue = stdout.lines().last().unwrap();
    assert!(epilogue.starts_with("✓ Recorded session"), "{epilogue}");
    assert!(!epilogue.contains("dropped"), "{epilogue}");

    let conn = env.db();
    let (sid, status, dropped, reason) = session_counts(&conn);
    assert_eq!((status.as_str(), dropped, reason), ("ok", 0, None));
    assert!(count_kind(&conn, "terminal_output") >= 1);

    let list = env.hh().args(["list"]).output().unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    let row = list.lines().find(|l| l.starts_with(&sid)).unwrap();
    assert!(!row.contains('⚠'), "{row}");

    let doctor = env.hh().args(["doctor", "--json"]).output().unwrap();
    let doctor: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let check = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "dropped events")
        .unwrap();
    assert_eq!(check["status"], "ok");
}
