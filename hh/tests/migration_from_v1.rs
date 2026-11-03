//! Migration test from the wild for migration 0007 (CLAUDE.md v1.0.0
//! addendum; STABILITY.md (c)): a database written by **hh 1.1.0** opens
//! under the current binary with no data loss.
//!
//! The fixture (`tests/fixtures/v1-db/`, provenance in its README) was
//! recorded by the real 1.1.0 binary at schema version 6. The test reads
//! every row of every pre-0007 table with raw SQLite *before* the new binary
//! touches it, lets `hh` migrate it, and asserts that every one of those
//! values reads back identical, that the 0007 columns exist, and that
//! reopening again changes nothing.

use rusqlite::types::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if from.is_dir() {
            copy_dir(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

/// Run `hh` against `data` with an isolated, config-free home.
fn hh(data: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new(env!("CARGO_BIN_EXE_hh"))
        .args(args)
        .env("HH_DATA_DIR", data)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "hh {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Every row of `table`, restricted to `columns`, ordered by rowid.
fn dump(conn: &rusqlite::Connection, table: &str, columns: &[String]) -> Vec<Vec<Value>> {
    let sql = format!("SELECT {} FROM {table} ORDER BY rowid", columns.join(", "));
    let mut stmt = conn.prepare(&sql).unwrap();
    stmt.query_map([], |r| {
        (0..columns.len()).map(|i| r.get::<_, Value>(i)).collect()
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn columns(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

const TABLES: &[&str] = &[
    "sessions",
    "events",
    "file_changes",
    "blobs",
    "schema_migrations",
];

#[test]
fn v1_1_database_upgrades_to_0007_without_data_loss() {
    let data = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    copy_dir(&fixture("v1-db"), data.path());
    let db = data.path().join("hh.db");

    // Before: the 1.1.0 schema and every value in it.
    let before: Vec<(String, Vec<String>, Vec<Vec<Value>>)> = {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v, 6, "the fixture is a schema-6 (v1.1.0) database");
        TABLES
            .iter()
            .map(|t| {
                let cols = columns(&conn, t);
                let rows = dump(&conn, t, &cols);
                ((*t).to_string(), cols, rows)
            })
            .collect()
    };
    assert!(
        !columns(&rusqlite::Connection::open(&db).unwrap(), "sessions")
            .contains(&"model_id".to_string())
    );

    // Open with the new binary (twice: the second open must be a no-op).
    let first = hh(data.path(), home.path(), &["list", "--json"]).stdout;
    let second = hh(data.path(), home.path(), &["list", "--json"]).stdout;
    assert_eq!(first, second, "reopening is idempotent");

    let conn = rusqlite::Connection::open(&db).unwrap();
    for (table, cols, rows) in &before {
        let after = dump(&conn, table, cols);
        if table == "schema_migrations" {
            assert_eq!(&after[..rows.len()], &rows[..], "earlier versions kept");
            continue;
        }
        assert_eq!(&after, rows, "every pre-0007 value in `{table}` survives");
    }
    let v: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(v, 7);
    let session_cols = columns(&conn, "sessions");
    for c in [
        "name",
        "model_id",
        "model_display",
        "provider",
        "context_limit",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
        "capture_tier",
        "adapter_id",
    ] {
        assert_eq!(
            session_cols.iter().filter(|n| *n == c).count(),
            1,
            "`{c}` added exactly once"
        );
    }
    assert!(
        !session_cols.contains(&"total_tokens".to_string()),
        "total is never stored"
    );

    // Through the JSON surface: the Desktop session's legacy model is carried
    // into model_id and resolved from the table; usage it never accumulated
    // reads as null, not 0.
    let list: Vec<serde_json::Value> = serde_json::from_slice(&first).unwrap();
    assert_eq!(list.len(), 2);
    let desktop = list
        .iter()
        .find(|s| s["agent_kind"] == "claude-desktop")
        .unwrap();
    assert_eq!(desktop["model_id"], "claude-sonnet-5");
    assert_eq!(desktop["model_display"], "Claude Sonnet 5");
    assert_eq!(desktop["provider"], "Anthropic");
    assert!(desktop["usage"]["input_tokens"].is_null());
    assert!(desktop["usage"]["total_tokens"].is_null());
    assert_eq!(desktop["steps"], 34);
    let generic = list.iter().find(|s| s["agent_kind"] == "generic").unwrap();
    assert!(generic["model_id"].is_null());
    assert_eq!(generic["exit_code"], 3);
    assert_eq!(generic["files_changed"], 1);

    // Every session still inspects, with and without --context.
    for s in &list {
        let id = s["short_id"].as_str().unwrap();
        hh(data.path(), home.path(), &["inspect", id]);
        let ctx = hh(
            data.path(),
            home.path(),
            &["inspect", id, "--context", "--json"],
        )
        .stdout;
        let ctx: serde_json::Value = serde_json::from_slice(&ctx).unwrap();
        assert_eq!(ctx["session"]["short_id"], id);
        assert!(ctx["estimated_cost_usd"].is_null(), "no usage → no cost");
    }
}
