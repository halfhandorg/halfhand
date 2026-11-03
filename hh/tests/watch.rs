//! End-to-end tests for `hh watch`: the real binary, a throwaway data dir
//! (`HH_DATA_DIR`), and directories in temp dirs — never the real data dir.
//!
//! Synchronization is by pipes and process exit, never by sleeping: a
//! foreground watcher prints its "● Watching" line only after the watcher has
//! taken its baseline, so reading that line from its stdout is the latch
//! after which every change is recorded; and the watcher's shutdown rescans
//! the directory before finalizing, so a change made just before the stop
//! signal is still captured.

use std::path::Path;
use std::process::Command;

/// A `Command` for the compiled `hh`, with `HOME` and the config dir pointed
/// at `home` so no real config is read, and the data dir at `data`.
fn hh(home: &Path, data: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hh"));
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home)
        .env("HH_DATA_DIR", data)
        .env("NO_COLOR", "1");
    cmd
}

/// The temp dirs one test needs: a home, a data dir, and the watched dir.
struct Fixture {
    home: tempfile::TempDir,
    data: tempfile::TempDir,
    work: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            data: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        }
    }

    fn hh(&self) -> Command {
        hh(self.home.path(), self.data.path())
    }

    fn work(&self) -> std::path::PathBuf {
        std::fs::canonicalize(self.work.path()).unwrap()
    }

    /// `hh list --json`, parsed.
    fn sessions(&self) -> Vec<serde_json::Value> {
        let out = self.hh().args(["list", "--json"]).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v.as_array().unwrap().clone()
    }
}

#[cfg(unix)] // Starting, signalling and killing a watcher process use Unix signals.
mod unix {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Child, ChildStdout, Stdio};

    /// A foreground `hh watch` child that has printed its ready line.
    struct Watcher {
        child: Child,
        stdout: BufReader<ChildStdout>,
        ready: String,
    }

    impl Watcher {
        /// Spawn `hh watch <dir>` and block until it reports it is observing.
        fn start(fx: &Fixture, dir: &Path) -> Self {
            let mut child = fx
                .hh()
                .arg("watch")
                .arg(dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let mut stdout = BufReader::new(child.stdout.take().unwrap());
            let mut ready = String::new();
            stdout.read_line(&mut ready).unwrap();
            assert!(ready.starts_with("● Watching "), "ready line: {ready:?}");
            Self {
                child,
                stdout,
                ready,
            }
        }

        fn signal(&self, sig: &str) {
            let ok = Command::new("kill")
                .arg(format!("-{sig}"))
                .arg(self.child.id().to_string())
                .status()
                .unwrap()
                .success();
            assert!(ok, "kill -{sig} failed");
        }

        /// Send `sig`, wait for exit, return (success, rest of stdout).
        fn finish(mut self, sig: &str) -> (std::process::ExitStatus, String) {
            self.signal(sig);
            let status = self.child.wait().unwrap();
            let mut rest = String::new();
            self.stdout.read_to_string(&mut rest).unwrap();
            (status, rest)
        }
    }

    /// The point of `hh watch`: a change made by a process hh did not start
    /// is recorded, and Ctrl-C (SIGINT) finalizes the session.
    #[test]
    fn records_a_change_made_by_an_unrelated_process() {
        let fx = Fixture::new();
        let work = fx.work();
        std::fs::write(work.join("notes.md"), "draft\n").unwrap();
        let w = Watcher::start(&fx, &work);

        // An unrelated process: a shell hh knows nothing about.
        let ok = Command::new("sh")
            .arg("-c")
            .arg("printf 'hello\\n' > new.txt && printf 'final\\n' > notes.md")
            .current_dir(&work)
            .status()
            .unwrap()
            .success();
        assert!(ok);

        let (status, rest) = w.finish("INT");
        assert!(status.success(), "hh watch exits 0 on Ctrl-C: {status:?}");
        assert!(rest.contains("Recorded session"), "epilogue: {rest:?}");
        assert!(rest.contains("2 files changed"), "epilogue: {rest:?}");

        let sessions = fx.sessions();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s["status"], "ok");
        assert_eq!(s["capture_tier"], "effects");
        assert_eq!(s["command"], serde_json::json!([]));
        assert_eq!(s["files_changed"], 2);
        assert_eq!(s["cwd"], work.to_string_lossy().as_ref());

        // The edit has a real before-side: the session-start baseline.
        let out = fx
            .hh()
            .args(["inspect", "last", "--diff"])
            .output()
            .unwrap();
        let diff = String::from_utf8_lossy(&out.stdout);
        assert!(diff.contains("-draft"), "baseline before-side: {diff}");
        assert!(diff.contains("+final"), "{diff}");
    }

    /// SIGINT ends the session as `ok` with an end time, and releases the
    /// directory (no watcher left in `--status`).
    #[test]
    fn sigint_finalizes_the_session_cleanly() {
        let fx = Fixture::new();
        let w = Watcher::start(&fx, &fx.work());
        let (status, _) = w.finish("INT");
        assert!(status.success());

        let sessions = fx.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["status"], "ok");
        assert!(sessions[0]["ended_at"].is_i64(), "{:?}", sessions[0]);
        assert_eq!(sessions[0]["exit_code"], serde_json::Value::Null);

        let out = fx.hh().args(["watch", "--status"]).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "no watchers running — start one with `hh watch [path]`\n"
        );
    }

    /// A second `hh watch` on a directory that is already watched reports
    /// the running session and exits 0 without creating another one.
    #[test]
    fn a_second_watch_on_the_same_directory_reports_the_first() {
        let fx = Fixture::new();
        let work = fx.work();
        let w = Watcher::start(&fx, &work);
        let short = w
            .ready
            .split("session ")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap()
            .to_string();

        let out = fx.hh().arg("watch").arg(&work).output().unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.starts_with("● Already watching "), "{stdout}");
        assert!(stdout.contains(&format!("session {short}")), "{stdout}");
        assert_eq!(fx.sessions().len(), 1, "no second session");

        // The same directory reached through a different spelling is still
        // the same directory.
        let dotted = work.join(".");
        let out = fx.hh().arg("watch").arg(&dotted).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("Already watching"));

        let (status, _) = w.finish("INT");
        assert!(status.success());
        assert_eq!(fx.sessions().len(), 1);
    }

    /// `--daemon` detaches; `--status` lists it; `--stop` finalizes it.
    #[test]
    fn daemon_status_and_stop() {
        let fx = Fixture::new();
        let work = fx.work();
        let out = fx
            .hh()
            .arg("watch")
            .arg(&work)
            .arg("--daemon")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let started = String::from_utf8_lossy(&out.stdout);
        assert!(started.contains("in the background"), "{started}");

        let out = fx.hh().args(["watch", "--status"]).output().unwrap();
        let status = String::from_utf8_lossy(&out.stdout);
        assert!(status.starts_with("PATH "), "{status}");
        assert!(status.contains(&*work.to_string_lossy()), "{status}");
        assert!(status.contains("background"), "{status}");

        std::fs::write(work.join("made-while-detached.txt"), "x\n").unwrap();

        let out = fx
            .hh()
            .arg("watch")
            .arg(&work)
            .arg("--stop")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stopped = String::from_utf8_lossy(&out.stdout);
        assert!(stopped.starts_with("✓ Stopped watching "), "{stopped}");
        assert!(stopped.contains("1 files changed"), "{stopped}");

        let sessions = fx.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["status"], "ok");
        assert_eq!(sessions[0]["files_changed"], 1);

        // Stopping again is a no-op, not an error.
        let out = fx
            .hh()
            .arg("watch")
            .arg(&work)
            .arg("--stop")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("No watcher running"));
    }

    /// A watcher killed outright (no chance to finalize) is recovered by the
    /// next `hh` invocation: its session is marked interrupted, its lock is
    /// cleared, and the directory can be watched again.
    #[test]
    fn a_crashed_watcher_is_recovered_on_the_next_invocation() {
        let fx = Fixture::new();
        let work = fx.work();
        let w = Watcher::start(&fx, &work);
        let (status, _) = w.finish("KILL");
        assert!(!status.success());

        let sessions = fx.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["status"], "interrupted");

        let out = fx.hh().args(["watch", "--status"]).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("no watchers running"));

        let again = Watcher::start(&fx, &work);
        let (status, _) = again.finish("INT");
        assert!(status.success());
        assert_eq!(fx.sessions().len(), 2);
    }
}

/// Watching `$HOME` is refused as a usage error (exit 2), before anything
/// is recorded.
#[test]
fn watching_home_is_refused() {
    let fx = Fixture::new();
    let home = std::fs::canonicalize(fx.home.path()).unwrap();
    let out = fx.hh().arg("watch").arg(&home).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("refusing to watch"), "{stderr}");
    assert!(stderr.contains("it is your home directory"), "{stderr}");
    assert!(stderr.contains("hint:"), "{stderr}");
    assert_eq!(fx.sessions().len(), 0);

    // An ancestor of home holds all of it too.
    let parent = home.parent().unwrap();
    let out = fx.hh().arg("watch").arg(parent).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("contains your home directory"));
}

/// A filesystem root is refused (exit 2).
#[test]
fn watching_a_filesystem_root_is_refused() {
    let fx = Fixture::new();
    let out = fx.hh().args(["watch", "/"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("it is a filesystem root"));
}

/// A path that is missing or not a directory is a usage error (exit 2).
#[test]
fn watching_a_missing_path_or_a_file_is_a_usage_error() {
    let fx = Fixture::new();
    let missing = fx.work().join("nope");
    let out = fx.hh().arg("watch").arg(&missing).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let file = fx.work().join("file.txt");
    std::fs::write(&file, "x").unwrap();
    let out = fx.hh().arg("watch").arg(&file).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a directory"));
}

/// `--stop` / `--daemon` explain themselves on Windows instead of failing
/// obscurely.
#[cfg(windows)] // The Unix build supports both flags; see the `unix` module.
#[test]
fn daemon_and_stop_are_explained_on_windows() {
    let fx = Fixture::new();
    for flag in ["--daemon", "--stop"] {
        let out = fx
            .hh()
            .arg("watch")
            .arg(fx.work())
            .arg(flag)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("not available on Windows yet"));
    }
}
