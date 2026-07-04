//! End-to-end CLI tests for the `hh` binary (CLAUDE.md testing standards:
//! run `hh` against the real binary, never the real data dir — set
//! `HH_DATA_DIR`).
//!
//! These cover `--version`/`--help` (FR-6.2), the "not implemented" path for
//! subcommands still on the roadmap, and a full `hh run` end-to-end against a
//! fixture script (SRS acceptance #2).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::Connection;

/// Build a `Command` that runs the compiled `hh` binary. Cargo sets
/// `CARGO_BIN_EXE_hh` to its path for integration tests of the `hh` bin crate.
fn hh() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hh"))
}

/// Absolute path to a fixture under `tests/fixtures/`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn version_prints_pkg_version() {
    let out = hh().arg("--version").output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("0.1.0-beta.1"),
        "expected package version in: {stdout}"
    );
    // NFR-8: git sha is embedded in parentheses.
    assert!(stdout.contains('('), "expected git sha in: {stdout}");
}

#[test]
fn help_lists_every_subcommand_and_examples() {
    let out = hh().arg("--help").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for sub in ["run", "replay", "inspect", "list", "delete", "mcp-proxy"] {
        assert!(stdout.contains(sub), "missing `{sub}` in --help: {stdout}");
    }
    assert!(
        stdout.contains("Examples:"),
        "missing Examples section: {stdout}"
    );
}

#[test]
fn every_subcommand_help_has_an_example() {
    for sub in ["run", "replay", "inspect", "list", "delete", "mcp-proxy"] {
        let out = hh().args([sub, "--help"]).output().unwrap();
        assert!(out.status.success(), "`hh {sub} --help` failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("Example"),
            "`hh {sub} --help` has no example: {stdout}"
        );
    }
}

#[test]
fn unimplemented_subcommand_returns_not_implemented_exiting_nonzero() {
    // `hh run` is implemented; use a still-unimplemented subcommand here.
    let out = hh().args(["replay", "last"]).output().unwrap();
    assert!(
        !out.status.success(),
        "expected nonzero exit for unimplemented replay"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not implemented"),
        "expected 'not implemented' in stderr: {stderr}"
    );
    // NFR-7: error includes a hint (suggested next step).
    assert!(
        stderr.contains("hint:"),
        "missing actionable hint: {stderr}"
    );
}

/// Open the `hh.db` at `data_dir` for assertions (DR-2: the schema is a public
/// interface; we query it directly rather than through hh-core's Store).
fn open_db(data_dir: &Path) -> Connection {
    Connection::open(data_dir.join("hh.db")).expect("hh.db should exist after a recording")
}

/// Synchronously run a temp data dir + work dir, returning both. Cleans up on
/// drop. Tests never touch the real data dir (CLAUDE.md).
struct Temp {
    data: tempfile::TempDir,
    work: tempfile::TempDir,
}

impl Temp {
    fn new() -> Self {
        Self {
            data: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        }
    }
}

/// Run `hh run -- <fixture>` in `work` with `HH_DATA_DIR=data`, returning the
/// captured output. stdin is /dev/null so the stdin proxy exits immediately
/// and the test never blocks on a TTY read.
fn run_fixture(temp: &Temp, args: &[&str]) -> std::process::Output {
    hh().args(["run", "--"])
        .args(args)
        .env("HH_DATA_DIR", temp.data.path())
        .current_dir(temp.work.path())
        .stdin(Stdio::null())
        .output()
        .expect("hh run should execute")
}

#[test]
fn run_records_generic_session_with_terminal_and_file_changes() {
    let temp = Temp::new();
    let fx = fixture("fixture_agent.sh").to_string_lossy().to_string();
    let out = run_fixture(&temp, &["sh", &fx]);

    // The fixture exits 3; hh propagates the child exit code (FR-1.6).
    assert_eq!(
        out.status.code(),
        Some(3),
        "hh run should propagate the child's nonzero exit code; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Epilogue on stdout (FR-1.6): "✓ Recorded session <id> · ... · hh replay <id>".
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("✓ Recorded session"),
        "missing epilogue: {stdout}"
    );
    assert!(
        stdout.contains("files changed"),
        "epilogue should mention files changed: {stdout}"
    );

    let conn = open_db(temp.data.path());

    // Exactly one session, generic agent, errored, exit code 3, cwd = work dir.
    let (agent_kind, status, exit_code, cwd): (String, String, i64, String) = conn
        .query_row(
            "SELECT agent_kind, status, exit_code, cwd FROM sessions ORDER BY started_at DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(agent_kind, "generic");
    assert_eq!(status, "error");
    assert_eq!(exit_code, 3);

    // Normalize canonical paths before comparing to handle macOS /var -> /private/var differences.
    let recorded_cwd_path = std::path::Path::new(&cwd)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(&cwd));
    let expected_cwd_path = temp
        .work
        .path()
        .canonicalize()
        .unwrap_or_else(|_| temp.work.path().to_path_buf());
    assert_eq!(
        recorded_cwd_path, expected_cwd_path,
        "cwd mismatch: recorded {recorded_cwd_path:?} vs expected {expected_cwd_path:?}",
    );

    // Terminal output was captured (FR-1.3): at least one terminal_output
    // event whose body includes the fixture's banner.
    let terminal_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'terminal_output'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(terminal_count >= 1, "expected terminal_output events");

    // File changes were captured (FR-1.4): fixture_output.txt with an after_hash.
    let (path, change_kind, after_hash): (String, String, Option<String>) = conn
        .query_row(
            "SELECT path, change_kind, after_hash FROM file_changes
             WHERE path = 'fixture_output.txt' ORDER BY event_id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(path, "fixture_output.txt");
    assert!(
        change_kind == "created" || change_kind == "modified",
        "unexpected change_kind: {change_kind}"
    );
    let after_hash = after_hash.expect("text file should have an after_hash");
    assert!(!after_hash.is_empty());

    // The after_hash references a real blob row (content was stored).
    let blob_refcount: i64 = conn
        .query_row(
            "SELECT refcount FROM blobs WHERE hash = ?1",
            [&after_hash],
            |r| r.get(0),
        )
        .unwrap_or(0);
    assert!(blob_refcount >= 1, "blob should be referenced");
}

#[test]
fn run_marks_stale_recording_sessions_interrupted_on_next_open() {
    // FR-1.7: a session left in 'recording' (e.g. hh crashed) must be marked
    // 'interrupted' the next time hh opens the store. We simulate a crashed
    // session by inserting a 'recording' row with no ended_at directly, then
    // run a fresh `hh run` and assert the stale row flipped to 'interrupted'.
    let temp = Temp::new();

    // First, open the store once so the schema exists, then insert a stale row.
    {
        let conn = open_db_after_init(temp.data.path());
        conn.execute(
            "INSERT INTO sessions (id, short_id, started_at, status, agent_kind,
                                    adapter_status, command, cwd, hh_version)
             VALUES ('stale-full-id-0', 'stale0', 1234, 'recording', 'generic',
                     'none', '[]', '/tmp', 'test')",
            [],
        )
        .unwrap();
    }

    // Run a real (very short) session to trigger mark_stale_interrupted on open.
    let out = run_fixture(&temp, &["true"]);
    assert!(out.status.success(), "true should exit 0");

    let conn = open_db(temp.data.path());
    let stale_status: String = conn
        .query_row(
            "SELECT status FROM sessions WHERE id = 'stale-full-id-0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stale_status, "interrupted",
        "stale recording session should be marked interrupted on next open"
    );
}

/// Ensure the DB exists (run a trivial `hh run` and tear it down) — actually we
/// just open via hh-core-free rusqlite after ensuring the file exists. For the
/// stale test we need the schema applied; the cleanest way is to run an
/// empty `hh run` first, but we don't want its session row. Instead, open the
/// store through the binary by running `hh run -- true` once, then reset.
fn open_db_after_init(data_dir: &Path) -> Connection {
    // Apply migrations by running a throwaway recording, then delete its row
    // so it does not pollute the stale test. The schema persists.
    let temp_work = tempfile::tempdir().unwrap();
    hh().args(["run", "--", "true"])
        .env("HH_DATA_DIR", data_dir)
        .current_dir(temp_work.path())
        .stdin(Stdio::null())
        .output()
        .expect("hh run should init the schema");
    let conn = Connection::open(data_dir.join("hh.db")).unwrap();
    // Clear any sessions the init run created so the stale test is clean.
    conn.execute("DELETE FROM sessions WHERE id != 'stale-full-id-0'", [])
        .ok();
    conn
}
