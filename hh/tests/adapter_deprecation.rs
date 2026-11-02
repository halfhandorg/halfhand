//! Deprecated `hh run --adapter` values (STABILITY.md (a); `docs/adapters.md`,
//! "Deprecated flags").
//!
//! `--adapter codex-cli`, `--adapter gemini-cli` and `--adapter claude-desktop`
//! are scheduled for removal in the next minor release. Until then each must
//! keep working exactly as before and print exactly one standard deprecation
//! line (`hh_core::warn_deprecated`'s format). `--adapter claude-code` and
//! auto-detection never warn.
//!
//! Self-contained like `tests/cli_conformance.rs`: each file under `tests/`
//! is its own crate, and the helpers here are small enough to duplicate.

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// The prefix every standard deprecation line starts with
/// (`hh_core::deprecation::format_deprecation_warning`).
const DEPRECATION_PREFIX: &str = "hh: warning: ";

/// Run `hh run [--adapter <adapter>] -- true` against throwaway data, config
/// and home dirs (never the real ones — CLAUDE.md testing standards). The
/// empty `HOME` means no agent transcript can exist, so a structured adapter
/// degrades exactly as it would have before the deprecation.
fn run_true(adapter: Option<&str>, data: &Path, home: &Path, work: &Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hh"));
    cmd.arg("run");
    if let Some(a) = adapter {
        cmd.args(["--adapter", a]);
    }
    cmd.args(["--", "true"])
        .env("HH_DATA_DIR", data)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env_remove("HH_DEBUG")
        .current_dir(work)
        .stdin(Stdio::null())
        .output()
        .expect("hh run should execute")
}

/// The lines of `stderr` that are deprecation warnings.
fn deprecation_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.starts_with(DEPRECATION_PREFIX) && l.contains(" is deprecated; "))
        .collect()
}

/// `(agent_kind, adapter_status, status)` of the only recorded session.
fn only_session(data: &Path) -> (String, String, String) {
    let conn = rusqlite::Connection::open(data.join("hh.db")).expect("hh.db exists");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "exactly one session should be recorded");
    conn.query_row(
        "SELECT agent_kind, adapter_status, status FROM sessions",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

/// Run one deprecated adapter flag and return its single deprecation line,
/// after asserting the run behaved exactly as an un-deprecated run would:
/// exit 0, one session recorded `ok` under the adapter's agent kind.
fn assert_warns_once_and_still_runs(adapter: &str) -> String {
    let data = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let out = run_true(Some(adapter), data.path(), home.path(), work.path());
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        out.status.code(),
        Some(0),
        "`hh run --adapter {adapter} -- true` must still run and propagate exit 0; stderr: {stderr}"
    );
    let lines = deprecation_lines(&stderr);
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one deprecation line for --adapter {adapter}, got {}: {stderr}",
        lines.len()
    );
    let line = lines[0];
    assert!(
        line.starts_with(&format!(
            "hh: warning: `--adapter {adapter}` is deprecated; "
        )),
        "deprecation line must use the standard format: {line}"
    );
    assert!(
        line.contains("removed in the next minor release"),
        "deprecation line must say when the flag goes away: {line}"
    );

    let (agent_kind, _adapter_status, status) = only_session(data.path());
    assert_eq!(
        agent_kind, adapter,
        "the forced adapter is still selected (behavior unchanged)"
    );
    assert_eq!(status, "ok", "the session still records and finalizes ok");
    line.to_string()
}

#[test]
fn adapter_codex_cli_prints_exactly_one_deprecation_line_and_still_runs() {
    let line = assert_warns_once_and_still_runs("codex-cli");
    insta::assert_snapshot!(line);
}

#[test]
fn adapter_gemini_cli_prints_exactly_one_deprecation_line_and_still_runs() {
    let line = assert_warns_once_and_still_runs("gemini-cli");
    insta::assert_snapshot!(line);
}

#[test]
fn adapter_claude_desktop_prints_exactly_one_deprecation_line_and_still_runs() {
    let line = assert_warns_once_and_still_runs("claude-desktop");
    insta::assert_snapshot!(line);
}

#[test]
fn adapter_claude_code_and_auto_detection_do_not_warn() {
    for adapter in [Some("claude-code"), None] {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let out = run_true(adapter, data.path(), home.path(), work.path());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
        assert!(
            deprecation_lines(&stderr).is_empty(),
            "{adapter:?} must not print a deprecation warning: {stderr}"
        );
    }
}
