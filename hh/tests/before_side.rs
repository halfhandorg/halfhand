//! End-to-end regression tests for file-change before-sides (migration 0006).
//!
//! The bug these guard: the first change to a file that existed before the
//! session started was recorded with no before-side, so `hh inspect --diff`
//! rendered a one-line edit as the whole file added (0 removed lines), a
//! delete as "(content not captured)", and a rename lost its old path
//! entirely. ~380 tests passed with that bug because none of them recorded a
//! change to a *pre-existing* file and then read the diff back.
//!
//! Every test here drives the real `hh` binary (`hh run` → `hh inspect
//! --diff`) against `tests/fixtures/before_side_agent.sh`, in a temp work dir
//! that is (a) a git repository with everything committed, (b) not a git
//! repository at all, or (c) a git repository with the developer's own
//! uncommitted edit in place before the session. No `thread::sleep`: `hh run`
//! returns only after the watcher's shutdown flush + backstop, so the
//! recording is complete and deterministic when we read it.
//!
//! Isolation follows `cli.rs`: `HH_DATA_DIR` and `HOME`/`XDG_CONFIG_HOME`
//! point at temp dirs (never the real data or config dir), and
//! `GIT_CEILING_DIRECTORIES` stops git discovery at the temp root so the
//! "not a git repository" case cannot accidentally find an enclosing repo.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The 20-line pre-existing file the fixture edits one line of.
fn twenty_lines() -> String {
    (1..=20).fold(String::new(), |mut s, i| {
        let _ = writeln!(s, "line {i}");
        s
    })
}

struct Scenario {
    _root: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    work: PathBuf,
    home: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Repo {
    /// Everything committed: before-sides come from the git index.
    GitClean,
    /// No repository: before-sides come from the session-start baseline.
    NoGit,
    /// Committed, then the developer edits line 3 before the session.
    GitDeveloperDirty,
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
        std::fs::write(work.join("f.txt"), twenty_lines()).unwrap();
        std::fs::write(work.join("del.txt"), "doomed line one\ndoomed line two\n").unwrap();
        std::fs::write(work.join("old.txt"), "moving content\n").unwrap();
        let w = Self {
            _root: tmp,
            root,
            data,
            work,
            home,
        };
        if repo != Repo::NoGit {
            w.git(&["init", "-q"]);
            w.git(&["add", "-A"]);
            w.git(&["commit", "-q", "-m", "initial"]);
        }
        if repo == Repo::GitDeveloperDirty {
            // The developer's own uncommitted edit, made before `hh run`.
            let edited = twenty_lines().replace("line 3\n", "line 3 DEV EDIT\n");
            std::fs::write(w.work.join("f.txt"), edited).unwrap();
        }
        w
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

    /// Record the fixture agent; return the session id.
    fn record(&self) -> String {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/before_side_agent.sh");
        let out = self
            .hh()
            .args(["run", "--", "sh"])
            .arg(fixture)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "hh run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let conn = rusqlite::Connection::open(self.data.join("hh.db")).unwrap();
        conn.query_row("SELECT id FROM sessions", [], |r| r.get(0))
            .unwrap()
    }

    fn diff(&self, id: &str) -> String {
        let out = self.hh().args(["inspect", id, "--diff"]).output().unwrap();
        assert!(
            out.status.success(),
            "hh inspect --diff failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// `(change_kind, before_source, renamed_from)` for `path`.
    fn row(&self, path: &str) -> (String, Option<String>, Option<String>) {
        let conn = rusqlite::Connection::open(self.data.join("hh.db")).unwrap();
        conn.query_row(
            "SELECT change_kind, before_source, renamed_from FROM file_changes WHERE path = ?1",
            [path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or_else(|e| panic!("no file_changes row for {path}: {e}"))
    }

    fn paths_recorded(&self) -> Vec<String> {
        let conn = rusqlite::Connection::open(self.data.join("hh.db")).unwrap();
        let mut stmt = conn
            .prepare("SELECT path FROM file_changes ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
}

/// Split `hh inspect --diff` output into per-file sections keyed by the
/// `b/` path on their `diff -- a/… b/…` header, sorted by path (the order
/// events land in depends on debounce timing; the content does not).
fn sections(diff: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff -- ") {
            let b = rest.rsplit(" b/").next().unwrap_or(rest).to_string();
            out.push((b, String::new()));
        }
        if let Some((_, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    // The blank separator line after a section depends on whether it was
    // printed last, i.e. on event order — drop it so sections compare equal.
    for (_, body) in &mut out {
        let trimmed = body.trim_end().len();
        body.truncate(trimmed);
        body.push('\n');
    }
    out.sort();
    out
}

fn section<'a>(diff: &'a [(String, String)], path: &str) -> &'a str {
    diff.iter().find(|(p, _)| p == path).map_or_else(
        || panic!("no diff section for {path}: {diff:?}"),
        |(_, s)| s.as_str(),
    )
}

/// Count `+`/`-` content lines (not the `+++`/`---` file labels).
fn added_removed(section: &str) -> (Vec<&str>, Vec<&str>) {
    let added = section
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .collect();
    let removed = section
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .collect();
    (added, removed)
}

fn assert_one_line_edit(w: &Scenario, id: &str, expected_source: &str) {
    let diff = sections(&w.diff(id));
    let (added, removed) = added_removed(section(&diff, "f.txt"));
    assert_eq!(added, ["+line SEVEN"], "exactly one added line: {diff:?}");
    assert_eq!(removed, ["-line 7"], "exactly one removed line: {diff:?}");
    let (kind, source, from) = w.row("f.txt");
    assert_eq!(kind, "modified");
    assert_eq!(source.as_deref(), Some(expected_source));
    assert_eq!(from, None);
}

fn assert_delete_keeps_content(w: &Scenario, id: &str, expected_source: &str) {
    let diff = sections(&w.diff(id));
    let del = section(&diff, "del.txt");
    assert!(del.contains("+++ /dev/null"), "delete section: {del}");
    let (added, removed) = added_removed(del);
    assert!(added.is_empty(), "a delete adds nothing: {del}");
    assert_eq!(removed, ["-doomed line one", "-doomed line two"], "{del}");
    assert!(!del.contains("content not captured"), "{del}");
    let (kind, source, _) = w.row("del.txt");
    assert_eq!(kind, "deleted");
    assert_eq!(source.as_deref(), Some(expected_source));
}

fn assert_rename_is_one_row(w: &Scenario, id: &str, expected_source: &str) {
    let (kind, source, from) = w.row("new.txt");
    assert_eq!(
        kind, "modified",
        "a rename is one modified row on the new path"
    );
    assert_eq!(from.as_deref(), Some("old.txt"));
    assert_eq!(source.as_deref(), Some(expected_source));
    assert!(
        !w.paths_recorded().iter().any(|p| p == "old.txt"),
        "the old path must not be recorded as a separate delete: {:?}",
        w.paths_recorded()
    );
    let diff = sections(&w.diff(id));
    let ren = section(&diff, "new.txt");
    assert!(ren.starts_with("diff -- a/old.txt b/new.txt\n"), "{ren}");
    assert!(
        ren.contains("rename from old.txt\nrename to new.txt\n"),
        "{ren}"
    );
    let (added, removed) = added_removed(ren);
    assert!(added.is_empty() && removed.is_empty(), "pure rename: {ren}");
}

#[test]
fn git_repo_one_line_edit_shows_one_added_one_removed() {
    let w = Scenario::new(Repo::GitClean);
    let id = w.record();
    assert_one_line_edit(&w, &id, "git");
}

#[test]
fn git_repo_delete_shows_removed_content() {
    let w = Scenario::new(Repo::GitClean);
    let id = w.record();
    assert_delete_keeps_content(&w, &id, "git");
}

#[test]
fn git_repo_rename_records_as_rename() {
    let w = Scenario::new(Repo::GitClean);
    let id = w.record();
    assert_rename_is_one_row(&w, &id, "git");
}

#[test]
fn non_git_one_line_edit_shows_one_added_one_removed() {
    let w = Scenario::new(Repo::NoGit);
    let id = w.record();
    assert_one_line_edit(&w, &id, "baseline");
}

#[test]
fn non_git_delete_shows_removed_content() {
    let w = Scenario::new(Repo::NoGit);
    let id = w.record();
    assert_delete_keeps_content(&w, &id, "baseline");
}

#[test]
fn non_git_rename_records_as_rename() {
    let w = Scenario::new(Repo::NoGit);
    let id = w.record();
    assert_rename_is_one_row(&w, &id, "baseline");
}

/// The developer edited line 3 (uncommitted) before starting the session;
/// the agent then edited line 7. The diff must show ONLY the agent's change.
/// Diffing against the git index or HEAD would also show `-line 3` /
/// `+line 3 DEV EDIT` and attribute the developer's work to the agent.
#[test]
fn developer_uncommitted_edit_is_not_attributed_to_the_agent() {
    let w = Scenario::new(Repo::GitDeveloperDirty);
    let id = w.record();
    let diff = sections(&w.diff(&id));
    let f = section(&diff, "f.txt");
    let (added, removed) = added_removed(f);
    assert_eq!(added, ["+line SEVEN"], "{f}");
    assert_eq!(removed, ["-line 7"], "{f}");
    // The developer's line is unchanged context (or out of the hunk), never
    // a +/- line.
    assert!(!f.contains("-line 3\n"), "{f}");
    assert!(!f.contains("+line 3 DEV EDIT"), "{f}");
    let (_, source, _) = w.row("f.txt");
    assert_eq!(
        source.as_deref(),
        Some("baseline"),
        "a file dirty against the index must use its session-start content, not git"
    );
    // The untouched-by-the-developer files in the same repo still come from git.
    assert_eq!(w.row("del.txt").1.as_deref(), Some("git"));
}

/// Human-readable output of all three change kinds (CLAUDE.md: rendered
/// output ships with an insta snapshot). Sections are sorted by path so the
/// snapshot does not depend on debounce timing.
#[test]
fn snapshot_inspect_diff_of_edit_delete_rename() {
    let w = Scenario::new(Repo::GitClean);
    let id = w.record();
    let rendered: String = sections(&w.diff(&id))
        .into_iter()
        .map(|(_, s)| s)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered);
}
