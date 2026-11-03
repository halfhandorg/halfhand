//! Effects-only recording: watch a directory with no wrapped command.
//!
//! `hh watch` records what *something else* does to a directory — a GUI
//! agent, or an agent session that is already running — without launching it
//! as a child of `hh`. It is `hh run` minus the PTY: the same session row, the
//! same single writer task, the same filesystem watcher
//! ([`crate::watcher::spawn_watcher`]) with the same session-start baseline,
//! and the same finalize path ([`crate::runner::finalize`]). Nothing here is a
//! second implementation of any of those.
//!
//! The session is marked with capture tier
//! [`hh_core::CAPTURE_TIER_EFFECTS`] at creation and records an empty
//! command. It ends when the caller's stop flag is set (SIGINT/SIGTERM via
//! [`register_stop_signals`], or a test) or, when configured, after an idle
//! timeout with no filesystem activity. Either way it finalizes as `ok`:
//! stopping is the normal way a watch ends, not an interruption.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hh_core::event::{AdapterStatus, AgentKind, NewSession};
use hh_core::store::Store;

use crate::runner::RunOutcome;
use crate::watcher::{spawn_watcher, WatchOptions};

/// How often the watch loop re-checks its stop flag and idle clock. Short
/// enough that Ctrl-C feels immediate; the actual capture happens on the
/// watcher thread, so this costs nothing in fidelity.
const STOP_POLL: Duration = Duration::from_millis(50);

/// Options for one `hh watch` session.
#[derive(Debug, Clone)]
pub struct WatchSessionOptions {
    /// The directory to record. The caller has already validated and
    /// canonicalized it (and refused `$HOME` or a filesystem root).
    pub dir: PathBuf,
    /// Max file size for capture (bytes).
    pub max_file_size: u64,
    /// Store binary file contents (default off).
    pub record_binary: bool,
    /// Extra ignore patterns for the watcher.
    pub extra_ignore: Vec<String>,
    /// Total bytes of pre-existing content captured at session start as
    /// before-sides (`[record] baseline_budget`).
    pub baseline_budget: u64,
    /// Absolute halfhand-owned paths under `dir` to exclude (db, blobs, data
    /// dir), so the recorder never records itself.
    pub internal_exclude: Vec<PathBuf>,
    /// `hh` version string.
    pub hh_version: String,
    /// Record-time redactor, exactly as for `hh run`.
    pub redactor: Option<Arc<hh_core::redact::Detectors>>,
    /// End the session after this long with no filesystem activity under
    /// `dir`. `None` (the default) never times out.
    pub idle_timeout: Option<Duration>,
}

/// Identifies a watch session once it is observing. Handed to the caller's
/// `on_started` callback, after the baseline is taken: any change made after
/// the callback runs is recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchStarted {
    /// Full session UUID string.
    pub session_id: String,
    /// 6-hex-char short id.
    pub short_id: String,
}

/// Why a watch session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchStop {
    /// The stop flag was set: Ctrl-C, SIGTERM (`hh watch --stop`), or the
    /// caller.
    Requested,
    /// No filesystem activity for the configured idle timeout.
    Idle,
}

/// The outcome of a finished watch session.
#[derive(Debug, Clone)]
pub struct WatchOutcome {
    /// The same shape `hh run` returns, so the binary prints one epilogue:
    /// `exit_code` is `None`, `status` is `ok`, the agent kind is generic and
    /// no adapter ran.
    pub run: RunOutcome,
    /// What ended the session.
    pub stopped_by: WatchStop,
}

/// Register SIGINT and SIGTERM to set `stop`. Best-effort, like `hh run`: a
/// signal that cannot be registered is simply not handled.
pub fn register_stop_signals(stop: &Arc<AtomicBool>) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    let _ = signal_hook::flag::register(SIGINT, Arc::clone(stop));
    let _ = signal_hook::flag::register(SIGTERM, Arc::clone(stop));
}

/// Ignore SIGHUP, so a detached watcher survives the terminal that started
/// it closing. Registering any handler replaces the default action
/// (terminate); the flag itself is never read.
#[cfg(unix)] // SIGHUP has no Windows equivalent; detaching is Unix-only.
pub fn ignore_hangup() {
    let _ = signal_hook::flag::register(
        signal_hook::consts::SIGHUP,
        Arc::new(AtomicBool::new(false)),
    );
}

/// Record an effects-only session for `opts.dir` until `stop` is set or the
/// idle timeout passes, then finalize it.
///
/// `on_started` runs once, on the calling thread, after the session row
/// exists and the watcher has taken its baseline — the moment from which
/// every change is recorded. The binary uses it to write its lock file and
/// print the "watching" line; tests use it as a latch.
///
/// # Errors
///
/// A storage failure creating or finalizing the session, or a watcher that
/// cannot observe `opts.dir` at all ([`crate::RecordError::Watch`]; the
/// session is then finalized as `error` so it never lingers as
/// `recording`).
pub fn watch(
    store: &Store,
    opts: &WatchSessionOptions,
    stop: &AtomicBool,
    on_started: impl FnOnce(&WatchStarted),
) -> crate::Result<WatchOutcome> {
    let start = Instant::now();
    let started_at = crate::runner::now_unix_ms();
    let git = crate::git::GitMeta::capture(&opts.dir);
    let created = store.create_session(&NewSession {
        id: hh_core::event::now_v7(),
        started_at,
        agent_kind: AgentKind::Generic,
        adapter_status: AdapterStatus::None,
        command: Vec::new(),
        cwd: opts.dir.clone(),
        hostname: crate::runner::hostname(),
        hh_version: opts.hh_version.clone(),
        model: None,
        git_branch: git.branch,
        git_sha: git.sha,
        git_dirty: git.dirty,
    })?;
    let session_id = created.id;
    let short_id = created.short_id;
    store.set_session_capture_tier(&session_id, hh_core::CAPTURE_TIER_EFFECTS)?;

    // One writer task for this session, fed through the same shared handle
    // `hh run` uses; the store's own connection stays on this thread.
    let writer = Arc::new(Mutex::new(
        store.event_writer_with_redactor(opts.redactor.clone())?,
    ));
    let blobs = Arc::new(crate::make_blob_store(store, opts.redactor.clone()));
    let watcher = spawn_watcher(
        WatchOptions {
            cwd: opts.dir.clone(),
            max_file_size: opts.max_file_size,
            record_binary: opts.record_binary,
            extra_ignore: opts.extra_ignore.clone(),
            internal_exclude: opts.internal_exclude.clone(),
            baseline_budget: opts.baseline_budget,
        },
        Arc::clone(&writer),
        Arc::clone(&blobs),
        session_id.clone(),
        start,
    )?;
    let Some(watcher) = watcher else {
        // `hh run` keeps going without file capture because it still has the
        // terminal; a watch session has nothing else, so recording nothing
        // would be silent degradation. Close the session out and say so.
        crate::runner::finalize(store, &session_id, &writer, None, None, "error")?;
        return Err(crate::RecordError::Watch(format!(
            "could not watch {} for file changes (see the warning above); \
             nothing would be recorded — run `hh doctor`",
            opts.dir.display()
        )));
    };

    on_started(&WatchStarted {
        session_id: session_id.clone(),
        short_id: short_id.clone(),
    });

    let stopped_by = loop {
        if stop.load(Ordering::Acquire) {
            break WatchStop::Requested;
        }
        if let Some(limit) = opts.idle_timeout {
            if start.elapsed().saturating_sub(watcher.last_activity()) >= limit {
                break WatchStop::Idle;
            }
        }
        std::thread::sleep(STOP_POLL);
    };

    // The watcher's own shutdown drains late events and rescans for missed
    // changes before it returns, so a change made just before the stop is
    // still recorded.
    watcher.stop_and_join();
    let (event_count, steps, files_changed, dropped) =
        crate::runner::finalize(store, &session_id, &writer, None, None, "ok")?;
    Ok(WatchOutcome {
        run: RunOutcome {
            session_id,
            short_id,
            exit_code: None,
            duration_ms: i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX),
            event_count,
            steps,
            files_changed,
            status: "ok",
            agent_kind: AgentKind::Generic,
            adapter_status: AdapterStatus::None,
            degrade_reason: None,
            dropped,
        },
        stopped_by,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hh_core::event::{EventKind, SessionStatus};

    fn opts(dir: PathBuf, idle_timeout: Option<Duration>) -> WatchSessionOptions {
        WatchSessionOptions {
            dir,
            max_file_size: 4 * 1024 * 1024,
            record_binary: false,
            extra_ignore: Vec::new(),
            baseline_budget: hh_core::DEFAULT_BASELINE_BUDGET,
            internal_exclude: Vec::new(),
            hh_version: "test".into(),
            redactor: None,
            idle_timeout,
        }
    }

    /// A change made by another thread after `on_started` is recorded, and a
    /// requested stop finalizes the session as an effects-only `ok` session
    /// with no command. Synchronized by channels, not sleeps: the watch runs
    /// on a worker thread, reports "started" over one channel, and the test
    /// sets the stop flag only after its write has completed.
    #[test]
    fn records_a_change_and_finalizes_on_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("work");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("before.txt"), "old\n").unwrap();
        let db = tmp.path().join("hh.db");
        let blobs = tmp.path().join("blobs");

        let stop = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker = {
            let (dir, db, blobs, stop) = (dir.clone(), db.clone(), blobs.clone(), stop.clone());
            std::thread::spawn(move || {
                // The store (and its connection) lives on this thread only.
                let store = Store::open(&db, &blobs).unwrap();
                watch(&store, &opts(dir, None), &stop, |s| {
                    started_tx.send(s.clone()).unwrap();
                })
                .unwrap()
            })
        };
        let started = started_rx.recv().unwrap();
        std::fs::write(dir.join("before.txt"), "new\n").unwrap();
        std::fs::write(dir.join("added.txt"), "hello\n").unwrap();
        stop.store(true, Ordering::Release);
        let outcome = worker.join().unwrap();

        assert_eq!(outcome.stopped_by, WatchStop::Requested);
        assert_eq!(outcome.run.session_id, started.session_id);
        assert_eq!(outcome.run.status, "ok");
        assert_eq!(outcome.run.files_changed, 2);

        let store = Store::open(&db, &blobs).unwrap();
        let row = store.get_session(&started.session_id).unwrap();
        assert_eq!(row.status, SessionStatus::Ok);
        assert_eq!(row.command, Vec::<String>::new());
        assert_eq!(row.capture_tier.as_deref(), Some("effects"));
        assert!(row.ended_at.is_some());
        let kinds: Vec<EventKind> = store
            .list_event_index(&started.session_id)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, vec![EventKind::FileChange, EventKind::FileChange]);
    }

    /// With an idle timeout and no activity, the watch ends by itself.
    #[test]
    fn idle_timeout_ends_the_session() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("work");
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let stop = AtomicBool::new(false);
        let outcome = watch(
            &store,
            &opts(dir, Some(Duration::from_millis(200))),
            &stop,
            |_| {},
        )
        .unwrap();
        assert_eq!(outcome.stopped_by, WatchStop::Idle);
        assert_eq!(outcome.run.status, "ok");
        assert_eq!(outcome.run.files_changed, 0);
    }
}
