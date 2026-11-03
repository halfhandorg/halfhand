//! End-to-end tests for `hh init`: artefacts, idempotency, never overwriting
//! a config without `--force`, the three config resolution paths
//! (`config.toml`, `halfhand.toml`, built-in defaults), exit codes, the
//! printed report (insta) and the additive `init` object in `hh doctor --json`.
//!
//! Isolation: `HOME` and `XDG_CONFIG_HOME` point at a per-test temp dir, so
//! the platform config dir (`$XDG_CONFIG_HOME/halfhand` on Linux,
//! `$HOME/Library/Application Support/halfhand` on macOS) is inside it, and
//! `HH_DATA_DIR` (when set) points at another temp path. The real data or
//! config directory is never touched.
//!
//! Unix only: on Windows the config dir comes from the Known Folder API
//! (`%APPDATA%`), which no environment variable redirects, so these tests
//! would write into the real profile. Windows is build-only in CI.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// One isolated environment: a fake home (holding the config dir) and a
/// separate root for data directories.
struct Env {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
        }
    }

    /// The platform config dir `directories` resolves under this home.
    fn config_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home
                .path()
                .join("Library/Application Support/halfhand")
        } else {
            self.home.path().join("halfhand")
        }
    }

    fn config(&self) -> PathBuf {
        self.config_dir().join("config.toml")
    }

    /// A data dir that does not exist yet (nested, to prove recursive
    /// creation).
    fn data(&self) -> PathBuf {
        self.root.path().join("nested/data")
    }

    /// `hh` with HOME/XDG isolated and no HH_DATA_DIR.
    fn hh_no_data_env(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hh"));
        cmd.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path())
            .env("XDG_DATA_HOME", self.root.path().join("xdg-data"))
            .env_remove("HH_DATA_DIR")
            .env("NO_COLOR", "1")
            .env("HH_NO_CONFIG_HINT", "1");
        cmd
    }

    /// `hh` with HH_DATA_DIR = [`Env::data`].
    fn hh(&self) -> Command {
        let mut cmd = self.hh_no_data_env();
        cmd.env("HH_DATA_DIR", self.data());
        cmd
    }

    /// Replace this env's temp paths with placeholders for snapshots.
    fn normalize(&self, s: &str) -> String {
        s.replace(&self.config_dir().display().to_string(), "<config-dir>")
            .replace(&self.data().display().to_string(), "<data-dir>")
    }
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("hh should run")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "hh failed ({:?})\nstdout: {}\nstderr: {}",
        out.status.code(),
        stdout(out),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// `path -> (is_dir, len, file mtime, file content)`.
type Tree = BTreeMap<PathBuf, (bool, u64, Option<std::time::SystemTime>, Vec<u8>)>;

/// Every file and directory under `dirs` with its size, mtime and (for
/// files) content, so "nothing changed" is a real comparison. Directory
/// mtimes are recorded as `None`: opening SQLite briefly creates and removes
/// its `-wal`/`-shm` files, which bumps the data dir's mtime without changing
/// anything in it.
fn tree(dirs: &[&Path]) -> Tree {
    fn walk(p: &Path, out: &mut Tree) {
        let meta = std::fs::metadata(p).unwrap();
        let content = if meta.is_file() {
            std::fs::read(p).unwrap()
        } else {
            Vec::new()
        };
        out.insert(
            p.to_path_buf(),
            (
                meta.is_dir(),
                if meta.is_dir() { 0 } else { meta.len() },
                (!meta.is_dir()).then(|| meta.modified().unwrap()),
                content,
            ),
        );
        if meta.is_dir() {
            for e in std::fs::read_dir(p).unwrap() {
                walk(&e.unwrap().path(), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    for d in dirs {
        walk(d, &mut out);
    }
    out
}

/// Built-in defaults path: no config file anywhere. `hh init` creates every
/// artefact (private on Unix), writes the starter template, and the
/// database opens with the migrated schema.
#[test]
fn init_creates_every_artefact_and_the_database_opens() {
    let env = Env::new();
    let out = run(env.hh().arg("init"));
    assert_ok(&out);

    let data = env.data();
    assert_eq!(
        std::fs::read_to_string(env.config()).unwrap(),
        hh_core::starter_config_template()
    );
    for dir in [data.clone(), data.join("blobs"), data.join("exports")] {
        assert!(dir.is_dir(), "{} missing", dir.display());
        assert_eq!(mode(&dir), 0o700, "{} must be private", dir.display());
    }
    assert_eq!(mode(&env.config_dir()), 0o700);

    let conn = rusqlite::Connection::open(data.join("hh.db")).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);

    // Every path is printed, labelled and absolute.
    let text = stdout(&out);
    for (label, path) in [
        ("config", env.config()),
        ("data dir", data.clone()),
        ("database", data.join("hh.db")),
        ("blobs", data.join("blobs")),
        ("exports", data.join("exports")),
    ] {
        let line = text
            .lines()
            .find(|l| l.trim_start().starts_with(label))
            .unwrap_or_else(|| panic!("no `{label}` row in:\n{text}"));
        assert!(line.contains("created"), "{line}");
        assert!(
            line.ends_with(&path.display().to_string()),
            "{line} should end with {}",
            path.display()
        );
    }
    insta::assert_snapshot!("init_first_run", env.normalize(&text));
}

/// Running twice changes nothing on disk and says so.
#[test]
fn init_is_idempotent() {
    let env = Env::new();
    assert_ok(&run(env.hh().arg("init")));
    let before = tree(&[&env.config_dir(), &env.data()]);
    let out = run(env.hh().arg("init"));
    assert_ok(&out);
    let after = tree(&[&env.config_dir(), &env.data()]);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "no file added or removed"
    );
    assert!(before == after, "no file or directory changed");
    let text = stdout(&out);
    assert!(text.contains("nothing changed"), "{text}");
    assert!(!text.contains("created"), "{text}");
    insta::assert_snapshot!("init_second_run", env.normalize(&text));
}

/// `config.toml` path: an existing config is read (its `[storage] data_dir`
/// honored) and never overwritten without `--force`; `--force` keeps a
/// `.bak` of the old file.
#[test]
fn existing_config_is_never_overwritten_without_force() {
    let env = Env::new();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    let custom = format!(
        "[storage]\ndata_dir = \"{}\"\n[record]\nmax_file_size = \"1MiB\"\n",
        env.data().display()
    );
    std::fs::write(env.config(), &custom).unwrap();

    let out = run(env.hh_no_data_env().arg("init"));
    assert_ok(&out);
    assert_eq!(std::fs::read_to_string(env.config()).unwrap(), custom);
    assert!(
        env.data().join("hh.db").is_file(),
        "data_dir from config.toml"
    );
    let config_line = stdout(&out)
        .lines()
        .find(|l| l.trim_start().starts_with("config"))
        .unwrap()
        .to_string();
    assert!(config_line.contains("found"), "{config_line}");

    let out = run(env.hh_no_data_env().args(["init", "--force"]));
    assert_ok(&out);
    assert_eq!(
        std::fs::read_to_string(env.config()).unwrap(),
        hh_core::starter_config_template()
    );
    assert_eq!(
        std::fs::read_to_string(env.config_dir().join("config.toml.bak")).unwrap(),
        custom
    );
    assert!(stdout(&out).contains("replaced"), "{}", stdout(&out));
    assert!(stdout(&out).contains("config.toml.bak"), "{}", stdout(&out));
}

/// `halfhand.toml` path: it is read (its data dir honored), no config.toml
/// is written next to it, the output says which file is read and offers
/// `--migrate-config`, which renames it.
#[test]
fn legacy_halfhand_toml_is_reported_and_migrated() {
    let env = Env::new();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    let legacy = env.config_dir().join("halfhand.toml");
    let body = format!("[storage]\ndata_dir = \"{}\"\n", env.data().display());
    std::fs::write(&legacy, &body).unwrap();

    let out = run(env.hh_no_data_env().arg("init"));
    assert_ok(&out);
    assert!(!env.config().exists(), "must not shadow halfhand.toml");
    assert!(
        env.data().join("hh.db").is_file(),
        "data_dir from halfhand.toml"
    );
    let text = stdout(&out);
    assert!(text.contains(&legacy.display().to_string()), "{text}");
    assert!(text.contains("--migrate-config"), "{text}");
    insta::assert_snapshot!("init_legacy_config", env.normalize(&text));

    let out = run(env.hh_no_data_env().args(["init", "--migrate-config"]));
    assert_ok(&out);
    assert!(!legacy.exists());
    assert_eq!(std::fs::read_to_string(env.config()).unwrap(), body);
    assert!(stdout(&out).contains("renamed"), "{}", stdout(&out));
}

/// `--force` with only a legacy file refuses (it would shadow the file being
/// read) and changes nothing; it is a runtime refusal, so exit 1.
#[test]
fn force_with_only_a_legacy_config_refuses() {
    let env = Env::new();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(env.config_dir().join("halfhand.toml"), "").unwrap();
    let out = run(env.hh().args(["init", "--force"]));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--migrate-config"));
    assert!(!env.config().exists());
}

/// A usage error (unknown flag) exits 2 and creates nothing.
#[test]
fn init_usage_error_exits_2() {
    let env = Env::new();
    let out = run(env.hh().args(["init", "--bogus"]));
    assert_eq!(out.status.code(), Some(2));
    assert!(!env.data().exists());
    assert!(!env.config().exists());
}

/// When the data dir is the config dir (the macOS default), the output says
/// so explicitly.
#[test]
fn shared_config_and_data_dir_is_called_out() {
    let env = Env::new();
    let out = run(env.hh().env("HH_DATA_DIR", env.config_dir()).arg("init"));
    assert_ok(&out);
    let text = stdout(&out);
    assert!(text.contains("share one directory"), "{text}");
}

/// Init is not a prerequisite: other commands bootstrap implicitly and
/// never write a config file.
#[test]
fn other_commands_do_not_need_init() {
    let env = Env::new();
    assert_ok(&run(env.hh().arg("list")));
    assert!(env.data().join("hh.db").is_file());
    assert!(!env.config().exists());
}

/// `hh doctor --json` gains an additive `init` object reporting what `hh
/// init` would create, and existing fields are untouched.
#[test]
fn doctor_reports_init_status() {
    let env = Env::new();
    let out = run(env.hh().args(["doctor", "--json"]));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["checks"].is_array() && v["status"].is_string() && v["schema"].is_number());
    assert_eq!(v["init"]["initialized"], false);
    let labels: Vec<&str> = v["init"]["would_create"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        ["config", "data dir", "database", "blobs", "exports"]
    );

    assert_ok(&run(env.hh().arg("init")));
    let out = run(env.hh().args(["doctor", "--json"]));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["init"]["initialized"], true);
    assert_eq!(v["init"]["would_create"], serde_json::json!([]));

    let out = run(env.hh().arg("doctor"));
    assert!(stdout(&out).contains("● init — done"), "{}", stdout(&out));
}
