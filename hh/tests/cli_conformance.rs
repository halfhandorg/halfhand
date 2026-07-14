//! CLI conformance test (STABILITY.md (a)): `hh --help` and every
//! `hh <sub> --help` are snapshot-tested so wording can improve but a
//! documented subcommand/flag never silently disappears, and the documented
//! exit-code contract (`2` usage error, `3` session not found, `4` `hh scan`
//! with findings) is asserted end to end against the real binary.
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
    "run",
    "replay",
    "inspect",
    "list",
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
