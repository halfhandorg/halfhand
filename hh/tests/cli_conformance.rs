//! CLI conformance test (STABILITY.md (a)): `hh --help` and every
//! `hh <sub> --help` are snapshot-tested so wording can improve but a
//! documented subcommand/flag never silently disappears, and every code in
//! the exit-code contract (`docs/exit-codes.md`: `0` success, `1` generic
//! error, `2` usage error, `3` session not found, `4` `hh scan` with findings,
//! plus `hh run`'s pass-through and the documented known gaps) is asserted
//! end to end against the real binary.
//!
//! Deliberately self-contained (does not share helpers with `tests/cli.rs`):
//! each file under `tests/` compiles as its own crate, and this file's scope
//! is narrow enough that duplicating a handful of small helpers is simpler
//! than factoring out a shared support module.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Every subcommand `hh --help` lists today (SRS FR-6.2 / STABILITY.md (a)).
/// A subcommand leaving this list is a CLI-stability break — removing one
/// requires the STABILITY.md (a) deprecation cycle, not a silent drop here.
const SUBCOMMANDS: &[&str] = &[
    "init",
    "run",
    "replay",
    "inspect",
    "list",
    "rename",
    "delete",
    "mcp-proxy",
    "doctor",
    "gc",
    "stats",
    "scan",
    "redact",
    "export",
    "import",
    "search",
    "completions",
];

/// An empty config-home directory shared by every [`hh()`] call in this test
/// binary, so no test's outcome depends on the developer machine's real
/// `~/.config/halfhand/config.toml` (mirrors `tests/cli.rs`'s `hh()`).
fn isolated_config_home() -> &'static Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| tempfile::tempdir().expect("isolated config-home tempdir"))
        .path()
}

/// Build a `Command` running the compiled `hh` binary with an isolated,
/// config-free `HOME`/`XDG_CONFIG_HOME`.
fn hh() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hh"));
    let home = isolated_config_home();
    cmd.env("HOME", home);
    cmd.env("XDG_CONFIG_HOME", home);
    cmd
}

/// Absolute path to a fixture under `tests/fixtures/`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// ---------------------------------------------------------------------------
// `--help` snapshots
// ---------------------------------------------------------------------------

#[test]
fn hh_help_matches_snapshot() {
    let out = hh().arg("--help").output().expect("hh --help should run");
    assert!(out.status.success(), "hh --help must exit 0");
    insta::assert_snapshot!("hh_help", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn every_subcommand_help_matches_snapshot() {
    for sub in SUBCOMMANDS {
        let out = hh()
            .args([*sub, "--help"])
            .output()
            .unwrap_or_else(|e| panic!("hh {sub} --help should run: {e}"));
        assert!(out.status.success(), "hh {sub} --help must exit 0");
        let snapshot_name = format!("hh_{}_help", sub.replace('-', "_"));
        insta::assert_snapshot!(snapshot_name, String::from_utf8_lossy(&out.stdout));
    }
}

// ---------------------------------------------------------------------------
// Documented exit codes (STABILITY.md (a))
// ---------------------------------------------------------------------------

/// A temp data dir + work dir pair. Tests never touch the real data dir
/// (CLAUDE.md). Mirrors `tests/cli.rs`'s `Temp`.
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

/// `2` — clap's own usage-error exit code, independent of any `hh` logic:
/// an unrecognized flag never reaches subcommand dispatch.
#[test]
fn usage_error_exits_2() {
    let out = hh()
        .args(["list", "--this-flag-does-not-exist"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "an unrecognized flag must exit 2 (usage error); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // An unrecognized subcommand is also a usage error.
    let out2 = hh().args(["not-a-real-subcommand"]).output().unwrap();
    assert_eq!(
        out2.status.code(),
        Some(2),
        "an unrecognized subcommand must exit 2 (usage error); stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
}

/// `3` — any subcommand that resolves a session id/`last` and fails to.
#[test]
fn unknown_session_exits_3() {
    let temp = Temp::new();
    let out = hh()
        .args(["inspect", "no-such-session"])
        .env("HH_DATA_DIR", temp.data.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "an unresolvable session id must exit 3; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `4` — `hh scan` exiting with findings (the one place this code is used
/// today; see STABILITY.md (a)).
#[test]
fn scan_with_findings_exits_4() {
    let temp = Temp::new();
    let fx = fixture("secret_agent.sh").to_string_lossy().to_string();
    let record = hh()
        .args(["run", "--", "sh", &fx])
        .env("HH_DATA_DIR", temp.data.path())
        .current_dir(temp.work.path())
        .stdin(Stdio::null())
        .output()
        .expect("hh run should execute");
    assert_eq!(
        record.status.code(),
        Some(0),
        "secret_agent.sh fixture should exit 0; stderr: {}",
        String::from_utf8_lossy(&record.stderr)
    );

    let scan = hh()
        .args(["scan", "last"])
        .env("HH_DATA_DIR", temp.data.path())
        .output()
        .unwrap();
    assert_eq!(
        scan.status.code(),
        Some(4),
        "hh scan with findings must exit 4; stdout: {} stderr: {}",
        String::from_utf8_lossy(&scan.stdout),
        String::from_utf8_lossy(&scan.stderr)
    );
}

// ---------------------------------------------------------------------------
// Every code in docs/exit-codes.md, plus the documented pass-through and known
// gaps. Each assertion pins what the binary does today; a change here is an
// exit-code change and goes through STABILITY.md.
// ---------------------------------------------------------------------------

/// Run `hh <args>` against `temp`'s data dir with stdin closed (so nothing
/// can prompt), from `temp`'s work dir. Returns `(code, stderr)`.
fn hh_in(temp: &Temp, args: &[&str]) -> (Option<i32>, String) {
    let out = hh()
        .args(args)
        .env("HH_DATA_DIR", temp.data.path())
        .env_remove("HH_SESSION_ID")
        .current_dir(temp.work.path())
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("hh {args:?} should run: {e}"));
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Assert `hh <args>` exits `want`.
fn assert_exit(temp: &Temp, args: &[&str], want: i32) {
    let (code, stderr) = hh_in(temp, args);
    assert_eq!(
        code,
        Some(want),
        "`hh {}` must exit {want}; stderr: {stderr}",
        args.join(" ")
    );
}

/// Record one session that exits 0 (no secrets) so commands have a `last`.
fn record_clean_session(temp: &Temp) {
    assert_exit(temp, &["run", "--", "sh", "-c", "echo hello"], 0);
}

/// `0` — success: a successful command, help/version, and a clean scan.
#[test]
fn success_exits_0() {
    let temp = Temp::new();
    assert_exit(&temp, &["list"], 0);
    assert_exit(&temp, &["--version"], 0);
    assert_exit(&temp, &["help"], 0);
    // `--all` over an empty store is a clean scan, not "session not found".
    assert_exit(&temp, &["scan", "--all"], 0);

    record_clean_session(&temp);
    assert_exit(&temp, &["inspect", "last"], 0);
    assert_exit(&temp, &["scan", "last"], 0);
    assert_exit(&temp, &["scan", "--all", "--json"], 0);
    assert_exit(&temp, &["export", "last"], 0);
    assert_exit(&temp, &["inspect", "last", "--context"], 0);
    assert_exit(&temp, &["inspect", "last", "--context", "--json"], 0);
    assert_exit(&temp, &["rename", "last", "smoke test"], 0);
    assert_exit(&temp, &["rename", "last", "--clear"], 0);
    assert_exit(&temp, &["delete", "last", "--yes"], 0);
}

/// `1` — generic error: a failure that is not usage, session-not-found or a
/// scan finding. A missing import file is the canonical case.
#[test]
fn generic_error_exits_1() {
    let temp = Temp::new();
    let missing = temp.work.path().join("no-such-bundle.hh");
    let (code, stderr) = hh_in(&temp, &["import", missing.to_str().unwrap()]);
    assert_eq!(code, Some(1), "unreadable bundle must exit 1: {stderr}");
    assert!(
        stderr.contains("why:"),
        "a generic error is still actionable (why/hint): {stderr}"
    );
    // An invalid FTS5 query is a generic error too.
    assert_exit(&temp, &["search", "\"unterminated"], 1);
}

/// `2` — every flavor of command-line parse failure, not just unknown flags.
#[test]
fn every_usage_error_exits_2() {
    let temp = Temp::new();
    for args in [
        &["list", "--limit", "many"][..],                 // wrong value type
        &["delete"],                                      // missing required argument
        &["export", "--html", "--bundle"],                // conflicting flags
        &["scan", "last", "--all"],                       // session conflicts with --all
        &["completions", "no-such-shell"],                // invalid enum value
        &[],                                              // no subcommand at all
        &["rename", "last"],                              // neither a name nor --clear
        &["rename", "last", "   "],                       // a blank name
        &["rename", "last", "x", "--clear"],              // name conflicts with --clear
        &["inspect", "last", "--context", "--step", "1"], // conflicting views
        &["run", "--name", "", "--", "true"],             // empty --name, before recording
    ] {
        assert_exit(&temp, args, 2);
    }
}

/// `3` — every subcommand that takes a session argument, for an unknown id
/// and for `last` on an empty store.
#[test]
fn every_session_taking_command_exits_3_when_not_found() {
    let temp = Temp::new();
    for session in ["no-such-session", "last"] {
        for args in [
            vec!["inspect", session],
            vec!["replay", session, "--web"],
            vec!["export", session],
            vec!["delete", session, "--yes"],
            vec!["scan", session],
            vec!["redact", session, "--yes"],
            vec!["rename", session, "some name"],
            vec!["inspect", session, "--context"],
        ] {
            assert_exit(&temp, &args, 3);
        }
    }
}

/// `4` — `hh scan` with findings, for one session, `--all`, and `--json`.
#[test]
fn scan_with_findings_exits_4_for_all_and_json() {
    let temp = Temp::new();
    let fx = fixture("secret_agent.sh").to_string_lossy().to_string();
    assert_exit(&temp, &["run", "--", "sh", &fx], 0);
    assert_exit(&temp, &["scan", "--all"], 4);
    assert_exit(&temp, &["scan", "last", "--json"], 4);
    assert_exit(&temp, &["scan", "--all", "--json"], 4);
}

/// Pass-through (docs/exit-codes.md): `hh run` exits with the wrapped
/// command's own code, so 2/3/4 from `hh run` are the child's codes, not
/// Halfhand's usage / not-found / policy codes.
#[test]
fn run_passes_the_wrapped_commands_exit_code_through() {
    let temp = Temp::new();
    for code in [0, 1, 2, 3, 4, 42] {
        let script = format!("exit {code}");
        assert_exit(&temp, &["run", "--", "sh", "-c", &script], code);
    }
}

/// Pass-through: a wrapped command killed by a signal makes `hh run` exit 1.
#[cfg(unix)] // `kill -9 $$` is POSIX sh; Windows CI is build-only
#[test]
fn run_exits_1_when_the_wrapped_command_is_killed_by_a_signal() {
    let temp = Temp::new();
    assert_exit(&temp, &["run", "--", "sh", "-c", "kill -9 $$"], 1);
}

/// Known gap 2 (docs/exit-codes.md): session-not-found cases that exit 1.
#[test]
fn known_gap_some_session_not_found_cases_exit_1() {
    let temp = Temp::new();
    // `hh mcp-proxy` attaching to an unknown HH_SESSION_ID.
    let out = hh()
        .args(["mcp-proxy", "--", "sh", "-c", "exit 0"])
        .env("HH_DATA_DIR", temp.data.path())
        .env("HH_SESSION_ID", "no-such-session")
        .current_dir(temp.work.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "mcp-proxy with an unknown HH_SESSION_ID exits 1 today; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // `hh replay <unknown>` without a terminal: the TTY check runs first.
    assert_exit(&temp, &["replay", "no-such-session"], 1);
}

/// Known gap 3 (docs/exit-codes.md): usage-style errors `hh` checks itself,
/// after parsing, exit 1 rather than 2.
#[test]
fn known_gap_post_parse_usage_errors_exit_1() {
    let temp = Temp::new();
    record_clean_session(&temp);
    assert_exit(&temp, &["inspect", "last", "--json", "--diff"], 1);
    assert_exit(
        &temp,
        &["run", "--adapter", "no-such-adapter", "--", "true"],
        1,
    );
    assert_exit(&temp, &["replay", "last"], 1); // no terminal
}

/// Known gap 4 (docs/exit-codes.md): policy refusals other than `hh scan`
/// exit 1, not 4.
#[test]
fn known_gap_policy_refusals_other_than_scan_exit_1() {
    let temp = Temp::new();
    record_clean_session(&temp);
    assert_exit(&temp, &["export", "last", "--no-redact"], 1);
    assert_exit(&temp, &["redact", "last"], 1);
    assert_exit(&temp, &["delete", "last"], 1);
}
