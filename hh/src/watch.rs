//! `hh watch`: record what happens to a directory without launching the
//! agent that does it.
//!
//! The recording itself is [`hh_record::watch`] — the same session row,
//! writer task, watcher and finalize as `hh run`. This module is the
//! command-line shell around it:
//!
//! - **Which directory.** The path is canonicalized and refused when it is a
//!   filesystem root, the home directory, or an ancestor of it (usage error,
//!   exit 2).
//! - **One watcher per directory.** A lock file under
//!   `<data_dir>/watch/<key>.lock` (key: a hash of the canonical path) holds
//!   the watcher's pid, process start time and session. It is created
//!   atomically (written to a temporary file, then hard-linked into place,
//!   which fails if a lock already exists). An existing lock whose recorder is
//!   provably gone — the same pid + start-time liveness check that marks a
//!   crashed recording `interrupted` — is stale and replaced; a live one is
//!   reported instead of racing it. The lock file doubles as the `--daemon`
//!   PID file.
//! - **`--daemon`** (Unix) re-runs `hh watch <dir>` detached, in its own
//!   process group with SIGHUP ignored, stdin closed and stderr appended to
//!   `<data_dir>/watch/<key>.log`. The parent returns once the child reports
//!   on its stdout that it is observing (or that a watcher already exists).
//! - **`--stop`** (Unix) sends SIGTERM to the recorded pid after re-checking
//!   its identity, then waits for the lock to disappear, which happens only
//!   after the session is finalized.
//! - **`--status`** lists live watchers and quietly removes stale locks.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Context;
use owo_colors::OwoColorize;

use crate::{cli, render, UsageError};

/// Subdirectory of the data dir holding watch lock files and daemon logs.
const WATCH_DIR: &str = "watch";

/// How long `hh watch --stop` waits for the watcher to finalize and release
/// its lock before giving up with an actionable error.
#[cfg(unix)] // `--stop` is Unix-only.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// `hh watch`.
pub(crate) fn watch_command(args: &cli::WatchArgs) -> anyhow::Result<ExitCode> {
    if args.status {
        return status_command(args.path.as_deref());
    }
    if args.stop {
        return stop_command(args.path.as_deref());
    }
    let dir = resolve_watch_dir(args.path.as_deref())?;
    if args.daemon {
        return daemon_command(&dir, args);
    }
    record(&dir, args)
}

// ---------------------------------------------------------------------------
// Which directory
// ---------------------------------------------------------------------------

/// Canonicalize the directory to watch (default: the current directory) and
/// refuse the ones that must never be watched. Every failure is a
/// [`UsageError`] (exit 2): nothing has started yet.
fn resolve_watch_dir(path: Option<&Path>) -> anyhow::Result<PathBuf> {
    let dir = canonical_dir(path)?;
    if let Some(why) = refusal(&dir, home_dir().as_deref()) {
        return Err(anyhow::Error::new(UsageError(format!(
            "refusing to watch {}\n  why: {why} — watching it would exhaust the operating \
             system's file-watch limits and record everything you do, not just the agent's work\n  \
             hint: watch the project directory the agent works in, e.g. `hh watch ~/code/my-project`",
            dir.display()
        ))));
    }
    Ok(dir)
}

/// The canonical form of `path` (default: the current directory), which
/// must be an existing directory.
fn canonical_dir(path: Option<&Path>) -> anyhow::Result<PathBuf> {
    let raw = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().map_err(|e| {
            anyhow::anyhow!(
                "could not determine the current directory\n  why: {e}\n  \
                 hint: pass the directory to watch, e.g. `hh watch ~/code/my-project`"
            )
        })?,
    };
    let dir = std::fs::canonicalize(&raw).map_err(|e| {
        anyhow::Error::new(UsageError(format!(
            "cannot watch {}\n  why: {e}\n  hint: pass an existing directory",
            raw.display()
        )))
    })?;
    if !dir.is_dir() {
        return Err(anyhow::Error::new(UsageError(format!(
            "cannot watch {}\n  why: it is not a directory\n  \
             hint: pass the directory the agent works in, not a file inside it",
            raw.display()
        ))));
    }
    Ok(dir)
}

/// Why `dir` (canonical) must not be watched, or `None` if it may be.
fn refusal(dir: &Path, home: Option<&Path>) -> Option<&'static str> {
    if dir.parent().is_none() {
        return Some("it is a filesystem root");
    }
    let home = home?;
    if dir == home {
        Some("it is your home directory")
    } else if home.starts_with(dir) {
        Some("it contains your home directory")
    } else {
        None
    }
}

/// The user's home directory, canonicalized when possible. Read from the
/// environment (`HOME`; `USERPROFILE` on Windows) so it agrees with what the
/// user's shell calls `~`.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let var = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let var = std::env::var_os("HOME");
    let home = PathBuf::from(var.filter(|v| !v.is_empty())?);
    Some(std::fs::canonicalize(&home).unwrap_or(home))
}

// ---------------------------------------------------------------------------
// The lock file
// ---------------------------------------------------------------------------

/// What a watch lock file records.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LockInfo {
    /// The watching `hh` process.
    pid: u32,
    /// Its platform start-time token (pid-reuse guard).
    start_time: Option<i64>,
    /// The canonical watched directory.
    dir: PathBuf,
    /// The session, once created.
    session_id: Option<String>,
    /// Its short id, once created.
    short_id: Option<String>,
    /// Started by `--daemon`.
    background: bool,
    /// Unix-ms when the watcher started.
    started_at: i64,
}

impl LockInfo {
    fn to_json(&self) -> String {
        serde_json::json!({
            "pid": self.pid,
            "start_time": self.start_time,
            "dir": self.dir.to_string_lossy(),
            "session_id": self.session_id,
            "short_id": self.short_id,
            "background": self.background,
            "started_at": self.started_at,
        })
        .to_string()
    }

    /// Parse a lock file. Anything malformed is `None` (treated as stale):
    /// a lock file is ours to write, so garbage means corruption, never a
    /// live watcher.
    fn parse(text: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let s = |k: &str| {
            v.get(k)
                .and_then(serde_json::Value::as_str)
                .map(String::from)
        };
        Some(Self {
            pid: u32::try_from(v.get("pid")?.as_u64()?).ok()?,
            start_time: v.get("start_time").and_then(serde_json::Value::as_i64),
            dir: PathBuf::from(s("dir")?),
            session_id: s("session_id"),
            short_id: s("short_id"),
            background: v
                .get("background")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            started_at: v.get("started_at").and_then(serde_json::Value::as_i64)?,
        })
    }

    /// Whether the recorder this lock names is provably gone. An
    /// undeterminable answer counts as alive (fail safe).
    fn is_stale(&self) -> bool {
        hh_core::recorder_is_gone(self.pid, self.start_time)
    }
}

fn watch_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(WATCH_DIR)
}

/// A stable, filename-safe key for a canonical directory.
fn lock_key(dir: &Path) -> String {
    hh_core::BlobStore::hash(dir.as_os_str().as_encoded_bytes())[..16].to_string()
}

fn lock_path(data_dir: &Path, dir: &Path) -> PathBuf {
    watch_dir(data_dir).join(format!("{}.lock", lock_key(dir)))
}

/// The background watcher's log for `dir`.
#[cfg(unix)] // Only the Unix daemon writes a log.
fn log_path(data_dir: &Path, dir: &Path) -> PathBuf {
    watch_dir(data_dir).join(format!("{}.log", lock_key(dir)))
}

fn read_lock(path: &Path) -> Option<LockInfo> {
    LockInfo::parse(&std::fs::read_to_string(path).ok()?)
}

/// Remove `path` if it still holds `stale` — re-read first so a lock a
/// newer watcher just took in its place is left alone.
fn remove_if_unchanged(path: &Path, stale: Option<&LockInfo>) {
    if read_lock(path).as_ref() == stale {
        let _ = std::fs::remove_file(path);
    }
}

/// The result of trying to take a directory's lock.
enum Acquire {
    /// This process now watches the directory.
    Acquired(WatchLock),
    /// A live watcher already does.
    Held(LockInfo),
}

/// This process's hold on a directory's lock file. Dropping it removes the
/// file, but only while it still names this process.
struct WatchLock {
    path: PathBuf,
    info: LockInfo,
}

impl WatchLock {
    /// Take `dir`'s lock, replacing a stale one. See the module docs.
    fn acquire(data_dir: &Path, dir: &Path, background: bool) -> anyhow::Result<Acquire> {
        let wdir = watch_dir(data_dir);
        std::fs::create_dir_all(&wdir).with_context(|| {
            format!(
                "could not create {}\n  hint: check permissions on the data directory",
                wdir.display()
            )
        })?;
        let (pid, start_time) = hh_core::current_process_identity();
        let info = LockInfo {
            pid,
            start_time,
            dir: dir.to_path_buf(),
            session_id: None,
            short_id: None,
            background,
            started_at: render::now_unix_ms(),
        };
        let path = lock_path(data_dir, dir);
        // A few attempts: each lost race against a stale lock removes it.
        for _ in 0..3 {
            let tmp = path.with_extension(format!("{pid}.tmp"));
            write_file(&tmp, &info.to_json())?;
            let linked = std::fs::hard_link(&tmp, &path);
            let _ = std::fs::remove_file(&tmp);
            match linked {
                Ok(()) => return Ok(Acquire::Acquired(Self { path, info })),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let existing = read_lock(&path);
                    match existing {
                        Some(held) if !held.is_stale() => return Ok(Acquire::Held(held)),
                        stale => remove_if_unchanged(&path, stale.as_ref()),
                    }
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "could not create the watch lock {}\n  why: {e}\n  \
                         hint: the data directory must be on a filesystem that supports \
                         hard links; set HH_DATA_DIR to a local directory",
                        path.display()
                    ));
                }
            }
        }
        Err(anyhow::anyhow!(
            "could not take the watch lock {}\n  why: it kept changing while hh tried to \
             replace a stale one\n  hint: run `hh watch --status`, then try again",
            path.display()
        ))
    }

    /// Record the session in the lock file, atomically (write + rename).
    fn record_session(&mut self, started: &hh_record::WatchStarted) -> anyhow::Result<()> {
        self.info.session_id = Some(started.session_id.clone());
        self.info.short_id = Some(started.short_id.clone());
        let tmp = self.path.with_extension(format!("{}.tmp", self.info.pid));
        write_file(&tmp, &self.info.to_json())?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("could not update {}", self.path.display()))
    }
}

impl Drop for WatchLock {
    fn drop(&mut self) {
        if read_lock(&self.path).is_some_and(|l| l.pid == self.info.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn write_file(path: &Path, text: &str) -> anyhow::Result<()> {
    let mut f = std::fs::File::create(path)
        .with_context(|| format!("could not write {}", path.display()))?;
    f.write_all(text.as_bytes())
        .and_then(|()| f.sync_all())
        .with_context(|| format!("could not write {}", path.display()))
}

/// Every live watcher's lock, oldest first. Stale locks (their watcher
/// crashed) are removed on the way: the crashed session itself is marked
/// `interrupted` by the store's own stale-session check.
fn live_locks(data_dir: &Path) -> Vec<LockInfo> {
    let Ok(entries) = std::fs::read_dir(watch_dir(data_dir)) else {
        return Vec::new();
    };
    let mut live = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lock") {
            continue;
        }
        match read_lock(&path) {
            Some(info) if !info.is_stale() => live.push(info),
            stale => remove_if_unchanged(&path, stale.as_ref()),
        }
    }
    live.sort_by_key(|l| l.started_at);
    live
}

/// Remove lock files left by crashed watchers. Called on every store open,
/// so a crash is cleaned up by the next `hh` invocation of any kind.
/// Best-effort and silent.
pub(crate) fn sweep_stale_locks(data_dir: &Path) {
    let _ = live_locks(data_dir);
}

// ---------------------------------------------------------------------------
// Recording (foreground, or the detached child of --daemon)
// ---------------------------------------------------------------------------

/// Record `dir` in this process until stopped.
fn record(dir: &Path, args: &cli::WatchArgs) -> anyhow::Result<ExitCode> {
    let background = args.daemon_child;
    let (store, paths, config) = crate::open_store()?;
    let mut lock = match WatchLock::acquire(&paths.data_dir, dir, background)? {
        Acquire::Held(held) => {
            say(&already_line(&held, render::use_color()));
            return Ok(ExitCode::SUCCESS);
        }
        Acquire::Acquired(lock) => lock,
    };

    let stop = Arc::new(AtomicBool::new(false));
    hh_record::register_stop_signals(&stop);
    #[cfg(unix)] // SIGHUP has no Windows equivalent; detaching is Unix-only.
    if background {
        hh_record::ignore_hangup();
    }

    let opts = hh_record::WatchSessionOptions {
        dir: dir.to_path_buf(),
        max_file_size: config.record.max_file_size,
        record_binary: config.record.record_binary,
        extra_ignore: config.record.ignore.clone(),
        baseline_budget: config.record.baseline_budget,
        internal_exclude: crate::paths_under(
            dir,
            &[&paths.db_path, &paths.blobs_dir, &paths.data_dir],
        ),
        hh_version: crate::HH_VERSION.to_string(),
        redactor: crate::record_redactor(&config)?,
        idle_timeout: args.idle_timeout,
    };
    let mut lock_error = None;
    let outcome = hh_record::watch(&store, &opts, &stop, |started| {
        if let Err(e) = lock.record_session(started) {
            lock_error = Some(e);
        }
        if let Some(name) = args.name.as_deref() {
            if let Err(e) = store.set_session_name(&started.session_id, Some(name)) {
                eprintln!(
                    "hh: warning: could not name session {sid}: {e}; \
                     run `hh rename {sid} \"{name}\"` to retry",
                    sid = started.short_id
                );
            }
        }
        say(&started_line(
            dir,
            &started.short_id,
            background.then_some(lock.info.pid),
            render::use_color(),
        ));
    });
    if let Some(e) = lock_error {
        eprintln!(
            "hh: warning: {e:#}; recording continues, but `hh watch --status` and `--stop` \
             will not name this session — run `hh list` once it ends"
        );
    }
    let outcome = outcome.map_err(|e| {
        anyhow::anyhow!(
            "watching {} failed\n  why: {e}\n  hint: run `hh list` to see what was recorded",
            dir.display()
        )
    })?;
    // The lock goes only now, after finalize: `hh watch --stop` waits for it
    // to disappear, so it must outlive the recording.
    drop(lock);

    let idle_note = (outcome.stopped_by == hh_record::WatchStop::Idle).then(|| {
        format!(
            "hh: stopped after {} with no file activity (--idle-timeout)",
            render::humanize_ms(
                i64::try_from(args.idle_timeout.unwrap_or_default().as_millis())
                    .unwrap_or(i64::MAX)
            )
        )
    });
    if background {
        // stdout belonged to the parent, which has exited; the log is ours.
        eprintln!("{}", crate::format_epilogue_line(&outcome.run, false));
        if let Some(note) = idle_note {
            eprintln!("{note}");
        }
    } else {
        if let Some(note) = idle_note {
            eprintln!("{note}");
        }
        crate::print_epilogue(&outcome.run);
    }
    Ok(ExitCode::SUCCESS)
}

/// Print one line to stdout, ignoring a closed pipe: the detached child's
/// stdout is a pipe to a parent that exits as soon as it has read the line.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The line printed once the watcher is observing.
fn started_line(dir: &Path, short_id: &str, background_pid: Option<u32>, color: bool) -> String {
    let dot = accent("●", color);
    let sid = accent(short_id, color);
    match background_pid {
        Some(pid) => format!(
            "{dot} Watching {} in the background · session {sid} · pid {pid} · \
             stop with `hh watch --stop`",
            dir.display()
        ),
        None => format!(
            "{dot} Watching {} · session {sid} · Ctrl-C or `hh watch --stop` to finish",
            dir.display()
        ),
    }
}

/// The line printed when a watcher already holds the directory.
fn already_line(held: &LockInfo, color: bool) -> String {
    let dot = accent("●", color);
    let session = held.short_id.as_deref().map_or_else(
        || "session starting".to_string(),
        |s| format!("session {}", accent(s, color)),
    );
    format!(
        "{dot} Already watching {} · {session} · pid {} · `hh watch --stop` to finish it",
        held.dir.display(),
        held.pid
    )
}

fn accent(s: &str, color: bool) -> String {
    if color {
        s.green().to_string()
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// --daemon
// ---------------------------------------------------------------------------

#[cfg(not(unix))] // Detaching relies on Unix process groups and signals.
fn daemon_command(_dir: &Path, _args: &cli::WatchArgs) -> anyhow::Result<ExitCode> {
    Err(not_on_windows("--daemon"))
}

#[cfg(not(unix))] // See `daemon_command`.
fn not_on_windows(flag: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`hh watch {flag}` is not available on Windows yet\n  \
         why: detaching and stopping a watcher rely on Unix process groups and signals\n  \
         hint: run `hh watch` in its own terminal and press Ctrl-C to finish the session"
    )
}

/// Start the detached child and relay its first line.
#[cfg(unix)] // Detaching relies on Unix process groups and signals.
fn daemon_command(dir: &Path, args: &cli::WatchArgs) -> anyhow::Result<ExitCode> {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let (_store, paths, _config) = crate::open_store()?;
    let wdir = watch_dir(&paths.data_dir);
    std::fs::create_dir_all(&wdir).with_context(|| {
        format!(
            "could not create {}\n  hint: check permissions on the data directory",
            wdir.display()
        )
    })?;
    let log = log_path(&paths.data_dir, dir);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("could not open the watcher log {}", log.display()))?;
    let exe = std::env::current_exe().context(
        "could not find the hh executable to start the background watcher\n  \
         hint: run `hh watch` without --daemon in its own terminal",
    )?;
    let mut cmd = Command::new(exe);
    cmd.arg("watch").arg(dir).arg("--daemon-child");
    if let Some(t) = args.idle_timeout {
        cmd.arg(format!("--idle-timeout={}s", t.as_secs()));
    }
    if let Some(name) = &args.name {
        cmd.arg(format!("--name={name}"));
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log_file))
        // Its own process group: a Ctrl-C in this terminal does not reach it.
        .process_group(0)
        .spawn()
        .context("could not start the background watcher")?;
    let mut line = String::new();
    if let Some(out) = child.stdout.take() {
        let _ = std::io::BufReader::new(out).read_line(&mut line);
    }
    let line = line.trim_end();
    if line.is_empty() {
        // The child exited (or closed stdout) before it was observing.
        let _ = child.wait();
        return Err(anyhow::anyhow!(
            "the background watcher for {} did not start\n  why: {}\n  \
             hint: run `hh watch` without --daemon to see the error directly",
            dir.display(),
            last_log_line(&log).unwrap_or_else(|| format!("see {}", log.display()))
        ));
    }
    println!("{line}");
    Ok(ExitCode::SUCCESS)
}

#[cfg(unix)] // Only the Unix daemon writes a log.
fn last_log_line(log: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log).ok()?;
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim_start_matches("✗ ").to_string())
}

// ---------------------------------------------------------------------------
// --stop
// ---------------------------------------------------------------------------

#[cfg(not(unix))] // Stopping another process cleanly needs a Unix signal.
fn stop_command(_path: Option<&Path>) -> anyhow::Result<ExitCode> {
    Err(not_on_windows("--stop"))
}

/// Ask the watcher for a directory to finish, and wait until it has.
#[cfg(unix)] // Stopping another process cleanly needs a Unix signal.
fn stop_command(path: Option<&Path>) -> anyhow::Result<ExitCode> {
    let dir = canonical_dir(path)?;
    let (store, paths, _config) = crate::open_store()?;
    let lock = lock_path(&paths.data_dir, &dir);
    let held = match read_lock(&lock) {
        Some(h) if !h.is_stale() => h,
        stale => {
            remove_if_unchanged(&lock, stale.as_ref());
            println!(
                "{} No watcher running for {}",
                accent("●", render::use_color()),
                dir.display()
            );
            return Ok(ExitCode::SUCCESS);
        }
    };
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(held.pid.to_string())
        .status();
    if !status.is_ok_and(|s| s.success()) {
        return Err(anyhow::anyhow!(
            "could not signal the watcher (pid {}) for {}\n  \
             why: `kill -TERM {}` failed\n  hint: it may belong to another user; stop it there",
            held.pid,
            dir.display(),
            held.pid
        ));
    }
    let deadline = std::time::Instant::now() + STOP_WAIT;
    while read_lock(&lock).is_some_and(|l| l.pid == held.pid && !l.is_stale()) {
        if std::time::Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "the watcher (pid {}) for {} did not finish within {}s\n  \
                 hint: it may still be saving; run `hh watch --status` to check",
                held.pid,
                dir.display(),
                STOP_WAIT.as_secs()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let color = render::use_color();
    let summary = held
        .session_id
        .as_deref()
        .and_then(|sid| store.get_session(sid).ok())
        .map_or_else(String::new, |row| {
            let dur = row
                .duration_ms()
                .map_or_else(|| "—".to_string(), render::humanize_ms);
            format!(
                " · session {sid} · {dur} · {steps} steps · {files} files changed · hh replay {sid}",
                sid = accent(&row.short_id, color),
                steps = row.step_count,
                files = row.files_changed,
            )
        });
    let check = if color {
        "✓".green().to_string()
    } else {
        "✓".to_string()
    };
    println!("{check} Stopped watching {}{summary}", dir.display());
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// --status
// ---------------------------------------------------------------------------

/// One row of `hh watch --status`.
#[derive(Debug, Clone)]
struct StatusRow {
    dir: PathBuf,
    short_id: Option<String>,
    pid: u32,
    background: bool,
    started_at: i64,
    files_changed: Option<i64>,
}

fn status_command(path: Option<&Path>) -> anyhow::Result<ExitCode> {
    let only = match path {
        Some(_) => Some(canonical_dir(path)?),
        None => None,
    };
    let (store, paths, _config) = crate::open_store()?;
    let rows: Vec<StatusRow> = live_locks(&paths.data_dir)
        .into_iter()
        .filter(|l| only.as_ref().is_none_or(|d| *d == l.dir))
        .map(|l| StatusRow {
            files_changed: l
                .session_id
                .as_deref()
                .and_then(|sid| store.session_stats(sid).ok())
                .map(|(_, files)| files),
            dir: l.dir,
            short_id: l.short_id,
            pid: l.pid,
            background: l.background,
            started_at: l.started_at,
        })
        .collect();
    print!(
        "{}",
        format_status(&rows, render::use_color(), render::now_unix_ms())
    );
    Ok(ExitCode::SUCCESS)
}

/// Render `hh watch --status`: an aligned table like `hh list`, or one line
/// saying nothing is running. Pure, for the snapshot test.
fn format_status(rows: &[StatusRow], color: bool, now: i64) -> String {
    if rows.is_empty() {
        return "no watchers running — start one with `hh watch [path]`\n".to_string();
    }
    let headers = ["PATH", "SESSION", "PID", "MODE", "STARTED", "FILES"];
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|r| {
            [
                r.dir.display().to_string(),
                r.short_id.clone().unwrap_or_else(|| "—".to_string()),
                r.pid.to_string(),
                if r.background {
                    "background"
                } else {
                    "foreground"
                }
                .to_string(),
                render::humanize_relative(r.started_at, now),
                r.files_changed
                    .map_or_else(|| "—".to_string(), |n| n.to_string()),
            ]
        })
        .collect();
    let mut widths = headers.map(|h| h.chars().count());
    for row in &cells {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    let last = headers.len() - 1;
    let mut out = String::new();
    let push_row = |out: &mut String, row: &[String]| {
        for (i, c) in row.iter().enumerate() {
            let shown = if i == 1 && color && c != "—" {
                c.green().to_string()
            } else {
                c.clone()
            };
            out.push_str(&shown);
            if i != last {
                out.extend(std::iter::repeat_n(' ', widths[i] - c.chars().count() + 1));
            }
        }
        out.push('\n');
    };
    push_row(&mut out, &headers.map(String::from));
    push_row(&mut out, &widths.map(|w| "-".repeat(w)));
    for row in &cells {
        push_row(&mut out, row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_roots_home_and_its_ancestors() {
        let home = Path::new("/home/me");
        assert_eq!(
            refusal(Path::new("/"), Some(home)),
            Some("it is a filesystem root")
        );
        assert_eq!(
            refusal(Path::new("/home/me"), Some(home)),
            Some("it is your home directory")
        );
        assert_eq!(
            refusal(Path::new("/home"), Some(home)),
            Some("it contains your home directory")
        );
        assert_eq!(refusal(Path::new("/home/me/code/site"), Some(home)), None);
        assert_eq!(refusal(Path::new("/home/meow"), Some(home)), None);
        assert_eq!(refusal(Path::new("/srv/site"), None), None);
    }

    #[test]
    fn lock_info_round_trips_and_rejects_garbage() {
        let info = LockInfo {
            pid: 4242,
            start_time: Some(99),
            dir: PathBuf::from("/srv/site"),
            session_id: Some("0190".into()),
            short_id: Some("a1b2c3".into()),
            background: true,
            started_at: 1_700_000_000_000,
        };
        assert_eq!(LockInfo::parse(&info.to_json()), Some(info));
        for bad in ["", "{", "[]", "{\"pid\":-1}", "{\"pid\":1,\"dir\":\"/x\"}"] {
            assert_eq!(LockInfo::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_live_lock_is_held_and_a_stale_one_is_replaced() {
        let data = tempfile::tempdir().unwrap();
        let dir = Path::new("/srv/site");
        let Acquire::Acquired(first) = WatchLock::acquire(data.path(), dir, false).unwrap() else {
            panic!("first acquire must succeed");
        };
        // Same process, so its recorder is alive: the second attempt sees it.
        let Acquire::Held(held) = WatchLock::acquire(data.path(), dir, false).unwrap() else {
            panic!("second acquire must report the live watcher");
        };
        assert_eq!(held.pid, std::process::id());
        drop(first);
        assert!(!lock_path(data.path(), dir).exists(), "drop releases");

        // A lock naming a process that is provably gone is replaced.
        let (pid, start) = hh_core::current_process_identity();
        let stale = LockInfo {
            pid,
            start_time: start.map(|s| s + 1),
            dir: dir.to_path_buf(),
            session_id: None,
            short_id: None,
            background: false,
            started_at: 0,
        };
        std::fs::create_dir_all(watch_dir(data.path())).unwrap();
        std::fs::write(lock_path(data.path(), dir), stale.to_json()).unwrap();
        assert!(matches!(
            WatchLock::acquire(data.path(), dir, false).unwrap(),
            Acquire::Acquired(_)
        ));
    }

    fn status_rows() -> Vec<StatusRow> {
        vec![
            StatusRow {
                dir: PathBuf::from("/home/me/code/site"),
                short_id: Some("a1b2c3".into()),
                pid: 4242,
                background: true,
                started_at: 1_700_000_000_000 - 5 * 60_000,
                files_changed: Some(3),
            },
            StatusRow {
                dir: PathBuf::from("/srv/notes"),
                short_id: None,
                pid: 77,
                background: false,
                started_at: 1_700_000_000_000 - 2 * 3_600_000,
                files_changed: None,
            },
        ]
    }

    #[test]
    fn snapshot_watch_status() {
        insta::assert_snapshot!(format_status(&status_rows(), false, 1_700_000_000_000));
    }

    #[test]
    fn snapshot_watch_status_nothing_running() {
        insta::assert_snapshot!(format_status(&[], false, 0));
    }

    #[test]
    fn status_columns_stay_aligned_under_color() {
        let plain = format_status(&status_rows(), false, 1_700_000_000_000);
        let colored = format_status(&status_rows(), true, 1_700_000_000_000);
        let strip = |s: &str| s.lines().map(render::visible_width).collect::<Vec<usize>>();
        assert_eq!(strip(&plain), strip(&colored));
    }

    #[test]
    fn started_and_already_lines_read_plainly() {
        let dir = Path::new("/srv/site");
        assert_eq!(
            started_line(dir, "a1b2c3", None, false),
            "● Watching /srv/site · session a1b2c3 · Ctrl-C or `hh watch --stop` to finish"
        );
        assert_eq!(
            started_line(dir, "a1b2c3", Some(42), false),
            "● Watching /srv/site in the background · session a1b2c3 · pid 42 · \
             stop with `hh watch --stop`"
        );
        let held = LockInfo {
            pid: 42,
            start_time: None,
            dir: dir.to_path_buf(),
            session_id: None,
            short_id: None,
            background: true,
            started_at: 0,
        };
        assert_eq!(
            already_line(&held, false),
            "● Already watching /srv/site · session starting · pid 42 · \
             `hh watch --stop` to finish it"
        );
    }
}
