//! Capture-fidelity harness: does a recorded session contain what the agent
//! actually did?
//!
//! ~380 tests passed while several capture bugs shipped (a one-line edit of
//! a pre-existing file recorded as the whole file added, a delete with no
//! content, a rename recorded as an unrelated delete + create). None of them
//! compared a recording against ground truth. This file does.
//!
//! `tests/fixtures/fidelity_agent.sh` performs a KNOWN set of effects —
//! create two files, edit one line of a pre-existing file, rename a file,
//! delete a file, run a command with known output, exit 3. Each test drives
//! the real `hh run` over it and asserts the database row by row against that
//! list, in a work dir that is a git repository and in one that is not.
//! Blob contents are read back from the real blob store; SQLite is a real
//! database in a temp dir (never the real data dir: `HH_DATA_DIR`).
//!
//! No `thread::sleep`: `hh run` returns only after the watcher's shutdown
//! flush and reconciliation backstop, so the recording is complete when we
//! read it. No network: everything is local files and a local `sh`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::Connection;

/// The fixture's exit code.
const FIXTURE_EXIT: i32 = 3;

/// The 20-line pre-existing file whose line 7 the fixture edits.
fn twenty_lines() -> String {
    (1..=20).fold(String::new(), |mut s, i| {
        let _ = writeln!(s, "line {i}");
        s
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Repo {
    /// A git repository with everything committed.
    Git,
    /// Not a git repository at all.
    NoGit,
}

impl Repo {
    /// Where pre-existing files' before-sides are expected to come from.
    fn before_source(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::NoGit => "baseline",
        }
    }
}

struct Scenario {
    _root: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    work: PathBuf,
    home: PathBuf,
}

impl Scenario {
    fn new(repo: Repo) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize: on macOS the temp dir is behind the /var symlink.
        let root = tmp.path().canonicalize().unwrap();
        let (data, work, home) = (root.join("data"), root.join("work"), root.join("home"));
        for d in [&data, &work, &home] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(work.join("keep.txt"), twenty_lines()).unwrap();
        std::fs::write(work.join("old.txt"), "moving content\n").unwrap();
        std::fs::write(work.join("del.txt"), "doomed line one\ndoomed line two\n").unwrap();
        let s = Self {
            _root: tmp,
            root,
            data,
            work,
            home,
        };
        if repo == Repo::Git {
            s.git(&["init", "-q"]);
            s.git(&["add", "-A"]);
            s.git(&["commit", "-q", "-m", "initial"]);
        }
        s
    }

    fn git(&self, args: &[&str]) {
        let st = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
            .args(args)
            .current_dir(&self.work)
            .env("GIT_CEILING_DIRECTORIES", &self.root)
            .stdout(Stdio::null())
            .status()
            .expect("git must be installed to run these tests");
        assert!(st.success(), "git {args:?} failed");
    }

    fn hh(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hh"));
        cmd.env("HH_DATA_DIR", &self.data)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.home)
            .env("GIT_CEILING_DIRECTORIES", &self.root)
            .env("NO_COLOR", "1")
            .current_dir(&self.work)
            .stdin(Stdio::null());
        cmd
    }

    /// Record the fixture; return the session id.
    fn record(&self) -> String {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fidelity_agent.sh");
        let out = self
            .hh()
            .args(["run", "--", "sh"])
            .arg(fixture)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(FIXTURE_EXIT),
            "hh run must propagate the fixture's exit code; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.db()
            .query_row("SELECT id FROM sessions", [], |r| r.get(0))
            .unwrap()
    }

    fn db(&self) -> Connection {
        Connection::open(self.data.join("hh.db")).unwrap()
    }

    fn blob(&self, hash: &str) -> String {
        let bytes = hh_core::BlobStore::new(self.data.join("blobs"))
            .get(hash)
            .unwrap_or_else(|e| panic!("blob {hash} not readable: {e}"));
        String::from_utf8(bytes).unwrap()
    }

    /// Every `file_changes` row of the session, sorted by path.
    fn changes(&self, session: &str) -> Vec<Change> {
        let conn = self.db();
        let mut stmt = conn
            .prepare(
                "SELECT fc.path, fc.change_kind, fc.before_source, fc.renamed_from,
                        fc.before_hash, fc.after_hash
                 FROM file_changes fc JOIN events e ON e.id = fc.event_id
                 WHERE e.session_id = ?1 ORDER BY fc.path",
            )
            .unwrap();
        stmt.query_map([session], |r| {
            Ok(Change {
                path: r.get(0)?,
                kind: r.get(1)?,
                before_source: r.get(2)?,
                renamed_from: r.get(3)?,
                before_hash: r.get(4)?,
                after_hash: r.get(5)?,
            })
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    /// All terminal output of the session, reassembled in order.
    fn terminal(&self, session: &str) -> String {
        let conn = self.db();
        let mut stmt = conn
            .prepare(
                "SELECT body_json, blob_hash FROM events
                 WHERE session_id = ?1 AND kind = 'terminal_output' ORDER BY ts_ms, id",
            )
            .unwrap();
        let rows: Vec<(Option<String>, Option<String>)> = stmt
            .query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut out = String::new();
        for (body, blob) in rows {
            let text = body
                .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string));
            match (text, blob) {
                (Some(t), _) => out.push_str(&t),
                (None, Some(h)) => out.push_str(&self.blob(&h)),
                (None, None) => {}
            }
        }
        out
    }
}

#[derive(Debug)]
struct Change {
    path: String,
    kind: String,
    before_source: Option<String>,
    renamed_from: Option<String>,
    before_hash: Option<String>,
    after_hash: Option<String>,
}

fn find<'a>(changes: &'a [Change], path: &str) -> &'a Change {
    changes
        .iter()
        .find(|c| c.path == path)
        .unwrap_or_else(|| panic!("no file_changes row for {path}: {changes:#?}"))
}

/// The full ground-truth check, shared by both repository variants.
fn assert_matches_ground_truth(repo: Repo) {
    let s = Scenario::new(repo);
    let id = s.record();
    assert_file_effects(&s, &id, repo);
    assert_command_and_failure(&s, &id, repo);
    assert_doctor_agrees(&s, &id, repo);
}

/// Every file effect is recorded, with real before-sides.
fn assert_file_effects(s: &Scenario, id: &str, repo: Repo) {
    let changes = s.changes(id);
    let expected_source = repo.before_source();

    // Every effect is present, and nothing else: exactly these five paths.
    let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        paths,
        ["del.txt", "keep.txt", "made.txt", "made2.txt", "new.txt"],
        "{repo:?}: recorded paths must equal the fixture's effects: {changes:#?}"
    );

    // Two creates, with their exact content and no before-side.
    for (path, content) in [
        ("made.txt", "first created file\n"),
        ("made2.txt", "second created file\nwith two lines\n"),
    ] {
        let c = find(&changes, path);
        assert_eq!(c.kind, "created", "{repo:?} {path}");
        assert_eq!(
            c.before_hash, None,
            "{repo:?} {path}: a create has no before"
        );
        let after = c.after_hash.as_deref().expect("created file has content");
        assert_eq!(s.blob(after), content, "{repo:?} {path}");
    }

    // The one-line edit: a real before-side (the pre-session content), and
    // the after-side differs from it by exactly that line.
    let keep = find(&changes, "keep.txt");
    assert_eq!(keep.kind, "modified", "{repo:?}");
    assert_eq!(
        keep.before_source.as_deref(),
        Some(expected_source),
        "{repo:?}"
    );
    let before = s.blob(keep.before_hash.as_deref().expect("edit has a before-side"));
    let after = s.blob(keep.after_hash.as_deref().expect("edit has an after-side"));
    assert_eq!(
        before,
        twenty_lines(),
        "{repo:?}: before-side is the real old file"
    );
    assert_eq!(
        after,
        twenty_lines().replace("line 7\n", "line SEVEN\n"),
        "{repo:?}"
    );
    let differing = before
        .lines()
        .zip(after.lines())
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(differing, 1, "{repo:?}: exactly one line changed");

    // The rename: one row on the new path naming the old one, with the
    // moved content on both sides; no separate delete of the old path.
    let ren = find(&changes, "new.txt");
    assert_eq!(
        ren.kind, "modified",
        "{repo:?}: a rename is one modified row"
    );
    assert_eq!(ren.renamed_from.as_deref(), Some("old.txt"), "{repo:?}");
    assert_eq!(
        ren.before_source.as_deref(),
        Some(expected_source),
        "{repo:?}"
    );
    assert_eq!(
        s.blob(
            ren.before_hash
                .as_deref()
                .expect("rename has a before-side")
        ),
        "moving content\n"
    );
    assert_eq!(
        s.blob(ren.after_hash.as_deref().expect("rename has an after-side")),
        "moving content\n"
    );

    // The delete keeps its content as the before-side.
    let del = find(&changes, "del.txt");
    assert_eq!(del.kind, "deleted", "{repo:?}");
    assert_eq!(
        del.before_source.as_deref(),
        Some(expected_source),
        "{repo:?}"
    );
    assert_eq!(del.after_hash, None, "{repo:?}");
    assert_eq!(
        s.blob(
            del.before_hash
                .as_deref()
                .expect("delete keeps its content")
        ),
        "doomed line one\ndoomed line two\n",
        "{repo:?}"
    );
}

/// The command's output and the non-zero exit are recorded.
fn assert_command_and_failure(s: &Scenario, id: &str, repo: Repo) {
    // The command ran and its output was captured.
    let term = s.terminal(id);
    assert!(
        term.contains("fidelity-command-output lines=20"),
        "{repo:?}: command output missing from terminal capture: {term:?}"
    );

    // The failure is recorded.
    let (status, exit_code): (String, i64) = s
        .db()
        .query_row(
            "SELECT status, exit_code FROM sessions WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "error", "{repo:?}");
    assert_eq!(exit_code, i64::from(FIXTURE_EXIT), "{repo:?}");
}

/// `hh doctor --capture --json` agrees: before-side coverage >= 95%.
fn assert_doctor_agrees(s: &Scenario, id: &str, repo: Repo) {
    let report = doctor_capture_json(s);
    let capture = &report["capture"];
    assert_eq!(capture["session"]["id"], id);
    assert_eq!(capture["file_changes"]["total"], 5);
    assert_eq!(capture["file_changes"]["renamed"], 1);
    assert_eq!(capture["dropped_events"], 0);
    let pct = capture["before_side_coverage"]["percent"]
        .as_f64()
        .expect("coverage is known for this fixture");
    assert!(pct >= 95.0, "{repo:?}: before-side coverage {pct}% < 95%");
}

fn doctor_capture_json(s: &Scenario) -> serde_json::Value {
    let out = s
        .hh()
        .args(["doctor", "--capture", "--json"])
        .output()
        .unwrap();
    // Exit 1 is expected here: this sandbox has no Claude transcript, so that
    // unrelated check fails. `--capture` itself never changes the exit code.
    assert!(
        matches!(out.status.code(), Some(0 | 1)),
        "hh doctor --capture --json: {:?} {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("doctor --json prints one JSON object")
}

#[test]
fn git_repo_recording_matches_ground_truth() {
    assert_matches_ground_truth(Repo::Git);
}

#[test]
fn non_git_recording_matches_ground_truth() {
    assert_matches_ground_truth(Repo::NoGit);
}

/// The JSON shape of `hh doctor --capture --json`: every existing doctor
/// field is still there, and the additive `capture` object carries every
/// documented key, with unknown facts as `null`.
#[test]
fn doctor_capture_json_shape() {
    let s = Scenario::new(Repo::NoGit);
    s.record();
    let v = doctor_capture_json(&s);
    for key in ["schema", "status", "checks"] {
        assert!(
            v.get(key).is_some(),
            "existing doctor field `{key}` missing"
        );
    }
    for check in v["checks"].as_array().unwrap() {
        for key in ["name", "status", "detail"] {
            assert!(check.get(key).is_some(), "check field `{key}` missing");
        }
    }
    let c = &v["capture"];
    let keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
    let mut expected = vec![
        "before_side_coverage",
        "dropped_events",
        "dropped_events_reason",
        "events_live",
        "events_reconciled",
        "file_changes",
        "permission_denied",
        "platform",
        "session",
        "watched_roots",
    ];
    expected.sort_unstable();
    let mut keys_sorted = keys.clone();
    keys_sorted.sort_unstable();
    assert_eq!(keys_sorted, expected);
    // Not persisted → null, never 0.
    for unknown in ["events_live", "events_reconciled", "permission_denied"] {
        assert!(c[unknown].is_null(), "`{unknown}` must be null: {c}");
    }
    assert_eq!(c["watched_roots"][0], s.work.to_string_lossy().as_ref());
    for key in ["id", "short_id", "status", "exit_code"] {
        assert!(c["session"].get(key).is_some(), "session.{key} missing");
    }
    for key in ["percent", "covered", "eligible", "unknown_provenance"] {
        assert!(
            c["before_side_coverage"].get(key).is_some(),
            "before_side_coverage.{key} missing"
        );
    }
    for key in ["os", "check", "status", "value", "detail"] {
        assert!(c["platform"].get(key).is_some(), "platform.{key} missing");
    }
    assert!(["ok", "warn", "unknown"].contains(&c["platform"]["status"].as_str().unwrap()));

    // Without `--capture` the JSON is unchanged: no `capture` key.
    let out = s.hh().args(["doctor", "--json"]).output().unwrap();
    let plain: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(plain.get("capture").is_none());
}

/// Human output end to end: the capture section follows the checks and
/// prints the coverage line; an empty store prints a dash-free hint.
#[test]
fn doctor_capture_human_output() {
    let s = Scenario::new(Repo::Git);
    let empty = s.hh().args(["doctor", "--capture"]).output().unwrap();
    let text = String::from_utf8_lossy(&empty.stdout);
    assert!(
        text.contains("capture health — no recorded sessions yet"),
        "{text}"
    );

    s.record();
    let out = s.hh().args(["doctor", "--capture"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find(|l| l.contains("before-side coverage"))
        .unwrap_or_else(|| panic!("no coverage line: {text}"));
    assert!(line.contains("100%"), "{line}");
    assert!(line.contains("3 of 3"), "{line}");
    assert!(text.contains("● observed live"), "{text}");
}
