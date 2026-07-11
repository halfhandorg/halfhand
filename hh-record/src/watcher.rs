//! Recursive filesystem watcher (FR-1.4).
//!
//! Spawns a `notify` watcher on the session cwd and records `file_change`
//! events through the shared writer. Honors a built-in ignore list, the
//! root `.gitignore`, and caller-supplied extra patterns. File contents are
//! stored in the blob store (BLAKE3-keyed, zstd); binary files record hashes
//! only unless `--record-binary` is set.
//!
//! # SRS deviation (flagged): nested `.gitignore`
//!
//! FR-1.4 says "honoring `.gitignore`". This implementation honors the cwd's
//! root `.gitignore` and the global gitignore, plus built-in + extra patterns,
//! via a single `ignore::gitignore::Gitignore` matcher. It does **not** honor
//! `.gitignore` files in subdirectories (the `ignore` crate does not expose a
//! public path-based matcher for the full directory-ignore tree without
//! `WalkBuilder`, which is walk-oriented, not event-oriented). For v0.1 this
//! covers the common case (one root `.gitignore`); nested ignores are a
//! roadmap refinement. See the decisions summary.
//!
//! A one-time recursive scan of `cwd` runs before the event loop starts, to
//! give [`resolve_first_seen`] a baseline of pre-existing files — see its
//! docs for why (a macOS FSEvents quirk misreports edits to pre-existing
//! files as creations otherwise).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hh_core::blob::BlobStore;
use hh_core::event::{ChangeKind, Event, EventKind, FileChange};
use hh_core::store::EventWriter;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Built-in ignore patterns (FR-1.4: `.git/`, `node_modules/`, `target/`).
const BUILTIN_IGNORE: &[&str] = &[".git/", "node_modules/", "target/"];

/// Panic-injection flag for the panic-hygiene test (`tests::mod
/// panic_hygiene`). See the check in [`run_loop`]. Test-only: compiled out of
/// every non-test build.
#[cfg(test)]
static INJECT_PANIC_FOR_TEST: AtomicBool = AtomicBool::new(false);

/// Serializes tests that run a real [`run_loop`] on a worker thread. Test-only.
///
/// [`INJECT_PANIC_FOR_TEST`] is a process-global `static`, so a worker spawned
/// by one test would observe a flag armed by a *different* concurrently-running
/// test and panic at the wrong time — aborting before its own shutdown backstop
/// runs and turning that test into a flaky "0 events" failure (the macOS-CI
/// `events seen: []` flake in `watcher_captures_write_then_immediate_stop`).
/// Tests that spawn a real long-running watcher take this mutex for their
/// duration so the injection flag is only ever seen by the worker it was meant
/// for. `const`-constructible since Rust 1.63, so fine at the 1.75 MSRV.
#[cfg(test)]
static REAL_RUN_LOOP_MUTEX: Mutex<()> = Mutex::new(());

/// Coalescing window for editor-style double-writes (FR-1.4). Editors save a
/// file as a burst of filesystem events within tens of milliseconds — e.g.
/// write-content then touch-mtime, or write-then-rename-into-place producing
/// two Modify events on the final path. A 100 ms window merges such a burst
/// into a single `file_change` event while staying well below the cadence of a
/// human's distinct saves (seconds apart). 100 ms also matches a familiar
/// typing-debounce and is too short to visibly delay recording.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(100);

/// How often the worker loop wakes to flush due pending events. Kept well
/// below [`DEBOUNCE_WINDOW`] so a pending change is processed promptly once its
/// window elapses.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Shutdown grace drain: once `stop` is signaled (the child has exited), keep
/// receiving filesystem events until the channel is quiet for this long with
/// no arriving event, then stop. Catches in-flight events from backends that
/// deliver asynchronously — notably macOS FSEvents, which coalesces deliveries
/// through a daemon and can report a write AFTER the process that performed it
/// has already exited. A prompt backend (Linux inotify) typically has nothing
/// left to deliver, so it pays only this short quiet wait. See [`run_loop`].
const GRACE_QUIET: Duration = Duration::from_millis(150);

/// Hard cap on the shutdown grace drain ([`GRACE_QUIET`]). Bounds the extra
/// latency a slow-delivering backend can add to `hh run` shutdown after the
/// child has exited. The drain ends at [`GRACE_QUIET`] of silence in the
/// common case, so this cap only bites a pathologically busy directory still
/// emitting events long after the child is gone.
const GRACE_MAX: Duration = Duration::from_secs(1);

/// Options for the filesystem watcher (FR-1.4).
#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Directory to watch recursively.
    pub cwd: PathBuf,
    /// Max file size to capture (bytes); larger files are skipped.
    pub max_file_size: u64,
    /// Whether to store binary file contents (default off).
    pub record_binary: bool,
    /// Extra ignore patterns extending built-in + `.gitignore`.
    pub extra_ignore: Vec<String>,
    /// Absolute paths under cwd to exclude (the halfhand data dir / db /
    /// blobs when the agent records into its own data dir). Matched by
    /// prefix so a whole tree can be excluded.
    pub internal_exclude: Vec<PathBuf>,
}

/// A handle to the running watcher thread.
pub struct WatcherHandle {
    /// The worker thread; joins when the recorder stops the watcher.
    pub thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl WatcherHandle {
    /// Signal the watcher to stop and join its thread. Best-effort: a panic
    /// in the worker is swallowed (the recording still finalizes).
    pub fn stop_and_join(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Spawn the watcher thread. Returns `Ok(Some(handle))` on success and
/// `Ok(None)` when filesystem watching could not be set up at all — the matcher
/// could not be built, the notify watcher could not initialize, or even the
/// cwd itself is unwatchable. In every one of those cases the recorder **does
/// not abort**: it prints a single actionable stderr warning and continues
/// without file-change recording (FR-1.5 best-effort: PTY + adapter capture
/// proceed regardless). Only a failure to spawn the worker thread returns `Err`
/// (a fatal OS-resource error).
///
/// A single recursive watch is tried first. If it fails — e.g. one unreadable
/// subdir (`EACCES` on `/tmp/systemd-private-*`) makes `notify` reject the whole
/// recursive call — we fall back to per-directory non-recursive watches,
/// skipping unreadable and ignored directories, so one bad subtree no longer
/// blacks out file recording for the entire session.
///
/// `start` is the session-start `Instant` used to compute event timestamps
/// relative to session start (FR-1.3).
#[allow(clippy::too_many_arguments)] // recorder wiring; a builder would be overkill here
pub fn spawn_watcher(
    opts: WatchOptions,
    writer: Arc<Mutex<EventWriter>>,
    blobs: Arc<BlobStore>,
    session_id: String,
    start: Instant,
) -> crate::Result<Option<WatcherHandle>> {
    let matcher = match build_matcher(&opts.cwd, &opts.extra_ignore) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "hh: warning: could not build ignore matcher ({e}); \
                 file-change recording disabled for this session (recording continues)"
            );
            return Ok(None);
        }
    };
    let stop = Arc::new(AtomicBool::new(false));

    let (ntx, nrx) = std::sync::mpsc::channel();
    let mut watcher = match notify::recommended_watcher(
        move |res: std::result::Result<notify::Event, notify::Error>| {
            // Channel send fails only when the watcher thread exited; ignore.
            let _ = ntx.send(res);
        },
    ) {
        Ok(w) => w,
        Err(e) => {
            eprintln!(
                "hh: warning: could not initialize filesystem watcher ({e}); \
                 file-change recording disabled for this session (recording continues)"
            );
            return Ok(None);
        }
    };

    // Try one recursive watch; on failure fall back to per-directory watches,
    // skipping unreadable / ignored subtrees (FR-1.4 best-effort).
    let cwd_watched = match watcher.watch(&opts.cwd, RecursiveMode::Recursive) {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "hh: warning: recursive watch of {} failed ({e}); \
                 falling back to per-directory watches \
                 (changes in unreadable or newly-created subdirectories may be missed)",
                opts.cwd.display()
            );
            add_per_dir_watches(&mut watcher, &opts.cwd, &matcher, &opts.internal_exclude)
        }
    };
    if !cwd_watched {
        eprintln!(
            "hh: warning: could not watch {} at all; file changes will not be \
             recorded for this session (recording continues — run `hh doctor`)",
            opts.cwd.display()
        );
        return Ok(None);
    }

    // Capture the startup baseline (path → content hash) synchronously on the
    // caller's thread BEFORE spawning the worker. This makes "the watch is
    // observing" a property that holds the instant `spawn_watcher` returns, so
    // any file written afterwards — by the recorded child, or by a test — is
    // guaranteed post-baseline and is never misclassified as a pre-existing
    // file the backstop would then skip as "unchanged". Doing the scan on the
    // worker thread instead left a window (wide in tests, narrow in prod)
    // where a write that raced ahead of the scan was folded into the baseline
    // and dropped — the macOS-CI `events seen: []` flake. The scan is
    // best-effort (unreadable files/dirs are skipped); see [`scan_baseline_hashes`].
    let mut existing: HashMap<PathBuf, String> = HashMap::new();
    scan_baseline_hashes(
        &opts.cwd,
        &opts.cwd,
        &matcher,
        &opts.internal_exclude,
        &opts,
        &mut existing,
    );

    let stop_for_thread = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("hh-fs-watcher".into())
        .spawn(move || {
            run_loop(
                watcher,
                nrx,
                matcher,
                opts,
                writer,
                blobs,
                session_id,
                start,
                stop_for_thread,
                existing,
            );
        })
        .map_err(|e| crate::RecordError::Pty(format!("spawn watcher thread: {e}")))?;

    Ok(Some(WatcherHandle {
        thread: Some(thread),
        stop,
    }))
}

/// Fall back when a single recursive watch fails: walk the tree from `root`
/// adding a non-recursive watch per directory, skipping unreadable (`EACCES`)
/// and ignored (built-in / `.gitignore` / extra) directories. Returns `true` if
/// at least `root` itself was watched, `false` if even the cwd is unwatchable
/// (the caller then disables file-change recording with a warning).
///
/// Newly-created subdirectories are not watched in this fallback mode (the
/// non-recursive watch does not auto-descend) — a known limitation called out in
/// the fallback warning. Best-effort: a `read_dir` failure on a subtree is
/// skipped with at most one warning rather than aborting the walk.
fn add_per_dir_watches(
    watcher: &mut RecommendedWatcher,
    root: &Path,
    matcher: &Gitignore,
    internal_exclude: &[PathBuf],
) -> bool {
    let mut stack = vec![root.to_path_buf()];
    let mut dirs_watched = 0usize;
    let mut warned_skip = false;
    while let Some(dir) = stack.pop() {
        match watcher.watch(&dir, RecursiveMode::NonRecursive) {
            Ok(()) => dirs_watched += 1,
            Err(e) => {
                if dir == root {
                    return false; // can't watch the cwd at all
                }
                if !warned_skip {
                    eprintln!(
                        "hh: warning: could not watch {} ({e}); \
                         skipping this directory and its children",
                        dir.display()
                    );
                    warned_skip = true;
                }
                continue; // don't descend into a directory we can't watch
            }
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if internal_exclude.iter().any(|excl| path.starts_with(excl)) {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let Ok(ft) = e.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            if matcher.matched_path_or_any_parents(rel, true).is_ignore() {
                continue;
            }
            stack.push(path);
        }
    }
    dirs_watched > 0
}

/// A self-cleaning temporary directory for the watcher smoke test. `Drop`
/// removes it (best-effort) so `watcher_smoke_test` leaves nothing behind even
/// on an early return.
struct TempProbe(PathBuf);

impl Drop for TempProbe {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Self-contained filesystem-watcher smoke test for `hh doctor`: create a
/// temporary directory, watch it, write a file inside it, and confirm the
/// watcher delivers at least one event within a short deadline. Returns
/// `Ok(())` if an event arrived and `Err(message)` otherwise (notify init
/// failed, the watch could not be added, the watcher reported an error, or no
/// event arrived in time). This exercises the same `notify` backend the
/// recorder uses, so a failure here explains why a session recorded 0 file
/// changes despite files changing. Read-only with respect to the user's data
/// dir — it writes only under a throwaway temp directory it cleans up itself.
#[allow(clippy::missing_errors_doc)] // returns an ad-hoc reason string, not a typed error
pub fn watcher_smoke_test() -> std::result::Result<(), String> {
    use std::sync::mpsc;
    // A unique-ish name from pid + a nanosecond stamp (no tempfile dependency
    // in the binary; `std::env::temp_dir` is enough for a one-shot probe).
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let root =
        std::env::temp_dir().join(format!("hh-doctor-smoke-{}-{}", std::process::id(), stamp));
    std::fs::create_dir_all(&root).map_err(|e| format!("create temp dir: {e}"))?;
    // Declared before `watcher` so it drops last: the watch is released before
    // the temp directory is removed.
    let _probe = TempProbe(root.clone());

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .map_err(|e| format!("notify init failed: {e}"))?;

    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| format!("watch failed: {e}"))?;

    std::fs::write(root.join("probe.txt"), b"hh doctor smoke test")
        .map_err(|e| format!("write probe file: {e}"))?;

    let outcome = match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(_event)) => Ok(()),
        Ok(Err(e)) => Err(format!("watcher delivered an error: {e}")),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(
            "no file-change event within 2s — the watcher is not delivering events on this platform"
                .to_string(),
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("watcher channel closed before any event".to_string())
        }
    };
    // Drop the watcher to release the inotify/FSEvents handle before the guard
    // removes the directory.
    drop(watcher);
    outcome
}

/// Build a single `Gitignore` matcher from built-in + root `.gitignore` +
/// extra patterns, rooted at `cwd`.
fn build_matcher(cwd: &Path, extra_ignore: &[String]) -> std::result::Result<Gitignore, String> {
    let mut builder = GitignoreBuilder::new(cwd);
    for line in BUILTIN_IGNORE {
        builder
            .add_line(None, line)
            .map_err(|e| format!("built-in ignore `{line}`: {e}"))?;
    }
    let root_gitignore = cwd.join(".gitignore");
    if root_gitignore.is_file() {
        // `GitignoreBuilder::add` loads a file's lines as patterns, returning
        // `Option<Error>` (a malformed line). Non-fatal: warn and continue.
        if let Some(e) = builder.add(&root_gitignore) {
            eprintln!(
                "hh: warning: failed to read {}: {e}",
                root_gitignore.display()
            );
        }
    }
    for line in extra_ignore {
        builder
            .add_line(None, line)
            .map_err(|e| format!("extra ignore `{line}`: {e}"))?;
    }
    builder.build().map_err(|e| format!("build gitignore: {e}"))
}

/// The watcher event loop, run on the worker thread. Events are not processed
/// immediately: each capturable path enters a [`pending`] map keyed by absolute
/// path, and is processed only after its debounce window elapses with no
/// further event on the same path (FR-1.4). On shutdown, any still-pending
/// change is flushed so a quick exit doesn't drop a write that landed inside
/// the window.
// recorder wiring; values are moved into the spawning closure and owned for
// the thread's lifetime (thread entry point, see runner::run_reader).
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)] // see comment above
fn run_loop(
    watcher: RecommendedWatcher,
    nrx: std::sync::mpsc::Receiver<std::result::Result<notify::Event, notify::Error>>,
    matcher: Gitignore,
    opts: WatchOptions,
    writer: Arc<Mutex<EventWriter>>,
    blobs: Arc<BlobStore>,
    session_id: String,
    start: Instant,
    stop: Arc<AtomicBool>,
    // Baseline of files (path → startup content hash) that existed under `cwd`
    // before the watch started observing, captured synchronously in
    // [`spawn_watcher`] on the caller's thread *before* the worker is spawned
    // — so anything written after `spawn_watcher` returns is post-baseline and
    // never misclassified as a pre-existing file. See [`spawn_watcher`] and
    // [`rescan_for_missed_changes`] for how this baseline is used (spurious-
    // create correction + missed-modify detection). The baseline content is
    // NOT stored as a blob (no refcount for `before_hash`); only its hash is
    // kept, so a backstopped modify renders with a missing before-side ("all
    // added") — the original was overwritten before we observed it.
    mut existing: HashMap<PathBuf, String>,
) {
    let mut known: HashMap<PathBuf, String> = HashMap::new();
    let mut pending: HashMap<PathBuf, Pending> = HashMap::new();
    while !stop.load(Ordering::Acquire) {
        // Panic-hygiene test hook (never compiled outside `cfg(test)`): lets a
        // test flip a flag and observe that a real panic on this thread is
        // contained by `stop_and_join`'s `let _ = t.join()` rather than
        // aborting the process or leaving the recording unable to finalize.
        #[cfg(test)]
        assert!(
            !INJECT_PANIC_FOR_TEST.swap(false, Ordering::SeqCst),
            "hh-record test: injected watcher panic"
        );
        match nrx.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(event)) => fold_event(&event, &matcher, &opts, &mut existing, &mut pending),
            Ok(Err(e)) => eprintln!("hh: warning: fs watcher error: {e}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Flush any pending changes whose debounce window has elapsed.
        flush_due(
            &mut pending,
            &opts,
            &writer,
            &blobs,
            &session_id,
            start,
            &mut known,
        );
    }
    // --- Shutdown grace drain (FR-1.4 robustness) ------------------------
    // `stop` was signaled: the child has exited and the recorder is shutting
    // down. But the OS filesystem event source may still have in-flight
    // events — notably macOS FSEvents, which coalesces deliveries through a
    // daemon and can report a write AFTER the process that performed it has
    // already exited. Stopping immediately (the old behavior) dropped those
    // late events, so a quick-exiting agent could finalize with "0 file
    // changes" even though it wrote files. Drain for a bounded window
    // instead: keep receiving until the channel is quiet for [`GRACE_QUIET`]
    // (caught up) or [`GRACE_MAX`] elapses (hard cap), whichever is first. A
    // prompt backend (Linux inotify, events already delivered) pays only the
    // short quiet wait.
    let drain_start = Instant::now();
    loop {
        let cap_left = GRACE_MAX
            .checked_sub(drain_start.elapsed())
            .unwrap_or(Duration::ZERO);
        if cap_left.is_zero() {
            break;
        }
        let wait = if GRACE_QUIET < cap_left {
            GRACE_QUIET
        } else {
            cap_left
        };
        match nrx.recv_timeout(wait) {
            Ok(Ok(event)) => fold_event(&event, &matcher, &opts, &mut existing, &mut pending),
            Ok(Err(e)) => eprintln!("hh: warning: fs watcher error: {e}"),
            // Quiet for `wait` with no event, or channel closed → caught up.
            Err(
                std::sync::mpsc::RecvTimeoutError::Timeout
                | std::sync::mpsc::RecvTimeoutError::Disconnected,
            ) => break,
        }
        flush_due(
            &mut pending,
            &opts,
            &writer,
            &blobs,
            &session_id,
            start,
            &mut known,
        );
    }
    // Flush whatever is still pending so a write that landed inside the window
    // just before shutdown is not lost.
    flush_all(
        &mut pending,
        &opts,
        &writer,
        &blobs,
        &session_id,
        start,
        &mut known,
    );
    // Deterministic backstop: re-walk cwd for any file the watcher never
    // observed (the grace drain only helps when an event is *late*; this
    // catches the case where the backend delivered *nothing* — e.g. macOS
    // FSEvents on CI runners, observed flaky/absent). Files already in `known`
    // (captured via the event path) or unchanged from the `existing` baseline
    // are skipped, so this never duplicates a normal-path capture. A pre-
    // existing file whose content hash differs from its baseline is recorded as
    // a missed `Modified`.
    rescan_for_missed_changes(
        &opts,
        &matcher,
        &writer,
        &blobs,
        &session_id,
        start,
        &mut known,
        &existing,
    );
    // Drop the watcher to release the inotify/fsevents handle.
    drop(watcher);
}

/// Fold a raw `notify` event into the debounced `pending` map: classify each
/// path against the ignore matcher, drop spurious first-seen "creates" on
/// pre-existing files, and coalesce. Shared by the live loop and the shutdown
/// grace drain so the two stay in sync.
fn fold_event(
    event: &notify::Event,
    matcher: &Gitignore,
    opts: &WatchOptions,
    existing: &mut HashMap<PathBuf, String>,
    pending: &mut HashMap<PathBuf, Pending>,
) {
    for path in &event.paths {
        if let Some(kind) = classify(path, event.kind, matcher, opts) {
            let kind = resolve_first_seen(kind, path, existing);
            coalesce(pending, path.clone(), kind);
        }
    }
}

/// Final shutdown backstop (FR-1.4 robustness): re-walk `cwd` and record any
/// capturable file change the watcher never observed.
///
/// This is deterministic and does **not** depend on the OS event source
/// delivering anything. It is the safety net for backends that drop or never
/// deliver events — notably macOS FSEvents, which is observed flaky/absent on
/// GitHub's macOS runners (a quick-exiting agent can write a file and exit
/// before `fseventsd` reports anything, and the grace drain only helps when
/// the event is merely *late*, not *absent*). Without this backstop such a
/// session finalizes with "0 file changes" and `hh inspect --diff` reports
/// "no file changes in this session" despite files being written.
///
/// For each capturable file currently on disk:
/// - in `known` → already recorded via the event path, skip (no duplicate);
/// - in `existing` (present at baseline) → compare its current content hash
///   to the baseline hash captured at startup. Unchanged → skip. Changed →
///   record as a missed `Modified`, seeding `known[path] = baseline_hash` so
///   [`process`] records `before = baseline`, `after = current`. The baseline
///   blob was never stored (only its hash was kept), so the diff before-side
///   is missing and the change renders as "all added" — the original content
///   was overwritten before we observed it, so there is nothing to diff
///   against. This is the one cosmetic gap of the hash-only baseline; it is
///   documented and preferable to dropping the change entirely;
/// - not in `known` and not in `existing` → created during the session with no
///   event observed → record as `Created`.
///
/// Files larger than `max_file_size` are never capturable, so they are skipped
/// by the hash comparison (and [`process`] would skip them anyway).
#[allow(clippy::too_many_arguments)] // recorder wiring threaded through to the writer
fn rescan_for_missed_changes(
    opts: &WatchOptions,
    matcher: &Gitignore,
    writer: &Arc<Mutex<EventWriter>>,
    blobs: &Arc<BlobStore>,
    session_id: &str,
    start: Instant,
    known: &mut HashMap<PathBuf, String>,
    existing: &HashMap<PathBuf, String>,
) {
    let mut current: HashSet<PathBuf> = HashSet::new();
    scan_existing_files(
        &opts.cwd,
        &opts.cwd,
        matcher,
        &opts.internal_exclude,
        &mut current,
    );
    for path in current {
        if known.contains_key(&path) {
            continue;
        }
        if let Some(baseline_hash) = existing.get(&path) {
            // Baseline file: detect a missed modify by comparing the current
            // content hash to the startup baseline hash. Unchanged → skip.
            let Some(current_hash) = read_hash_if_capturable(&path, opts) else {
                continue;
            };
            if current_hash == *baseline_hash {
                continue;
            }
            // Modified during the session but the event was missed. Seed
            // `known` with the baseline hash so `process` records
            // `before = baseline`, `after = current`.
            known.insert(path.clone(), baseline_hash.clone());
            if let Err(e) = process(
                &path,
                ChangeKind::Modified,
                opts,
                writer,
                blobs,
                session_id,
                start,
                known,
            ) {
                eprintln!(
                    "hh: warning: file change capture failed for {}: {e}",
                    path.display()
                );
            }
        } else {
            // Not in the baseline and not already recorded → created during
            // the session with no event observed.
            if let Err(e) = process(
                &path,
                ChangeKind::Created,
                opts,
                writer,
                blobs,
                session_id,
                start,
                known,
            ) {
                eprintln!(
                    "hh: warning: file change capture failed for {}: {e}",
                    path.display()
                );
            }
        }
    }
}

/// Read `path` and return its BLAKE3 content hash, or `None` if it is not
/// capturable (missing, larger than `max_file_size`, or unreadable). Used by
/// the shutdown backstop to compare a baseline file's current content against
/// its startup hash without storing a blob.
fn read_hash_if_capturable(path: &Path, opts: &WatchOptions) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > opts.max_file_size {
        return None;
    }
    let mut f = std::fs::File::open(path).ok()?;
    let mut content = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    f.read_to_end(&mut content).ok()?;
    Some(BlobStore::hash(&content))
}

/// A debounced, not-yet-processed change to one path. `suppress` is set when a
/// create was followed by a delete within the same window (net no-op).
#[derive(Debug, Clone, Copy)]
struct Pending {
    /// Merged change kind to record once the window elapses.
    kind: ChangeKind,
    /// Deadline after which the change may be flushed.
    deadline: Instant,
    /// If `true`, drop the change on flush (net no-op within the window).
    suppress: bool,
}

/// Flush every pending change whose deadline has passed.
fn flush_due(
    pending: &mut HashMap<PathBuf, Pending>,
    opts: &WatchOptions,
    writer: &Arc<Mutex<EventWriter>>,
    blobs: &Arc<BlobStore>,
    session_id: &str,
    start: Instant,
    known: &mut HashMap<PathBuf, String>,
) {
    let now = Instant::now();
    let due: Vec<PathBuf> = pending
        .iter()
        .filter(|(_, p)| p.deadline <= now)
        .map(|(k, _)| k.clone())
        .collect();
    for path in due {
        flush_one(
            pending, &path, opts, writer, blobs, session_id, start, known,
        );
    }
}

/// Flush every pending change, regardless of deadline (shutdown path).
fn flush_all(
    pending: &mut HashMap<PathBuf, Pending>,
    opts: &WatchOptions,
    writer: &Arc<Mutex<EventWriter>>,
    blobs: &Arc<BlobStore>,
    session_id: &str,
    start: Instant,
    known: &mut HashMap<PathBuf, String>,
) {
    let paths: Vec<PathBuf> = pending.keys().cloned().collect();
    for path in paths {
        flush_one(
            pending, &path, opts, writer, blobs, session_id, start, known,
        );
    }
}

/// Remove and process one pending change, logging a warning on failure.
#[allow(clippy::too_many_arguments)] // recorder wiring threaded through to the writer
fn flush_one(
    pending: &mut HashMap<PathBuf, Pending>,
    path: &Path,
    opts: &WatchOptions,
    writer: &Arc<Mutex<EventWriter>>,
    blobs: &Arc<BlobStore>,
    session_id: &str,
    start: Instant,
    known: &mut HashMap<PathBuf, String>,
) {
    let Some(p) = pending.remove(path) else {
        return;
    };
    if p.suppress {
        return;
    }
    if let Err(e) = process(path, p.kind, opts, writer, blobs, session_id, start, known) {
        eprintln!(
            "hh: warning: file change capture failed for {}: {e}",
            path.display()
        );
    }
}

/// Decide whether a raw notify event on `path` should be captured, and if so
/// return the [`ChangeKind`]. Filters internal halfhand paths, paths outside
/// the watched tree, directories, and ignored paths (built-in, `.gitignore`,
/// and caller-supplied extra patterns). This runs *before* debouncing so
/// ignored files never enter the pending map.
fn classify(
    path: &Path,
    kind: notify::EventKind,
    matcher: &Gitignore,
    opts: &WatchOptions,
) -> Option<ChangeKind> {
    for excl in &opts.internal_exclude {
        if path.starts_with(excl) {
            return None;
        }
    }
    let rel = path.strip_prefix(&opts.cwd).ok()?;
    if path.is_dir() {
        return None;
    }
    if matcher.matched_path_or_any_parents(rel, false).is_ignore() {
        return None;
    }
    Some(match kind {
        notify::EventKind::Create(_) => ChangeKind::Created,
        notify::EventKind::Modify(_) => ChangeKind::Modified,
        notify::EventKind::Remove(_) => ChangeKind::Deleted,
        // Any/Other/Access are not content changes we capture.
        notify::EventKind::Any | notify::EventKind::Other | notify::EventKind::Access(_) => {
            return None;
        }
    })
}

/// Recursively collect every regular file under `dir` (relative to `cwd`)
/// that isn't excluded or ignored, into `out`. Walked once at watch startup
/// to build the baseline for [`resolve_first_seen`]. Does not follow
/// symlinks (`DirEntry::file_type` reports the link itself, not its target),
/// which also avoids symlink-cycle recursion. Best-effort: an unreadable
/// subdirectory is skipped rather than failing the whole scan.
fn scan_existing_files(
    dir: &Path,
    cwd: &Path,
    matcher: &Gitignore,
    internal_exclude: &[PathBuf],
    out: &mut HashSet<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if internal_exclude.iter().any(|excl| path.starts_with(excl)) {
            continue;
        }
        let Ok(rel) = path.strip_prefix(cwd) else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        if matcher.matched_path_or_any_parents(rel, is_dir).is_ignore() {
            continue;
        }
        if is_dir {
            scan_existing_files(&path, cwd, matcher, internal_exclude, out);
        } else {
            out.insert(path);
        }
    }
}

/// Like [`scan_existing_files`] but records each capturable file's startup
/// content hash (BLAKE3) into `out`, not just its path. Run once at watch
/// startup to build the baseline that [`rescan_for_missed_changes`] diffs
/// against at shutdown. The baseline content itself is NOT stored as a blob —
/// only its hash — so a backstopped modify renders with a missing before-side.
///
/// Files larger than `max_file_size` are never capturable, so they are skipped
/// here (no point hashing a file the watcher would never record). Best-effort:
/// an unreadable subdirectory or an unreadable file (permissions, race) is
/// skipped rather than failing the whole scan — a missing baseline entry just
/// means that file, if later modified, is backstopped as a `Created` rather
/// than a `Modified`, which is a safe over-report.
fn scan_baseline_hashes(
    dir: &Path,
    cwd: &Path,
    matcher: &Gitignore,
    internal_exclude: &[PathBuf],
    opts: &WatchOptions,
    out: &mut HashMap<PathBuf, String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if internal_exclude.iter().any(|excl| path.starts_with(excl)) {
            continue;
        }
        let Ok(rel) = path.strip_prefix(cwd) else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        if matcher.matched_path_or_any_parents(rel, is_dir).is_ignore() {
            continue;
        }
        if is_dir {
            scan_baseline_hashes(&path, cwd, matcher, internal_exclude, opts, out);
        } else {
            // Skip oversized files (the watcher would never capture them, so a
            // baseline hash for them is useless I/O). A metadata error (file
            // raced away, unreadable) is silently dropped — see the doc comment.
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.len() > opts.max_file_size {
                    continue;
                }
                if let Ok(mut f) = std::fs::File::open(&path) {
                    let mut content = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
                    if f.read_to_end(&mut content).is_ok() {
                        out.insert(path, BlobStore::hash(&content));
                    }
                }
            }
        }
    }
}

/// Resolve a raw classified [`ChangeKind`] against the set of paths already
/// known to exist, correcting for a macOS FSEvents quirk: `fseventsd` can tag
/// the *first-ever* notification it delivers for a path with
/// `kFSEventStreamEventFlagItemCreated` even when the path predates the watch
/// (it keeps no baseline of what existed before the stream started). Left
/// uncorrected, editing a pre-existing file is recorded as `created` instead
/// of `modified` (its very first observed event wins the merge in
/// [`merge_kind`]).
///
/// `existing` maps a baseline path to its startup content hash. It is updated
/// as change kinds are resolved, so a later delete-then-recreate of the same
/// path within the same session is still reported as a genuine `Created`. A
/// pre-existing baseline hash is preserved across a `Created`→`Modified`
/// downgrade (it is not overwritten) so the shutdown backstop can still
/// diff against it; a genuinely new path is inserted with an empty sentinel
/// hash (no baseline) purely to mark it seen — the backstop skips any path
/// already in `known`, so the sentinel is never compared.
fn resolve_first_seen(
    kind: ChangeKind,
    path: &Path,
    existing: &mut HashMap<PathBuf, String>,
) -> ChangeKind {
    match kind {
        ChangeKind::Created if existing.contains_key(path) => ChangeKind::Modified,
        ChangeKind::Created | ChangeKind::Modified => {
            // Mark the path seen so a later raw `Created` (the FSEvents quirk
            // firing again) downgrades to `Modified`. `or_default` preserves a
            // pre-existing baseline hash (set at startup) for the backstop.
            existing.entry(path.to_path_buf()).or_default();
            kind
        }
        ChangeKind::Deleted => {
            existing.remove(path);
            ChangeKind::Deleted
        }
    }
}

/// Fold a new event on `path` into the pending map, extending its debounce
/// window and merging kinds. A create followed by a delete within the window
/// is a net no-op and is marked `suppress`.
fn coalesce(pending: &mut HashMap<PathBuf, Pending>, path: PathBuf, kind: ChangeKind) {
    let now = Instant::now();
    let deadline = now + DEBOUNCE_WINDOW;
    match pending.get_mut(&path) {
        None => {
            pending.insert(
                path,
                Pending {
                    kind,
                    deadline,
                    suppress: false,
                },
            );
        }
        Some(p) => {
            p.deadline = deadline;
            let (merged, suppress) = merge_kind(p.kind, kind);
            p.kind = merged;
            p.suppress = p.suppress || suppress;
        }
    }
}

/// Merge a prior pending kind with a newly-observed kind on the same path
/// within one debounce window. Returns `(merged_kind, suppress)`.
///
/// Rules:
/// - Create → Delete (same window): net no-op → `suppress`.
/// - Anything → Created: the path exists now → `Created` (covers delete-then-
///   recreate, which is rare within 100 ms but handled sensibly).
/// - Modify after a Create stays `Created` (still the first observation).
/// - Otherwise the latest kind wins, except two Modifies stay `Modified`.
fn merge_kind(prior: ChangeKind, new: ChangeKind) -> (ChangeKind, bool) {
    match (prior, new) {
        // Create then Delete within the window is a net no-op → suppress.
        (ChangeKind::Created, ChangeKind::Deleted) => (ChangeKind::Deleted, true),
        // A (re)create means the path exists now; a modify right after the
        // initial create is still the first observation of the file.
        (ChangeKind::Created, ChangeKind::Modified) | (_, ChangeKind::Created) => {
            (ChangeKind::Created, false)
        }
        // A delete wins over a prior modify/observe — including a Modify
        // observed after a Delete, which is stale/spurious (the path can't
        // be modified once it doesn't exist). macOS FSEvents can coalesce a
        // whole create+modify+delete history into one notification batch and
        // deliver it in `translate_flags`' fixed flag-check order rather than
        // true chronological order (e.g. Created, Removed, ..., Modified);
        // ground truth wins, so once deleted, stays deleted.
        (_, ChangeKind::Deleted) | (ChangeKind::Deleted, ChangeKind::Modified) => {
            (ChangeKind::Deleted, false)
        }
        // Two modifies (and any other combo) collapse to a single modify.
        _ => (ChangeKind::Modified, false),
    }
}

/// Process one debounced change into a `file_change` event (or skip it). The
/// change kind is already known (computed by [`classify`], possibly merged by
/// [`coalesce`]); this reads content, manages before/after hashes, and appends
/// the event + `file_changes` row.
#[allow(clippy::too_many_arguments)] // recorder wiring threaded through to the writer
fn process(
    path: &Path,
    change_kind: ChangeKind,
    opts: &WatchOptions,
    writer: &Arc<Mutex<EventWriter>>,
    blobs: &Arc<BlobStore>,
    session_id: &str,
    start: Instant,
    known: &mut HashMap<PathBuf, String>,
) -> std::result::Result<(), String> {
    // Defensive: re-strip in case a path outside cwd slipped through.
    let Ok(rel) = path.strip_prefix(&opts.cwd) else {
        return Ok(());
    };
    let ts_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    let rel_str = rel.to_string_lossy().to_string();

    let (before_hash, after_hash, blob_hash, blob_size, is_binary) =
        if change_kind == ChangeKind::Deleted {
            let before = known.remove(path);
            (before, None, None, None, false)
        } else {
            // Create / Modify: read current content if within size limit.
            let meta = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(format!("stat {}: {e}", path.display())),
            };
            if meta.len() > opts.max_file_size {
                // FR-1.4: only capture files ≤ max_file_size.
                return Ok(());
            }
            let mut content = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
            let mut f = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(format!("open {}: {e}", path.display())),
            };
            f.read_to_end(&mut content)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            let binary = is_binary(&content);
            let hash = BlobStore::hash(&content);
            let size = u64::try_from(content.len()).unwrap_or(u64::MAX);
            let before = known.get(path).cloned();
            // Store content unless it's binary and the user opted out.
            let stored_hash = if binary && !opts.record_binary {
                None
            } else {
                blobs
                    .put(&content)
                    .map_err(|e| format!("blob put {}: {e}", path.display()))?;
                Some(hash.clone())
            };
            known.insert(path.to_path_buf(), hash.clone());
            (before, Some(hash), stored_hash, Some(size), binary)
        };

    let event = Event {
        session_id: session_id.to_string(),
        ts_ms,
        kind: EventKind::FileChange,
        step: None, // file changes are steps in replay (FR-3.4); step ordinal derived at read time.
        summary: truncate_summary(&format!("{rel_str} {change_kind}")),
        body_json: Some(serde_json::json!({
            "path": rel_str,
            "change_kind": change_kind.to_string(),
        })),
        blob_hash: blob_hash.clone(),
        blob_size,
        correlates: None,
    };
    let file_change = FileChange {
        event_id: 0, // overwritten by the writer (EventWriter::append_file_change).
        path: rel_str,
        change_kind,
        before_hash,
        after_hash,
        is_binary,
    };

    // Recover from poisoning rather than failing this (and every subsequent)
    // capture: the writer `Mutex` is shared with the PTY reader and adapter
    // drain threads, and a panic in one of them must not silently blind the
    // others for the rest of the session (see `runner::lock_writer`).
    let writer = writer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    writer
        .append_file_change(event, file_change)
        .map_err(|e| format!("append file_change: {e}"))?;
    Ok(())
}

/// Binary heuristic (FR-1.4): a NUL byte in the first 8 KiB.
fn is_binary(content: &[u8]) -> bool {
    content.iter().take(8192).any(|&b| b == 0)
}

/// Truncate a summary to the SRS §4.1 limit of 120 chars.
fn truncate_summary(s: &str) -> String {
    const LIMIT: usize = 120;
    if s.chars().count() <= LIMIT {
        return s.to_string();
    }
    let truncated: String = s.chars().take(LIMIT - 1).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_byte_is_binary() {
        assert!(is_binary(b"hello\x00world"));
        assert!(!is_binary(b"plain text only"));
        // NUL beyond 8 KiB is not detected (matches the heuristic).
        let mut big = vec![b'a'; 9000];
        big[9000 - 1] = 0;
        assert!(!is_binary(&big));
    }

    #[test]
    fn truncate_summary_respects_limit() {
        let short = "a1b2c3";
        assert_eq!(truncate_summary(short), short);
        let long: String = "x".repeat(200);
        let t = truncate_summary(&long);
        assert!(t.chars().count() <= 120);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn build_matcher_skips_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        let m = build_matcher(tmp.path(), &[]).unwrap();
        assert!(m
            .matched_path_or_any_parents("node_modules/foo.rs", false)
            .is_ignore());
        assert!(m
            .matched_path_or_any_parents(".git/HEAD", false)
            .is_ignore());
        assert!(m
            .matched_path_or_any_parents("target/debug/hh", false)
            .is_ignore());
        assert!(!m
            .matched_path_or_any_parents("src/main.rs", false)
            .is_ignore());
    }

    #[test]
    fn build_matcher_extra_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let m = build_matcher(tmp.path(), &["dist/".into(), "*.lock".into()]).unwrap();
        assert!(m
            .matched_path_or_any_parents("dist/app.js", false)
            .is_ignore());
        assert!(m
            .matched_path_or_any_parents("Cargo.lock", false)
            .is_ignore());
        assert!(!m
            .matched_path_or_any_parents("src/main.rs", false)
            .is_ignore());
    }

    #[test]
    fn merge_kind_collapse_editor_double_write() {
        // Two modifies of the same file within the window collapse to one Modify.
        let (k, s) = merge_kind(ChangeKind::Modified, ChangeKind::Modified);
        assert_eq!(k, ChangeKind::Modified);
        assert!(!s);
        // Create then Modify (editor's "write then touch") stays a single Created.
        let (k, s) = merge_kind(ChangeKind::Created, ChangeKind::Modified);
        assert_eq!(k, ChangeKind::Created);
        assert!(!s);
    }

    #[test]
    fn merge_kind_create_then_delete_is_noop() {
        // Created then Removed within the window is a net no-op → suppress.
        let (k, s) = merge_kind(ChangeKind::Created, ChangeKind::Deleted);
        assert_eq!(k, ChangeKind::Deleted);
        assert!(s);
    }

    #[test]
    fn merge_kind_modify_then_delete_is_delete() {
        let (k, s) = merge_kind(ChangeKind::Modified, ChangeKind::Deleted);
        assert_eq!(k, ChangeKind::Deleted);
        assert!(!s);
    }

    #[test]
    fn merge_kind_delete_then_recreate_is_created() {
        let (k, s) = merge_kind(ChangeKind::Deleted, ChangeKind::Created);
        assert_eq!(k, ChangeKind::Created);
        assert!(!s);
    }

    #[test]
    fn merge_kind_delete_then_stale_modify_stays_deleted() {
        // A Modify flag observed after a Delete in the same coalesced batch
        // is stale (the path can't be modified once gone) — ground truth
        // (deleted) must win, not the catch-all Modified fallback.
        let (k, s) = merge_kind(ChangeKind::Deleted, ChangeKind::Modified);
        assert_eq!(k, ChangeKind::Deleted);
        assert!(!s);
    }

    #[test]
    fn coalesce_extends_window_and_suppresses_create_delete_burst() {
        let mut pending: HashMap<PathBuf, Pending> = HashMap::new();
        let path = PathBuf::from("/tmp/x");
        coalesce(&mut pending, path.clone(), ChangeKind::Created);
        let first_deadline = pending[&path].deadline;
        // A second event on the same path within the window extends the deadline.
        coalesce(&mut pending, path.clone(), ChangeKind::Deleted);
        let entry = &pending[&path];
        assert!(entry.deadline >= first_deadline);
        assert!(entry.suppress);
    }

    #[test]
    fn coalesce_keeps_separate_paths_independent() {
        let mut pending: HashMap<PathBuf, Pending> = HashMap::new();
        coalesce(&mut pending, PathBuf::from("/tmp/a"), ChangeKind::Created);
        coalesce(&mut pending, PathBuf::from("/tmp/b"), ChangeKind::Modified);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[&PathBuf::from("/tmp/a")].kind, ChangeKind::Created);
        assert_eq!(pending[&PathBuf::from("/tmp/b")].kind, ChangeKind::Modified);
    }

    #[test]
    fn resolve_first_seen_downgrades_created_for_known_existing_path() {
        // A path already in the baseline set was on disk before the watch
        // started: a raw `Created` for it is the macOS FSEvents quirk, not a
        // real creation. The baseline hash is preserved (not overwritten).
        let path = PathBuf::from("/tmp/modified.txt");
        let mut existing: HashMap<PathBuf, String> = [(path.clone(), "baseline-hash".into())]
            .into_iter()
            .collect();
        let kind = resolve_first_seen(ChangeKind::Created, &path, &mut existing);
        assert_eq!(kind, ChangeKind::Modified);
        assert_eq!(
            existing.get(&path).map(String::as_str),
            Some("baseline-hash")
        );
    }

    #[test]
    fn resolve_first_seen_keeps_created_for_new_path_and_tracks_it() {
        let path = PathBuf::from("/tmp/created.txt");
        let mut existing: HashMap<PathBuf, String> = HashMap::new();
        let kind = resolve_first_seen(ChangeKind::Created, &path, &mut existing);
        assert_eq!(kind, ChangeKind::Created);
        // Now tracked, so a later edit is a genuine Modified, not re-downgraded.
        assert!(existing.contains_key(&path));
    }

    #[test]
    fn resolve_first_seen_treats_recreate_after_delete_as_created() {
        // A baseline path that gets deleted then recreated within the same
        // session must report the recreate as a real Created, not Modified.
        let path = PathBuf::from("/tmp/doomed.txt");
        let mut existing: HashMap<PathBuf, String> = [(path.clone(), "baseline-hash".into())]
            .into_iter()
            .collect();
        assert_eq!(
            resolve_first_seen(ChangeKind::Deleted, &path, &mut existing),
            ChangeKind::Deleted
        );
        assert!(!existing.contains_key(&path));
        assert_eq!(
            resolve_first_seen(ChangeKind::Created, &path, &mut existing),
            ChangeKind::Created
        );
    }

    #[test]
    fn scan_existing_files_finds_pre_existing_files_respecting_ignores() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        std::fs::write(cwd.join("modified.txt"), "orig\n").unwrap();
        std::fs::create_dir_all(cwd.join("sub")).unwrap();
        std::fs::write(cwd.join("sub").join("nested.txt"), "orig\n").unwrap();
        std::fs::create_dir_all(cwd.join("node_modules")).unwrap();
        std::fs::write(cwd.join("node_modules").join("ignored.txt"), "x\n").unwrap();
        let matcher = build_matcher(cwd, &[]).unwrap();

        let mut existing = HashSet::new();
        scan_existing_files(cwd, cwd, &matcher, &[], &mut existing);

        assert!(existing.contains(&cwd.join("modified.txt")));
        assert!(existing.contains(&cwd.join("sub").join("nested.txt")));
        assert!(!existing.contains(&cwd.join("node_modules").join("ignored.txt")));
    }

    #[test]
    fn scan_existing_files_honors_internal_exclude() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        std::fs::create_dir_all(cwd.join(".hh")).unwrap();
        std::fs::write(cwd.join(".hh").join("hh.db"), "x\n").unwrap();
        let matcher = build_matcher(cwd, &[]).unwrap();

        let mut existing = HashSet::new();
        scan_existing_files(cwd, cwd, &matcher, &[cwd.join(".hh")], &mut existing);

        assert!(!existing.contains(&cwd.join(".hh").join("hh.db")));
    }

    /// Panic hygiene (CLAUDE.md v1.0.0 addendum): a panic on the watcher
    /// thread must never abort the recording. This injects a real panic (via
    /// [`INJECT_PANIC_FOR_TEST`]) into a live `run_loop` — on a real thread,
    /// with the real `notify::Watcher` — and checks: (1) `stop_and_join`
    /// does not propagate it to the caller, (2) a write from *before* the
    /// panic stays durable, and (3) the shared writer — even if its `Mutex`
    /// was poisoned by the panic — still accepts further appends (the
    /// single-writer lock is shared with the PTY reader and adapter drain
    /// threads, so this is the difference between "one source degrades" and
    /// "the whole session silently stops recording").
    ///
    /// The panic trigger doesn't depend on a real OS file-change event: the
    /// injection check runs at the top of every `run_loop` iteration
    /// (including timeout-driven wakeups every `POLL_INTERVAL`), so arming
    /// the flag and waiting a few multiples of it is enough — deliberately
    /// avoiding a dependency on FSEvents actually firing, which was observed
    /// flaky/absent on GitHub's macOS runners. The process-global injection
    /// flag is why this test and [`watcher_captures_write_then_immediate_stop`]
    /// (the only other test that runs a real `run_loop`) take
    /// [`REAL_RUN_LOOP_MUTEX`]: without serialization a *concurrent* worker
    /// would observe the armed flag, panic before its own backstop runs, and
    /// turn that test into the macOS-CI `events seen: []` flake.
    #[test]
    fn watcher_panic_does_not_abort_recording() {
        use hh_core::event::{AdapterStatus, AgentKind, NewSession};
        use hh_core::store::Store;

        // Serialize with the other real-`run_loop` test: this test arms the
        // process-global `INJECT_PANIC_FOR_TEST` flag, which a concurrently-
        // running worker from `watcher_captures_write_then_immediate_stop`
        // would observe and panic on, aborting before its backstop runs. Hold
        // the guard for the whole test so the flag is only seen by OUR worker.
        let _real_run_guard = REAL_RUN_LOOP_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let created = store
            .create_session(&NewSession {
                id: hh_core::event::now_v7(),
                started_at: 0,
                agent_kind: AgentKind::Generic,
                adapter_status: AdapterStatus::None,
                command: vec!["test".into()],
                cwd: cwd.clone(),
                hostname: None,
                hh_version: "test".into(),
                model: None,
                git_branch: None,
                git_sha: None,
                git_dirty: None,
            })
            .unwrap();
        let writer = Arc::new(Mutex::new(store.event_writer().unwrap()));
        let blobs = Arc::new(BlobStore::new(store.blobs().root().to_path_buf()));

        // A write from before anything panics, appended directly (not via a
        // real FS event — see the doc comment above) so durability is
        // asserted deterministically.
        writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .append_event(Event {
                session_id: created.id.clone(),
                ts_ms: 0,
                kind: EventKind::Lifecycle,
                step: None,
                summary: "before panic".into(),
                body_json: None,
                blob_hash: None,
                blob_size: None,
                correlates: None,
            })
            .unwrap();

        let opts = WatchOptions {
            cwd: cwd.clone(),
            max_file_size: 4 * 1024 * 1024,
            record_binary: false,
            extra_ignore: Vec::new(),
            internal_exclude: Vec::new(),
        };
        let handle = spawn_watcher(
            opts,
            Arc::clone(&writer),
            Arc::clone(&blobs),
            created.id.clone(),
            Instant::now(),
        )
        .unwrap()
        .expect("watcher should initialize on a writable temp dir");

        // Arm the injection; the loop wakes up (and hits the check) every
        // POLL_INTERVAL regardless of any real filesystem event, so a few
        // multiples of it is a generous, platform-independent margin.
        INJECT_PANIC_FOR_TEST.store(true, Ordering::SeqCst);
        std::thread::sleep(POLL_INTERVAL * 20);

        // Must not propagate the panic to this thread.
        handle.stop_and_join();

        let index = store.list_event_index(&created.id).unwrap();
        assert!(
            index.iter().any(|e| e.summary == "before panic"),
            "the write from before the injected panic must still be durable"
        );

        let w = writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.append_event(Event {
            session_id: created.id.clone(),
            ts_ms: 1,
            kind: EventKind::Lifecycle,
            step: None,
            summary: "after panic".into(),
            body_json: None,
            blob_hash: None,
            blob_size: None,
            correlates: None,
        })
        .expect("the writer must still accept appends after the watcher thread panicked");
    }

    /// Shutdown capture (FR-1.4 robustness on macOS FSEvents): a write
    /// performed immediately before `stop_and_join` must still be recorded.
    /// On macOS, FSEvents can deliver a write event AFTER the process that
    /// performed it has exited — or never deliver it at all on GitHub's macOS
    /// runners. The old behavior (stop the worker the instant `stop` is set,
    /// then flush only what was already pending) dropped that event, so a
    /// quick-exiting agent finalized with "0 file changes". Two mechanisms now
    /// cover it: the grace drain in [`run_loop`] catches *late* events, and the
    /// [`rescan_for_missed_changes`] backstop catches the *absent*-event case
    /// deterministically (it does not depend on `notify` delivering anything).
    ///
    /// This test is deterministic, not a flaky race, because the startup
    /// baseline is captured synchronously in [`spawn_watcher`] before it
    /// returns: `race.txt` is written AFTER `spawn_watcher` returns, so it is
    /// guaranteed post-baseline and the backstop records it as a `Created`
    /// even when `notify` delivers nothing. (Earlier, when the baseline scan
    /// ran on the worker thread, macOS scheduling could run it AFTER the write
    /// and fold `race.txt` into the baseline — the backstop then saw it as
    /// "unchanged" and skipped it, producing the `events seen: []` flake.)
    #[test]
    fn watcher_captures_write_then_immediate_stop() {
        use hh_core::event::{AdapterStatus, AgentKind, EventKind, NewSession};
        use hh_core::store::Store;

        // Serialize with `watcher_panic_does_not_abort_recording`: that test
        // arms the process-global `INJECT_PANIC_FOR_TEST` flag, which OUR worker
        // would observe if it ran concurrently and panic — aborting before the
        // backstop runs and turning this into the macOS-CI `events seen: []`
        // flake. Hold the guard so the flag is only seen by that test's worker.
        let _real_run_guard = REAL_RUN_LOOP_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let created = store
            .create_session(&NewSession {
                id: hh_core::event::now_v7(),
                started_at: 0,
                agent_kind: AgentKind::Generic,
                adapter_status: AdapterStatus::None,
                command: vec!["test".into()],
                cwd: cwd.clone(),
                hostname: None,
                hh_version: "test".into(),
                model: None,
                git_branch: None,
                git_sha: None,
                git_dirty: None,
            })
            .unwrap();
        let writer = Arc::new(Mutex::new(store.event_writer().unwrap()));
        let blobs = Arc::new(BlobStore::new(store.blobs().root().to_path_buf()));

        let opts = WatchOptions {
            cwd: cwd.clone(),
            max_file_size: 4 * 1024 * 1024,
            record_binary: false,
            extra_ignore: Vec::new(),
            internal_exclude: Vec::new(),
        };
        let handle = spawn_watcher(
            opts,
            Arc::clone(&writer),
            Arc::clone(&blobs),
            created.id.clone(),
            Instant::now(),
        )
        .unwrap()
        .expect("watcher should initialize on a writable temp dir");

        // Write a file and stop the watcher immediately — the FSEvents event
        // for this write may not have been delivered (or never will be).
        std::fs::write(cwd.join("race.txt"), b"written just before stop\n").unwrap();
        handle.stop_and_join();

        let index = store.list_event_index(&created.id).unwrap();
        assert!(
            index
                .iter()
                .any(|e| { e.kind == EventKind::FileChange && e.summary.contains("race.txt") }),
            "a write immediately before stop must be captured (grace drain + \
             rescan backstop); events seen: {:?}",
            index
                .iter()
                .map(|e| (e.kind, e.summary.clone()))
                .collect::<Vec<_>>()
        );
    }

    /// Deterministic unit test for [`rescan_for_missed_changes`] — the backstop
    /// that catches file changes the watcher never observed. Unlike
    /// [`watcher_captures_write_then_immediate_stop`] this does NOT spawn a
    /// real `notify` watcher, so it is fully deterministic and not subject to
    /// macOS FSEvents flakiness: it calls the backstop directly with a
    /// hand-built `existing` baseline (path → startup hash) and `known` set.
    /// Asserts:
    /// - an unobserved new file is recorded as `Created`;
    /// - a pre-existing file whose current hash differs from its baseline is
    ///   recorded as `Modified` (the missed-modify case the hash baseline
    ///   enables);
    /// - an unchanged baseline file and an already-recorded file are NOT
    ///   re-recorded (no duplicates).
    #[test]
    fn rescan_for_missed_changes_records_unobserved_creates_and_modifies() {
        use hh_core::event::{AdapterStatus, AgentKind, EventKind, NewSession};
        use hh_core::store::Store;

        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let created = store
            .create_session(&NewSession {
                id: hh_core::event::now_v7(),
                started_at: 0,
                agent_kind: AgentKind::Generic,
                adapter_status: AdapterStatus::None,
                command: vec!["test".into()],
                cwd: cwd.clone(),
                hostname: None,
                hh_version: "test".into(),
                model: None,
                git_branch: None,
                git_sha: None,
                git_dirty: None,
            })
            .unwrap();
        let writer = Arc::new(Mutex::new(store.event_writer().unwrap()));
        let blobs = Arc::new(BlobStore::new(store.blobs().root().to_path_buf()));

        // baseline.txt: existed before the watch, unchanged since. Its baseline
        // hash equals its current content hash → must NOT be recorded.
        let baseline_bytes = b"already here\n";
        std::fs::write(cwd.join("baseline.txt"), baseline_bytes).unwrap();
        // changed.txt: existed before the watch (baseline hash of "original\n")
        // but was modified during the session to "modified\n" with no event
        // observed → must be recorded as `Modified`.
        std::fs::write(cwd.join("changed.txt"), b"modified\n").unwrap();
        // recorded.txt: the watcher already captured it → in `known`. Must NOT
        // be re-recorded.
        std::fs::write(cwd.join("recorded.txt"), b"captured via event\n").unwrap();
        // missed.txt: created during the session but the watcher never saw an
        // event for it → not in `existing`, not in `known`. MUST be recorded as
        // `Created`.
        std::fs::write(cwd.join("missed.txt"), b"the watcher missed this\n").unwrap();

        // `existing` is the startup baseline: path → content hash. For
        // baseline.txt that is the hash of its (unchanged) content; for
        // changed.txt it is the hash of the ORIGINAL content ("original\n"),
        // which differs from what is on disk now.
        let existing: HashMap<PathBuf, String> = [
            (cwd.join("baseline.txt"), BlobStore::hash(baseline_bytes)),
            (cwd.join("changed.txt"), BlobStore::hash(b"original\n")),
        ]
        .into_iter()
        .collect();
        // `known` holds path → content hash; the hash value is irrelevant for
        // the backstop's skip check (it only tests key presence), so a dummy
        // hash is fine.
        let mut known: HashMap<PathBuf, String> = [(cwd.join("recorded.txt"), "dummyhash".into())]
            .into_iter()
            .collect();

        let opts = WatchOptions {
            cwd: cwd.clone(),
            max_file_size: 4 * 1024 * 1024,
            record_binary: false,
            extra_ignore: Vec::new(),
            internal_exclude: Vec::new(),
        };
        let matcher = build_matcher(&cwd, &[]).unwrap();
        rescan_for_missed_changes(
            &opts,
            &matcher,
            &writer,
            &blobs,
            &created.id,
            Instant::now(),
            &mut known,
            &existing,
        );

        let index = store.list_event_index(&created.id).unwrap();
        let summaries: Vec<String> = index.iter().map(|e| e.summary.clone()).collect();
        assert!(
            index
                .iter()
                .any(|e| e.kind == EventKind::FileChange && e.summary.contains("missed.txt")),
            "the unobserved new file must be recorded by the backstop; events: {summaries:?}"
        );
        assert!(
            index.iter().any(|e| {
                e.kind == EventKind::FileChange
                    && e.summary.contains("changed.txt")
                    && e.summary.contains("modified")
            }),
            "a baseline file whose current hash differs must be recorded as Modified; events: {summaries:?}"
        );
        assert!(
            !summaries.iter().any(|s| s.contains("baseline.txt")),
            "an unchanged baseline file must not be recorded; events: {summaries:?}"
        );
        assert!(
            !summaries.iter().any(|s| s.contains("recorded.txt")),
            "an already-recorded file must not be duplicated; events: {summaries:?}"
        );
    }

    /// FR-1.5 best-effort: when the cwd itself cannot be watched (here: it does
    /// not exist), `spawn_watcher` must return `Ok(None)` — degrading
    /// file-change recording with a warning — rather than `Err`, which would
    /// abort the whole recording. The recorder continues with PTY + adapter
    /// capture. (The accompanying stderr warning is asserted end-to-end in the
    /// `hh/tests` integration suite.)
    #[test]
    fn spawn_watcher_degrades_when_cwd_unwatchable() {
        use hh_core::store::Store;
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let writer = Arc::new(Mutex::new(store.event_writer().unwrap()));
        let blobs = Arc::new(BlobStore::new(store.blobs().root().to_path_buf()));
        let opts = WatchOptions {
            // A path that does not exist and cannot be watched.
            cwd: tmp.path().join("does-not-exist"),
            max_file_size: 4 * 1024 * 1024,
            record_binary: false,
            extra_ignore: Vec::new(),
            internal_exclude: Vec::new(),
        };
        let outcome = spawn_watcher(opts, writer, blobs, "session".into(), Instant::now());
        assert!(
            outcome.is_ok(),
            "watcher init failure must not abort the recording"
        );
        assert!(
            outcome.unwrap().is_none(),
            "an unwatchable cwd must degrade to Ok(None), not a handle"
        );
    }

    /// The per-directory fallback walk skips ignored directories (built-in
    /// `.git`/`node_modules`/`target`) so it does not waste watches on — or
    /// descend into — the very subtrees the matcher would filter anyway. This
    /// exercises `add_per_dir_watches` directly against a real `notify` watcher.
    #[test]
    fn add_per_dir_watches_skips_ignored_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        std::fs::create_dir_all(cwd.join("keep")).unwrap();
        std::fs::create_dir_all(cwd.join("node_modules").join("deep")).unwrap();
        std::fs::create_dir_all(cwd.join("target")).unwrap();
        let matcher = build_matcher(cwd, &[]).unwrap();
        let (ntx, _nrx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(
            move |r: std::result::Result<notify::Event, notify::Error>| {
                let _ = ntx.send(r);
            },
        )
        .expect("notify watcher init");
        // Must report the cwd as watched (at least one dir) and not error.
        assert!(add_per_dir_watches(&mut watcher, cwd, &matcher, &[]));
    }
}
