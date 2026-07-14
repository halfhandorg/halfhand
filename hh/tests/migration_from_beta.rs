//! Forward-compatibility migration test from the wild (CLAUDE.md v1.0.0
//! addendum: "Migration test from the wild").
//!
//! Opens a database created by **v0.1.0-beta.1** with the current 1.0 binary
//! and proves the session still lists, inspects (valid JSON), exports (JSON +
//! bundle), and replays (`--web` HTML) after any pending migrations run. This
//! is the forward-readable half of STABILITY.md's storage contract.
//!
//! The fixture lives at `tests/fixtures/beta-db/` (`hh.db` + `blobs/`) and was
//! produced by checking out the `v0.1.0-beta.1` tag and running
//! `tests/fixtures/fake_agent.py`; see `tests/fixtures/beta-db/README.md` for
//! provenance and regeneration. The schema was frozen at version 2 at the
//! beta.1 freeze point, so this fixture exercises forward readability rather
//! than a schema delta — exactly the contract a user upgrading from the beta
//! relies on.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Absolute path to a fixture under `tests/fixtures/`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Build a `Command` that runs the compiled `hh` binary with an isolated,
/// config-free HOME/XDG_CONFIG_HOME so the beta DB is the only state in play.
fn hh(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hh"));
    cmd.env("HOME", home).env("XDG_CONFIG_HOME", home);
    cmd
}

/// Recursively copy the committed beta-DB fixture into a fresh temp
/// `HH_DATA_DIR` so the test never mutates the fixture and never touches the
/// real data dir. Each call gets its own unique temp dir (tests run in
/// parallel).
fn copy_fixture_to_temp_data() -> TempDir {
    let data = TempDir::new().expect("create temp data dir");
    copy_dir(&fixture("beta-db"), data.path());
    data
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst dir");
    for entry in fs::read_dir(src).expect("read src dir") {
        let entry = entry.expect("dir entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy fixture file");
        }
    }
}

#[test]
fn beta_db_lists_under_1_0() {
    let home = TempDir::new().expect("create temp home");
    let data = copy_fixture_to_temp_data();
    let out = hh(home.path())
        .arg("list")
        .env("HH_DATA_DIR", data.path())
        .output()
        .expect("run hh list");
    assert!(
        out.status.success(),
        "hh list failed on beta DB: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("73146f"),
        "expected the beta session short id in `hh list`: {stdout}"
    );
}

#[test]
fn beta_db_list_json_is_valid_and_readable() {
    let home = TempDir::new().expect("create temp home");
    let data = copy_fixture_to_temp_data();
    let out = hh(home.path())
        .arg("list")
        .arg("--json")
        .env("HH_DATA_DIR", data.path())
        .output()
        .expect("run hh list --json");
    assert!(
        out.status.success(),
        "hh list --json failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Must be a parseable JSON array with at least one session carrying the
    // beta short id — proving the 1.0 reader decodes the beta row shape.
    let parsed: Vec<serde_json::Value> =
        serde_json::from_str(stdout.trim()).expect("list --json is valid JSON");
    assert!(!parsed.is_empty(), "expected ≥1 session in list --json");
    let ids: String = parsed
        .iter()
        .filter_map(|v| v.get("short_id").and_then(|s| s.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    assert!(
        ids.contains("73146f"),
        "expected beta short id in list --json: {ids}"
    );
}

#[test]
fn beta_db_inspect_json_is_valid() {
    let home = TempDir::new().expect("create temp home");
    let data = copy_fixture_to_temp_data();
    let out = hh(home.path())
        .arg("inspect")
        .arg("last")
        .arg("--json")
        .env("HH_DATA_DIR", data.path())
        .output()
        .expect("run hh inspect last --json");
    assert!(
        out.status.success(),
        "hh inspect last --json failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // inspect --json is an NDJSON stream (one object per event); every line
    // must parse and at least one event must be present.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!lines.is_empty(), "expected ≥1 inspect --json line");
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("inspect --json line not valid JSON: {e}\n{line}"));
    }
}

#[test]
fn beta_db_exports_json_and_bundle() {
    let home = TempDir::new().expect("create temp home");
    let data = copy_fixture_to_temp_data();

    let json_out = data.path().join("export.json");
    let out = hh(home.path())
        .arg("export")
        .arg("last")
        .arg("--out")
        .arg(&json_out)
        .env("HH_DATA_DIR", data.path())
        .output()
        .expect("run hh export last --out");
    assert!(
        out.status.success(),
        "hh export --out failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json_meta = fs::metadata(&json_out).expect("export json file exists");
    assert!(
        json_meta.len() > 0,
        "json export must be non-empty ({} bytes)",
        json_meta.len()
    );
    // The exported JSON bundle itself must parse.
    let body = fs::read_to_string(&json_out).expect("read export json");
    let _: serde_json::Value =
        serde_json::from_str(&body).expect("exported json bundle is valid JSON");

    let bundle_out = data.path().join("export.hh");
    let out = hh(home.path())
        .arg("export")
        .arg("last")
        .arg("--bundle")
        .arg("-o")
        .arg(&bundle_out)
        .env("HH_DATA_DIR", data.path())
        .output()
        .expect("run hh export last --bundle");
    assert!(
        out.status.success(),
        "hh export --bundle failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bundle_meta = fs::metadata(&bundle_out).expect("export bundle file exists");
    assert!(
        bundle_meta.len() > 0,
        "bundle export must be non-empty ({} bytes)",
        bundle_meta.len()
    );
}

#[test]
fn beta_db_replays_to_html() {
    let home = TempDir::new().expect("create temp home");
    let data = copy_fixture_to_temp_data();
    let out = hh(home.path())
        .arg("replay")
        .arg("last")
        .arg("--web")
        .env("HH_DATA_DIR", data.path())
        .output()
        .expect("run hh replay last --web");
    assert!(
        out.status.success(),
        "hh replay last --web failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // `replay --web` prints the path of the HTML file it wrote.
    let html_path = stdout
        .split_whitespace()
        .find(|p| {
            std::path::Path::new(p)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
        })
        .unwrap_or_else(|| panic!("expected an .html path in replay --web output: {stdout}"));
    let meta = fs::metadata(html_path)
        .unwrap_or_else(|e| panic!("replay --web html file missing at {html_path}: {e}"));
    assert!(
        meta.len() > 0,
        "replay html must be non-empty ({} bytes)",
        meta.len()
    );
}
