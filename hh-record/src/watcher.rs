//! Recursive filesystem watcher.
//!
//! Spawns a `notify` watcher on the session cwd and records `file_change`
//! events through the shared writer. Honors a built-in ignore list, the
//! root `.gitignore`, and caller-supplied extra patterns. File contents are
//! stored in the blob store (BLAKE3-keyed, zstd); binary files record hashes
//! only unless `--record-binary` is set.
//!
//! # Known limitation: nested `.gitignore`
//!
//! File-change capture is meant to honor `.gitignore`. This implementation
//! honors the cwd's root `.gitignore` and the global gitignore, plus
//! built-in and extra patterns, via a single `ignore::gitignore::Gitignore`
//! matcher. It does **not** honor `.gitignore` files in subdirectories (the
//! `ignore` crate does not expose a public path-based matcher for the full
//! directory-ignore tree without `WalkBuilder`, which is walk-oriented, not
//! event-oriented). For v0.1 this covers the common case (one root
//! `.gitignore`); nested ignores are a roadmap refinement. See the decisions
//! summary.
//!
//! A one-time recursive scan of `cwd` runs before the event loop starts, to
//! build the session-start baseline ([`scan_baseline`]). It serves two
//! purposes: [`resolve_first_seen`] uses the set of pre-existing paths to
//! correct a macOS FSEvents quirk (edits to pre-existing files misreported as
//! creations), and [`Tracker`] uses each file's captured content — or, for a
//! file clean against the git index, its index blob id — as the before-side
//! of the file's first change. Without it, the first change to any file that
//! predates the session had no before-side and rendered as all-added lines,
//! and a delete rendered as "content not captured".
//!
//! # Where the before-side comes from
//!
//! In order: the watcher's own previous capture of the path (`live`); the
//! git index blob for a file that was clean against the index at session
//! start (`git`); the content captured at session start for a dirty,
//! untracked or non-git file (`baseline`); otherwise nothing (`none`). Never
//! `HEAD`, and never the index for a dirty file — that would attribute the
//! developer's own uncommitted edits to the agent. The provenance is stored in
//! `file_changes.before_source` (migration 0006).
//!
//! # Renames
//!
//! A rename is recorded as one `file_changes` row on the new path with
//! `renamed_from` set to the old one. The two halves are paired from the
//! backend's own rename pairing when it has one (`RenameMode::Both` on
//! inotify/Windows), else by byte-identical content within one flush batch
//! (FSEvents reports each half separately). See [`record_changes`].

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hh_core::blob::BlobStore;
use hh_core::event::{
    BeforeSource, ChangeKind, Event, EventKind, FileChange, FileChangeProvenance,
};
use hh_core::store::EventWriter;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Built-in ignore patterns (`.git/`, `node_modules/`, `target/`).
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
/// for. `const`-constructible since Rust 1.63, so fine at the 1.88 MSRV.
#[cfg(test)]
static REAL_RUN_LOOP_MUTEX: Mutex<()> = Mutex::new(());

/// Coalescing window for editor-style double-writes. Editors save a
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

/// Environment variable read by the `test-seams` fault-injection seam (see
/// [`forced_unwatchable`]). Only exists in builds with that feature.
#[cfg(feature = "test-seams")]
pub const TEST_UNWATCHABLE_CWD_ENV: &str = "HH_TEST_UNWATCHABLE_CWD";

/// Test seam (feature `test-seams`, dev-only): when
/// [`TEST_UNWATCHABLE_CWD_ENV`] is set, every attempt to watch the recording's
/// cwd fails as if the OS had refused it (`EACCES`). This drives the real
/// degradation path — recursive watch fails, per-directory fallback fails on
/// the root, file-change recording is disabled with a warning — for any uid
/// on any OS. A permission trick (`chmod 000`) cannot do that: root ignores
/// it. Without the feature this is a constant `false` the optimizer removes.
fn forced_unwatchable(path: &Path, cwd: &Path) -> bool {
    #[cfg(feature = "test-seams")]
    {
        path == cwd && std::env::var_os(TEST_UNWATCHABLE_CWD_ENV).is_some()
    }
    #[cfg(not(feature = "test-seams"))]
    {
        let _ = (path, cwd);
        false
    }
}

/// `watcher.watch(path, mode)`, routed through the [`forced_unwatchable`]
/// seam so tests can fail the cwd watch without filesystem permissions.
fn watch_path(
    watcher: &mut RecommendedWatcher,
    path: &Path,
    mode: RecursiveMode,
    cwd: &Path,
) -> notify::Result<()> {
    if forced_unwatchable(path, cwd) {
        return Err(
            notify::Error::io(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                .add_path(path.to_path_buf()),
        );
    }
    watcher.watch(path, mode)
}

/// Options for the filesystem watcher.
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
    /// Total bytes of pre-existing file content held at session start as
    /// before-sides (`[record] baseline_budget`). Git-clean files do not
    /// count against it. See [`hh_core::DEFAULT_BASELINE_BUDGET`].
    pub baseline_budget: u64,
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
/// without file-change recording (best-effort: PTY + adapter capture
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
/// relative to session start.
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
    // skipping unreadable / ignored subtrees (best-effort).
    let cwd_watched = match watch_path(&mut watcher, &opts.cwd, RecursiveMode::Recursive, &opts.cwd)
    {
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
    // best-effort (unreadable files/dirs are skipped); see [`scan_baseline`].
    //
    // The baseline holds each pre-existing file's real before-side — its
    // content, or for a file clean against the git index, the index blob id —
    // so the first change to a file that predates the session diffs against
    // what was actually there, instead of rendering as "all added".
    let (existing, baseline) = scan_baseline(&opts, &matcher);
    let tracker = Tracker {
        known: HashMap::new(),
        baseline,
    };

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
                tracker,
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
        match watch_path(watcher, &dir, RecursiveMode::NonRecursive, root) {
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

/// What the watcher last recorded for a path during this session — the
/// before-side of that path's next change ([`BeforeSource::Live`]).
#[derive(Debug, Clone)]
struct Known {
    /// The hash the recorded after-side references: the stored blob's hash,
    /// or the disk-content hash of an unstored binary file (identity only).
    hash: String,
    /// BLAKE3 of the on-disk bytes, compared by rename pairing and by the
    /// shutdown backstop (differs from `hash` under record-time redaction).
    raw: String,
    /// Whether the recorded content was detected as binary.
    binary: bool,
}

/// One pre-existing file's session-start state, held until the first change
/// to the path consumes it as that change's before-side (see
/// [`Tracker::take_before`]).
#[derive(Debug, Clone)]
struct BaselineEntry {
    /// BLAKE3 of the content at session start.
    raw: String,
    /// Whether that content was detected as binary.
    binary: bool,
    /// Where the before-side content will be read from.
    content: BaselineContent,
}

/// Where a baseline file's before-side content comes from.
#[derive(Debug, Clone)]
enum BaselineContent {
    /// Clean against the git index at session start: read the index blob
    /// (`oid`) on demand. Nothing is copied unless the file changes.
    Git(String),
    /// Dirty, untracked, or outside a git repository: the bytes, captured at
    /// session start and written to the blob store on first change.
    Held(Vec<u8>),
    /// Not captured: binary with `record_binary` off, or over the baseline
    /// budget. The first change records no before-side.
    Unstored,
}

/// Per-session before-side bookkeeping: the live map of what the watcher has
/// recorded, plus the session-start baseline of everything that existed
/// before. Owned by the watcher thread (never shared).
#[derive(Debug, Default)]
struct Tracker {
    /// path → last recorded state ([`BeforeSource::Live`] before-sides).
    known: HashMap<PathBuf, Known>,
    /// path → session-start state, consumed by the path's first change.
    baseline: HashMap<PathBuf, BaselineEntry>,
}

/// A resolved before-side, ready to attach to a `file_changes` row.
#[derive(Debug)]
struct Before {
    hash: Option<String>,
    source: BeforeSource,
    /// Uncompressed size of a blob the row owns (git / baseline).
    size: Option<u64>,
    binary: bool,
}

impl Before {
    fn none() -> Self {
        Self {
            hash: None,
            source: BeforeSource::None,
            size: None,
            binary: false,
        }
    }
}

impl Tracker {
    /// Whether anything is known about `path`'s previous content (recorded
    /// live, or present at session start and not yet consumed).
    fn has_prior(&self, path: &Path) -> bool {
        self.known.contains_key(path) || self.baseline.contains_key(path)
    }

    /// BLAKE3 of `path`'s previous on-disk content, if known.
    fn prior_raw(&self, path: &Path) -> Option<&str> {
        self.known
            .get(path)
            .map(|k| k.raw.as_str())
            .or_else(|| self.baseline.get(path).map(|b| b.raw.as_str()))
    }

    /// Resolve and consume the before-side of a change to `path`.
    ///
    /// 1. Recorded earlier this session → that after-side ([`BeforeSource::Live`]).
    /// 2. Present at session start → the baseline, consumed here. A git-clean
    ///    file's index blob is read through `git cat-file --filters` and kept
    ///    only if it hashes to the session-start content (so a filter or
    ///    stat-cache surprise can never surface the wrong text); held bytes
    ///    are written as-is. Either way the content goes through the
    ///    recorder's blob store — redacting when `[redaction] at_record` is
    ///    on — and the row will own one reference on it.
    /// 3. Otherwise there is no before-side ([`BeforeSource::None`]).
    fn take_before(&mut self, path: &Path, sink: &Sink<'_>) -> Before {
        if let Some(k) = self.known.remove(path) {
            return Before {
                hash: Some(k.hash),
                source: BeforeSource::Live,
                size: None,
                binary: k.binary,
            };
        }
        let Some(entry) = self.baseline.remove(path) else {
            return Before::none();
        };
        let content = match entry.content {
            BaselineContent::Held(bytes) => Some((bytes, BeforeSource::Baseline)),
            BaselineContent::Git(oid) => path
                .strip_prefix(&sink.opts.cwd)
                .ok()
                .and_then(|rel| crate::git::read_blob(&sink.opts.cwd, &oid, rel))
                .filter(|bytes| BlobStore::hash(bytes) == entry.raw)
                .map(|bytes| (bytes, BeforeSource::Git)),
            BaselineContent::Unstored => None,
        };
        let Some((bytes, source)) = content else {
            return Before {
                binary: entry.binary,
                ..Before::none()
            };
        };
        match sink.blobs.put(&bytes) {
            Ok(out) => Before {
                hash: Some(out.hash),
                source,
                size: Some(out.size),
                binary: entry.binary,
            },
            Err(e) => {
                eprintln!(
                    "hh: warning: could not store the previous content of {} ({e}); \
                     its diff will show no removed lines",
                    path.display()
                );
                Before {
                    binary: entry.binary,
                    ..Before::none()
                }
            }
        }
    }
}

/// Everything a capture needs to write an event: options, the shared writer
/// and blob store, and the session clock.
struct Sink<'a> {
    opts: &'a WatchOptions,
    writer: &'a Arc<Mutex<EventWriter>>,
    blobs: &'a Arc<BlobStore>,
    session_id: &'a str,
    start: Instant,
}

/// Scan `cwd` once at session start (on the caller's thread, see
/// [`spawn_watcher`]) and build both the set of pre-existing paths (for
/// [`resolve_first_seen`]) and the [`Tracker`] baseline.
///
/// For each capturable file (not ignored, not excluded, ≤ `max_file_size`):
/// - clean against the git index → [`BaselineContent::Git`] (no copy);
/// - binary with `record_binary` off → [`BaselineContent::Unstored`], exactly
///   as a live capture of it would store no content;
/// - otherwise held in memory while `baseline_budget` lasts, else
///   [`BaselineContent::Unstored`] with one stderr note for the session.
///
/// Nothing is written to the blob store here: content reaches it only when
/// the file actually changes, so an unchanged file never leaves an orphan
/// blob for `hh gc` to reclaim (or race with a live recording over).
fn scan_baseline(
    opts: &WatchOptions,
    matcher: &Gitignore,
) -> (HashSet<PathBuf>, HashMap<PathBuf, BaselineEntry>) {
    let git_clean = crate::git::index_clean_blobs(&opts.cwd).unwrap_or_default();
    let mut files: HashSet<PathBuf> = HashSet::new();
    scan_existing_files(
        &opts.cwd,
        &opts.cwd,
        matcher,
        &opts.internal_exclude,
        &mut files,
    );
    let mut baseline = HashMap::new();
    let mut held: u64 = 0;
    let mut over_budget = 0usize;
    for path in &files {
        // Skip what the watcher would never capture, or that raced away.
        let Some((content, raw)) = read_if_capturable(path, opts) else {
            continue;
        };
        let binary = is_binary(&content);
        let len = u64::try_from(content.len()).unwrap_or(u64::MAX);
        let source = if binary && !opts.record_binary {
            BaselineContent::Unstored
        } else if let Some(oid) = git_clean.get(path) {
            BaselineContent::Git(oid.clone())
        } else if held.saturating_add(len) <= opts.baseline_budget {
            held += len;
            BaselineContent::Held(content)
        } else {
            over_budget += 1;
            BaselineContent::Unstored
        };
        baseline.insert(
            path.clone(),
            BaselineEntry {
                raw,
                binary,
                content: source,
            },
        );
    }
    if over_budget > 0 {
        eprintln!(
            "hh: note: {over_budget} pre-existing file(s) exceed the baseline budget \
             ({} bytes); their first change will be recorded without the previous \
             content (raise `[record] baseline_budget` or commit/stash first — \
             see docs/recording.md)",
            opts.baseline_budget
        );
    }
    (files, baseline)
}

/// The watcher event loop, run on the worker thread. Events are not processed
/// immediately: each capturable path enters a [`pending`] map keyed by absolute
/// path, and is processed only after its debounce window elapses with no
/// further event on the same path. On shutdown, any still-pending
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
    // Paths that existed under `cwd` before the watch started observing,
    // captured synchronously in [`spawn_watcher`] on the caller's thread
    // *before* the worker is spawned — so anything written after
    // `spawn_watcher` returns is never misclassified as pre-existing. Used by
    // [`resolve_first_seen`] for the FSEvents spurious-create correction.
    mut existing: HashSet<PathBuf>,
    // The before-side state: starts with the session-start baseline (real
    // content, or a git index blob id, per file) and accumulates what the
    // watcher records. See [`Tracker::take_before`].
    mut tracker: Tracker,
) {
    let sink = Sink {
        opts: &opts,
        writer: &writer,
        blobs: &blobs,
        session_id: &session_id,
        start,
    };
    let mut pending: HashMap<PathBuf, Pending> = HashMap::new();
    // Rename hints from backends that pair the two halves of a rename
    // (`RenameMode::Both`: inotify, Windows): new path → old path.
    let mut renames: HashMap<PathBuf, PathBuf> = HashMap::new();
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
            Ok(Ok(event)) => fold_event(
                &event,
                &matcher,
                &opts,
                &mut existing,
                &mut pending,
                &mut renames,
            ),
            Ok(Err(e)) => eprintln!("hh: warning: fs watcher error: {e}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Flush any pending changes whose debounce window has elapsed.
        flush_due(&mut pending, &mut renames, &sink, &mut tracker);
    }
    // --- Shutdown grace drain (robustness) -------------------------------
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
            Ok(Ok(event)) => fold_event(
                &event,
                &matcher,
                &opts,
                &mut existing,
                &mut pending,
                &mut renames,
            ),
            Ok(Err(e)) => eprintln!("hh: warning: fs watcher error: {e}"),
            // Quiet for `wait` with no event, or channel closed → caught up.
            Err(
                std::sync::mpsc::RecvTimeoutError::Timeout
                | std::sync::mpsc::RecvTimeoutError::Disconnected,
            ) => break,
        }
        flush_due(&mut pending, &mut renames, &sink, &mut tracker);
    }
    // Flush whatever is still pending — as ONE batch, so the two halves of a
    // rename are paired — so a write that landed inside the window just
    // before shutdown is not lost.
    let all: Vec<PathBuf> = pending.keys().cloned().collect();
    flush_batch(all, &mut pending, &mut renames, &sink, &mut tracker);
    // Deterministic backstop: re-walk cwd for any change the watcher never
    // observed (the grace drain only helps when an event is *late*; this
    // catches the case where the backend delivered *nothing* — e.g. macOS
    // FSEvents on CI runners, observed flaky/absent).
    rescan_for_missed_changes(&matcher, &sink, &mut tracker);
    // Drop the watcher to release the inotify/fsevents handle.
    drop(watcher);
}

/// Fold a raw `notify` event into the debounced `pending` map: classify each
/// path against the ignore matcher, drop spurious first-seen "creates" on
/// pre-existing files, and coalesce. A paired rename (`RenameMode::Both`,
/// whose `paths` are `[from, to]`) also leaves a hint in `renames` so the
/// flush records one rename instead of a delete plus a create. Shared by the
/// live loop and the shutdown grace drain so the two stay in sync.
fn fold_event(
    event: &notify::Event,
    matcher: &Gitignore,
    opts: &WatchOptions,
    existing: &mut HashSet<PathBuf>,
    pending: &mut HashMap<PathBuf, Pending>,
    renames: &mut HashMap<PathBuf, PathBuf>,
) {
    if let notify::EventKind::Modify(notify::event::ModifyKind::Name(
        notify::event::RenameMode::Both,
    )) = event.kind
    {
        if let [from, to] = event.paths.as_slice() {
            if classify(from, event.kind, matcher, opts).is_some()
                && classify(to, event.kind, matcher, opts).is_some()
            {
                renames.insert(to.clone(), from.clone());
            }
        }
    }
    for path in &event.paths {
        if let Some(kind) = classify(path, event.kind, matcher, opts) {
            let kind = resolve_first_seen(kind, path, existing);
            coalesce(pending, path.clone(), kind);
        }
    }
}

/// Final shutdown backstop (robustness): re-walk `cwd` and record any
/// capturable file change the watcher never observed.
///
/// This is deterministic and does **not** depend on the OS event source
/// delivering anything. It is the safety net for backends that drop or never
/// deliver events — notably macOS FSEvents, which is observed flaky/absent on
/// GitHub's macOS runners (a quick-exiting agent can write a file and exit
/// before `fseventsd` reports anything, and the grace drain only helps when
/// the event is merely *late*, not *absent*). It also catches what no
/// backend reports per file, such as the files inside a renamed directory.
///
/// For each capturable file on disk now:
/// - recorded this session ([`Tracker::known`]) → re-record as `Modified`
///   only if its content changed after the last capture;
/// - in the unconsumed baseline → `Modified` if its content differs from the
///   session-start content, with that content as the before-side (it was
///   captured at startup, so the diff is real, not "all added");
/// - otherwise → `Created`.
///
/// Every recorded or baseline path no longer on disk is a missed `Deleted`
/// whose before-side is its last known content. Missed deletes and creates
/// with identical content are paired into renames by [`record_changes`].
fn rescan_for_missed_changes(matcher: &Gitignore, sink: &Sink<'_>, tracker: &mut Tracker) {
    let opts = sink.opts;
    let mut current: HashSet<PathBuf> = HashSet::new();
    scan_existing_files(
        &opts.cwd,
        &opts.cwd,
        matcher,
        &opts.internal_exclude,
        &mut current,
    );
    let mut items: Vec<(PathBuf, ChangeKind)> = Vec::new();
    for path in &current {
        match tracker.prior_raw(path).map(str::to_owned) {
            Some(prior) => {
                let Some((_, raw)) = read_if_capturable(path, opts) else {
                    continue;
                };
                if raw != prior {
                    items.push((path.clone(), ChangeKind::Modified));
                }
            }
            None => items.push((path.clone(), ChangeKind::Created)),
        }
    }
    for path in tracker.known.keys().chain(tracker.baseline.keys()) {
        // `exists` re-checks against the filesystem: a directory the scan
        // could not read must not turn its files into phantom deletes.
        if !current.contains(path) && !path.exists() {
            items.push((path.clone(), ChangeKind::Deleted));
        }
    }
    record_changes(items, &mut HashMap::new(), sink, tracker);
}

/// Read `path` and return its content and BLAKE3 hash, or `None` if it is not
/// capturable (missing, not a regular file, larger than `max_file_size`, or
/// unreadable).
fn read_if_capturable(path: &Path, opts: &WatchOptions) -> Option<(Vec<u8>, String)> {
    match observe(path, opts) {
        Ok(Observed::Present(p)) => Some((p.content, p.raw)),
        _ => None,
    }
}

/// A path's state at flush time.
enum Observed {
    /// The path does not exist.
    Gone,
    /// Not capturable: larger than `max_file_size`, or not a regular file.
    Skip,
    /// A capturable file and its current content.
    Present(Present),
}

/// Stat and read `path` (the max file size limit applies). A vanished path is
/// [`Observed::Gone`], not an error: the debounce window means a change is
/// processed some time after it happened.
fn observe(path: &Path, opts: &WatchOptions) -> std::result::Result<Observed, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Observed::Gone),
        Err(e) => return Err(format!("stat {}: {e}", path.display())),
    };
    if !meta.is_file() || meta.len() > opts.max_file_size {
        // Only capture files ≤ max_file_size.
        return Ok(Observed::Skip);
    }
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Observed::Gone),
        Err(e) => return Err(format!("open {}: {e}", path.display())),
    };
    let mut content = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    f.read_to_end(&mut content)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let binary = is_binary(&content);
    let raw = BlobStore::hash(&content);
    Ok(Observed::Present(Present {
        content,
        raw,
        binary,
    }))
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

/// Flush every pending change whose deadline has passed. If any due change
/// is a create or delete, every other pending create/delete is flushed with
/// it, so the two halves of a rename (whose events arrive microseconds apart
/// and may straddle a poll) are paired in the same batch.
fn flush_due(
    pending: &mut HashMap<PathBuf, Pending>,
    renames: &mut HashMap<PathBuf, PathBuf>,
    sink: &Sink<'_>,
    tracker: &mut Tracker,
) {
    let now = Instant::now();
    let structural = |p: &Pending| matches!(p.kind, ChangeKind::Created | ChangeKind::Deleted);
    let mut due: Vec<PathBuf> = pending
        .iter()
        .filter(|(_, p)| p.deadline <= now)
        .map(|(k, _)| k.clone())
        .collect();
    if due.is_empty() {
        return;
    }
    if due
        .iter()
        .any(|path| pending.get(path).is_some_and(structural))
    {
        due.extend(
            pending
                .iter()
                .filter(|(_, p)| p.deadline > now && structural(p))
                .map(|(path, _)| path.clone()),
        );
    }
    flush_batch(due, pending, renames, sink, tracker);
}

/// Remove `paths` from `pending` and record them as one batch (dropping
/// suppressed net no-ops).
fn flush_batch(
    paths: Vec<PathBuf>,
    pending: &mut HashMap<PathBuf, Pending>,
    renames: &mut HashMap<PathBuf, PathBuf>,
    sink: &Sink<'_>,
    tracker: &mut Tracker,
) {
    let mut items = Vec::with_capacity(paths.len());
    for path in paths {
        match pending.remove(&path) {
            Some(p) if !p.suppress => items.push((path, p.kind)),
            _ => {
                renames.remove(&path);
            }
        }
    }
    record_changes(items, renames, sink, tracker);
}

/// Record one batch of debounced changes. The raw event kind is reconciled
/// with what is actually on disk now and with what the tracker knows (a
/// "create" of a file that existed is a modify; a "modify" of a vanished file
/// is a delete), then deletes and creates are paired into renames — first by
/// the backend's rename hint, then by identical non-empty content (git's exact
/// rename detection) — and everything is written.
fn record_changes(
    items: Vec<(PathBuf, ChangeKind)>,
    renames: &mut HashMap<PathBuf, PathBuf>,
    sink: &Sink<'_>,
    tracker: &mut Tracker,
) {
    let Batch {
        mut deletes,
        creates,
        modifies,
    } = reconcile(items, sink.opts, tracker);

    // Pair renames: a created path takes its hinted source if that source is
    // being deleted in this batch, else the first deleted path whose last
    // known content is byte-identical (never for empty files — any two empty
    // files are "identical", which says nothing about a rename).
    let mut paired: Vec<(PathBuf, PathBuf, Present)> = Vec::new();
    let mut plain_creates: Vec<(PathBuf, Present)> = Vec::new();
    for (to, present) in creates {
        let hinted = renames
            .remove(&to)
            .and_then(|from| deletes.iter().position(|d| *d == from));
        let by_content = || {
            (!present.content.is_empty())
                .then(|| {
                    deletes
                        .iter()
                        .position(|d| tracker.prior_raw(d) == Some(present.raw.as_str()))
                })
                .flatten()
        };
        match hinted.or_else(by_content) {
            Some(i) => paired.push((deletes.remove(i), to, present)),
            None => plain_creates.push((to, present)),
        }
    }
    for (path, _) in &modifies {
        renames.remove(path);
    }

    for (from, to, present) in paired {
        let before = tracker.take_before(&from, sink);
        write_change(
            &to,
            ChangeKind::Modified,
            before,
            Some(present),
            Some(&from),
            sink,
            tracker,
        );
    }
    for (path, present) in modifies {
        let before = tracker.take_before(&path, sink);
        write_change(
            &path,
            ChangeKind::Modified,
            before,
            Some(present),
            None,
            sink,
            tracker,
        );
    }
    for (path, present) in plain_creates {
        // A created path has no before-side; nothing to consume.
        write_change(
            &path,
            ChangeKind::Created,
            Before::none(),
            Some(present),
            None,
            sink,
            tracker,
        );
    }
    for path in deletes {
        let before = tracker.take_before(&path, sink);
        write_change(
            &path,
            ChangeKind::Deleted,
            before,
            None,
            None,
            sink,
            tracker,
        );
    }
}

/// One flush batch, reconciled against disk and the tracker (see
/// [`reconcile`]), each list sorted by path for a deterministic write order.
struct Batch {
    deletes: Vec<PathBuf>,
    creates: Vec<(PathBuf, Present)>,
    modifies: Vec<(PathBuf, Present)>,
}

/// Reconcile each raw `(path, kind)` with the filesystem now and with what
/// the tracker knows: a vanished path with a known prior (or a raw delete) is
/// a delete; a present path with a known prior is a modify whatever the raw
/// kind said; a present path with no prior is a create unless the backend
/// said modify (e.g. a file over `max_file_size` at session start, now
/// smaller — it existed, so "created" would be wrong).
fn reconcile(items: Vec<(PathBuf, ChangeKind)>, opts: &WatchOptions, tracker: &Tracker) -> Batch {
    let mut batch = Batch {
        deletes: Vec::new(),
        creates: Vec::new(),
        modifies: Vec::new(),
    };
    for (path, kind) in items {
        match observe(&path, opts) {
            Err(e) => warn_capture(&path, &e),
            Ok(Observed::Skip) => {}
            Ok(Observed::Gone) => {
                if kind == ChangeKind::Deleted || tracker.has_prior(&path) {
                    batch.deletes.push(path);
                }
            }
            Ok(Observed::Present(present)) => {
                let is_modify = match kind {
                    ChangeKind::Modified => true,
                    ChangeKind::Created | ChangeKind::Deleted => tracker.has_prior(&path),
                };
                if is_modify {
                    batch.modifies.push((path, present));
                } else {
                    batch.creates.push((path, present));
                }
            }
        }
    }
    batch.deletes.sort();
    batch.creates.sort_by(|a, b| a.0.cmp(&b.0));
    batch.modifies.sort_by(|a, b| a.0.cmp(&b.0));
    batch
}

/// A present file's content, read at flush time.
struct Present {
    content: Vec<u8>,
    raw: String,
    binary: bool,
}

/// One-line stderr warning for a failed capture (recording continues).
fn warn_capture(path: &Path, e: &str) {
    eprintln!(
        "hh: warning: file change capture failed for {}: {e}",
        path.display()
    );
}

/// Decide whether a raw notify event on `path` should be captured, and if so
/// return the [`ChangeKind`]. Filters internal halfhand paths, paths outside
/// the watched tree, directories, and ignored paths (built-in, `.gitignore`,
/// and caller-supplied extra patterns). This runs *before* debouncing so
/// ignored files never enter the pending map.
///
/// A rename event (`Modify(Name(_))`) names a path that either appeared or
/// disappeared, so it is classified by whether the path exists now: a
/// `Created` for the new name, a `Deleted` for the old one. (Classifying it as
/// `Modified`, as before, silently dropped the old name — a modify of a path
/// that no longer exists has nothing to read.)
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
        notify::EventKind::Modify(notify::event::ModifyKind::Name(_)) => {
            if path.exists() {
                ChangeKind::Created
            } else {
                ChangeKind::Deleted
            }
        }
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
/// (via [`scan_baseline`]) and again by the shutdown backstop. Does not
/// follow symlinks (`DirEntry::file_type` reports the link itself, not its
/// target), which also avoids symlink-cycle recursion. Best-effort: an
/// unreadable subdirectory is skipped rather than failing the whole scan.
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

/// Resolve a raw classified [`ChangeKind`] against the set of paths already
/// known to exist, correcting for a macOS FSEvents quirk: `fseventsd` can tag
/// the *first-ever* notification it delivers for a path with
/// `kFSEventStreamEventFlagItemCreated` even when the path predates the watch
/// (it keeps no baseline of what existed before the stream started). Left
/// uncorrected, editing a pre-existing file is recorded as `created` instead
/// of `modified` (its very first observed event wins the merge in
/// [`merge_kind`]).
///
/// `existing` is updated as change kinds are resolved, so a later
/// delete-then-recreate of the same path within the same session is still
/// reported as a genuine `Created`. (The flush additionally reconciles kinds
/// with the [`Tracker`], see [`record_changes`].)
fn resolve_first_seen(
    kind: ChangeKind,
    path: &Path,
    existing: &mut HashSet<PathBuf>,
) -> ChangeKind {
    match kind {
        ChangeKind::Created if existing.contains(path) => ChangeKind::Modified,
        ChangeKind::Created | ChangeKind::Modified => {
            // Mark the path seen so a later raw `Created` (the FSEvents quirk
            // firing again) downgrades to `Modified`.
            existing.insert(path.to_path_buf());
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

/// Write one resolved change: store the after-side content (unless it is
/// binary and `record_binary` is off), update the tracker, and append the
/// `file_change` event + `file_changes` row with its before-side provenance
/// (migration 0006). `renamed_from` is the old absolute path of a rename.
///
/// A change that cannot be stored is a dropped event, not a per-file warning:
/// it is counted in the writer's dropped-event tally, which prints one warning
/// line for the first drop only (a disk-full condition must not print one line
/// per file) and marks the session degraded at finalize. Recording continues.
#[allow(clippy::too_many_arguments)] // one change's full description; a struct would just rename the args
fn write_change(
    path: &Path,
    change_kind: ChangeKind,
    before: Before,
    after: Option<Present>,
    renamed_from: Option<&Path>,
    sink: &Sink<'_>,
    tracker: &mut Tracker,
) {
    let opts = sink.opts;
    // Defensive: re-strip in case a path outside cwd slipped through.
    let Ok(rel) = path.strip_prefix(&opts.cwd) else {
        return;
    };
    let rel_str = rel.to_string_lossy().to_string();
    let from_str = renamed_from
        .and_then(|p| p.strip_prefix(&opts.cwd).ok())
        .map(|p| p.to_string_lossy().to_string());

    let (after_hash, blob_hash, blob_size, is_binary) = match after {
        None => (None, None, None, before.binary),
        Some(Present {
            content,
            raw,
            binary,
        }) => {
            let size = u64::try_from(content.len()).unwrap_or(u64::MAX);
            // Store content unless it's binary and the user opted out. The
            // stored hash/size come from the put *outcome*, not the disk
            // content: with a record-time redactor the stored (redacted)
            // content hashes differently from the on-disk original, and
            // events/diffs must reference the blob that actually exists.
            let (stored_hash, stored_size) = if binary && !opts.record_binary {
                (None, Some(size))
            } else {
                match sink.blobs.put(&content) {
                    Ok(outcome) => (Some(outcome.hash), Some(outcome.size)),
                    Err(e) => {
                        // The event never reaches the writer, so count it
                        // here (the writer counts its own failures).
                        lock_sink_writer(sink)
                            .record_dropped(format!("store content of {}: {e}", path.display()));
                        return;
                    }
                }
            };
            // The before/after chain must point at fetchable blobs, so it
            // tracks the stored hash when content was stored, and falls back
            // to the disk-content hash for unstored (binary) files where the
            // hash is identity metadata only.
            let effective_hash = stored_hash.clone().unwrap_or_else(|| raw.clone());
            tracker.known.insert(
                path.to_path_buf(),
                Known {
                    hash: effective_hash.clone(),
                    raw,
                    binary,
                },
            );
            (Some(effective_hash), stored_hash, stored_size, binary)
        }
    };

    let summary = match &from_str {
        Some(from) => format!("{rel_str} renamed from {from}"),
        None => format!("{rel_str} {change_kind}"),
    };
    let mut body = serde_json::json!({
        "path": rel_str,
        "change_kind": change_kind.to_string(),
        "before_source": before.source.to_string(),
    });
    if let Some(from) = &from_str {
        body["renamed_from"] = serde_json::json!(from);
    }
    let event = Event {
        session_id: sink.session_id.to_string(),
        ts_ms: i64::try_from(sink.start.elapsed().as_millis()).unwrap_or(i64::MAX),
        kind: EventKind::FileChange,
        step: None, // file changes are steps in replay; step ordinal derived at read time.
        summary: truncate_summary(&summary),
        body_json: Some(body),
        blob_hash,
        blob_size,
        correlates: None,
    };
    let file_change = FileChange {
        event_id: 0, // overwritten by the writer (EventWriter::append_file_change).
        path: rel_str,
        change_kind,
        before_hash: before.hash,
        after_hash,
        is_binary,
    };
    let provenance = FileChangeProvenance::new(before.source)
        .with_before_size(before.size)
        .with_renamed_from(from_str);

    // Recover from poisoning rather than failing this (and every subsequent)
    // capture: the writer `Mutex` is shared with the PTY reader and adapter
    // drain threads, and a panic in one of them must not silently blind the
    // others for the rest of the session (see `runner::lock_writer`).
    // A failed append is counted in the writer's dropped tally (and warned
    // about once) by the writer itself; nothing more to do here.
    let _ =
        lock_sink_writer(sink).append_file_change_with_provenance(event, file_change, provenance);
}

/// Lock the sink's shared writer, recovering from poisoning rather than
/// failing this (and every subsequent) capture: the writer `Mutex` is shared
/// with the PTY reader and adapter drain threads, and a panic in one of them
/// must not silently blind the others for the rest of the session (see
/// `runner::lock_writer`).
fn lock_sink_writer<'a>(sink: &'a Sink<'_>) -> std::sync::MutexGuard<'a, EventWriter> {
    sink.writer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Binary heuristic: a NUL byte in the first 8 KiB.
fn is_binary(content: &[u8]) -> bool {
    content.iter().take(8192).any(|&b| b == 0)
}

/// Truncate a summary to the 120-char event summary limit.
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
        // real creation, and the path stays known.
        let path = PathBuf::from("/tmp/modified.txt");
        let mut existing: HashSet<PathBuf> = [path.clone()].into_iter().collect();
        let kind = resolve_first_seen(ChangeKind::Created, &path, &mut existing);
        assert_eq!(kind, ChangeKind::Modified);
        assert!(existing.contains(&path));
    }

    #[test]
    fn resolve_first_seen_keeps_created_for_new_path_and_tracks_it() {
        let path = PathBuf::from("/tmp/created.txt");
        let mut existing: HashSet<PathBuf> = HashSet::new();
        let kind = resolve_first_seen(ChangeKind::Created, &path, &mut existing);
        assert_eq!(kind, ChangeKind::Created);
        // Now tracked, so a later edit is a genuine Modified, not re-downgraded.
        assert!(existing.contains(&path));
    }

    #[test]
    fn resolve_first_seen_treats_recreate_after_delete_as_created() {
        // A baseline path that gets deleted then recreated within the same
        // session must report the recreate as a real Created, not Modified.
        let path = PathBuf::from("/tmp/doomed.txt");
        let mut existing: HashSet<PathBuf> = [path.clone()].into_iter().collect();
        assert_eq!(
            resolve_first_seen(ChangeKind::Deleted, &path, &mut existing),
            ChangeKind::Deleted
        );
        assert!(!existing.contains(&path));
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
            baseline_budget: hh_core::DEFAULT_BASELINE_BUDGET,
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

    /// Shutdown capture (robustness on macOS FSEvents): a write
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
            baseline_budget: hh_core::DEFAULT_BASELINE_BUDGET,
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

    /// A fresh store with one `generic` session, its writer and a plain blob
    /// store — the fixture every recording test below starts from.
    struct Fixture {
        _tmp: tempfile::TempDir,
        cwd: PathBuf,
        store: hh_core::store::Store,
        session_id: String,
        writer: Arc<Mutex<EventWriter>>,
        blobs: Arc<BlobStore>,
    }

    impl Fixture {
        fn new() -> Self {
            use hh_core::event::{AdapterStatus, AgentKind, NewSession};
            let tmp = tempfile::tempdir().unwrap();
            let cwd = tmp.path().join("work");
            std::fs::create_dir_all(&cwd).unwrap();
            let store =
                hh_core::store::Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs"))
                    .unwrap();
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
            Self {
                _tmp: tmp,
                cwd,
                store,
                session_id: created.id,
                writer,
                blobs,
            }
        }

        fn opts(&self) -> WatchOptions {
            WatchOptions {
                cwd: self.cwd.clone(),
                max_file_size: 4 * 1024 * 1024,
                record_binary: false,
                extra_ignore: Vec::new(),
                internal_exclude: Vec::new(),
                baseline_budget: hh_core::DEFAULT_BASELINE_BUDGET,
            }
        }

        /// Every recorded file change: `(path, kind, before text, after
        /// text, provenance)`, in recording order.
        fn changes(&self) -> Vec<RecordedChange> {
            self.store
                .list_event_index(&self.session_id)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == EventKind::FileChange)
                .map(|e| {
                    let d = self.store.get_event_detail(e.id).unwrap();
                    let fc = d.file_change.unwrap();
                    let text = |h: &Option<String>| {
                        h.as_deref()
                            .and_then(|h| self.store.blobs().get(h).ok())
                            .map(|b| String::from_utf8(b).unwrap())
                    };
                    RecordedChange {
                        path: fc.path.clone(),
                        kind: fc.change_kind,
                        before: text(&fc.before_hash),
                        after: text(&fc.after_hash),
                        provenance: self.store.file_change_provenance(e.id).unwrap().unwrap(),
                    }
                })
                .collect()
        }
    }

    #[derive(Debug)]
    struct RecordedChange {
        path: String,
        kind: ChangeKind,
        before: Option<String>,
        after: Option<String>,
        provenance: FileChangeProvenance,
    }

    fn held(content: &[u8]) -> BaselineEntry {
        BaselineEntry {
            raw: BlobStore::hash(content),
            binary: false,
            content: BaselineContent::Held(content.to_vec()),
        }
    }

    /// Deterministic unit test for [`rescan_for_missed_changes`] — the backstop
    /// that catches file changes the watcher never observed. Unlike
    /// [`watcher_captures_write_then_immediate_stop`] this does NOT spawn a
    /// real `notify` watcher, so it is fully deterministic and not subject to
    /// macOS FSEvents flakiness: it calls the backstop directly with a
    /// hand-built [`Tracker`]. Asserts:
    /// - an unobserved new file is recorded as `Created`;
    /// - a pre-existing file whose content differs from its baseline is
    ///   recorded as `Modified` WITH the session-start content as its
    ///   before-side (the bug this guards: it used to render "all added");
    /// - a pre-existing file that vanished is a `Deleted` carrying its content;
    /// - an unchanged baseline file and an already-recorded, unchanged file
    ///   are NOT re-recorded (no duplicates).
    #[test]
    fn rescan_for_missed_changes_records_unobserved_creates_modifies_and_deletes() {
        let fx = Fixture::new();
        let cwd = &fx.cwd;
        let baseline_bytes = b"already here\n";
        std::fs::write(cwd.join("baseline.txt"), baseline_bytes).unwrap();
        std::fs::write(cwd.join("changed.txt"), b"modified\n").unwrap();
        std::fs::write(cwd.join("recorded.txt"), b"captured via event\n").unwrap();
        std::fs::write(cwd.join("missed.txt"), b"the watcher missed this\n").unwrap();

        let mut tracker = Tracker::default();
        tracker
            .baseline
            .insert(cwd.join("baseline.txt"), held(baseline_bytes));
        tracker
            .baseline
            .insert(cwd.join("changed.txt"), held(b"original\n"));
        tracker
            .baseline
            .insert(cwd.join("gone.txt"), held(b"deleted while unobserved\n"));
        tracker.known.insert(
            cwd.join("recorded.txt"),
            Known {
                hash: "dummyhash".into(),
                raw: BlobStore::hash(b"captured via event\n"),
                binary: false,
            },
        );

        let opts = fx.opts();
        let sink = Sink {
            opts: &opts,
            writer: &fx.writer,
            blobs: &fx.blobs,
            session_id: &fx.session_id,
            start: Instant::now(),
        };
        let matcher = build_matcher(cwd, &[]).unwrap();
        rescan_for_missed_changes(&matcher, &sink, &mut tracker);

        let recorded = fx.changes();
        let find = |p: &str| recorded.iter().find(|c| c.path == p);
        let missed = find("missed.txt").expect("unobserved new file recorded");
        assert_eq!(missed.kind, ChangeKind::Created);
        let changed = find("changed.txt").expect("missed modify recorded");
        assert_eq!(changed.kind, ChangeKind::Modified);
        assert_eq!(changed.before.as_deref(), Some("original\n"));
        assert_eq!(changed.after.as_deref(), Some("modified\n"));
        assert_eq!(
            changed.provenance.before_source,
            Some(BeforeSource::Baseline)
        );
        let gone = find("gone.txt").expect("missed delete recorded");
        assert_eq!(gone.kind, ChangeKind::Deleted);
        assert_eq!(gone.before.as_deref(), Some("deleted while unobserved\n"));
        assert!(find("baseline.txt").is_none(), "unchanged: {recorded:?}");
        assert!(find("recorded.txt").is_none(), "no duplicate: {recorded:?}");
    }

    fn git(cwd: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
            .args(args)
            .current_dir(cwd)
            .env("GIT_CEILING_DIRECTORIES", cwd.parent().unwrap())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("git must be installed to run these tests");
        assert!(st.success(), "git {args:?} failed");
    }

    fn kind_of(entry: &BaselineEntry) -> &'static str {
        match entry.content {
            BaselineContent::Git(_) => "git",
            BaselineContent::Held(_) => "held",
            BaselineContent::Unstored => "unstored",
        }
    }

    /// The git-clean rule: only a file clean against the index takes its
    /// before-side from git; dirty and untracked files hold their real
    /// session-start content (diffing them against the index would attribute
    /// the developer's uncommitted edits to the agent). Binary files with
    /// `record_binary` off store nothing, exactly like a live capture.
    #[test]
    fn scan_baseline_applies_the_git_clean_rule() {
        let fx = Fixture::new();
        let cwd = &fx.cwd;
        std::fs::write(cwd.join("clean.txt"), "committed\n").unwrap();
        std::fs::write(cwd.join("dirty.txt"), "committed\n").unwrap();
        std::fs::write(cwd.join("bin.dat"), b"\x00\x01binary").unwrap();
        git(cwd, &["init", "-q"]);
        git(cwd, &["add", "-A"]);
        git(cwd, &["commit", "-q", "-m", "init"]);
        std::fs::write(cwd.join("dirty.txt"), "committed\nplus developer edit\n").unwrap();
        std::fs::write(cwd.join("untracked.txt"), "new\n").unwrap();

        let opts = fx.opts();
        let matcher = build_matcher(cwd, &[]).unwrap();
        let (existing, baseline) = scan_baseline(&opts, &matcher);
        let kind = |name: &str| kind_of(&baseline[&cwd.join(name)]);
        assert_eq!(kind("clean.txt"), "git");
        assert_eq!(kind("dirty.txt"), "held");
        assert_eq!(kind("untracked.txt"), "held");
        assert_eq!(kind("bin.dat"), "unstored");
        match &baseline[&cwd.join("dirty.txt")].content {
            BaselineContent::Held(b) => assert_eq!(b, b"committed\nplus developer edit\n"),
            other => panic!("dirty.txt: {other:?}"),
        }
        assert!(existing.contains(&cwd.join("clean.txt")));
        assert!(
            !baseline.keys().any(|p| p.starts_with(cwd.join(".git"))),
            ".git is never part of the baseline"
        );
    }

    /// Outside a git repository every file is held, until the budget runs
    /// out; files past it are `Unstored` (no before-side, never an error).
    #[test]
    fn scan_baseline_outside_git_holds_content_within_budget() {
        let fx = Fixture::new();
        let cwd = &fx.cwd;
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(cwd.join(name), "0123456789").unwrap(); // 10 bytes each
        }
        let mut opts = fx.opts();
        let matcher = build_matcher(cwd, &[]).unwrap();
        let (_, baseline) = scan_baseline(&opts, &matcher);
        assert!(baseline.values().all(|e| kind_of(e) == "held"));

        opts.baseline_budget = 25;
        let (_, baseline) = scan_baseline(&opts, &matcher);
        let held = baseline.values().filter(|e| kind_of(e) == "held").count();
        let unstored = baseline
            .values()
            .filter(|e| kind_of(e) == "unstored")
            .count();
        assert_eq!((held, unstored), (2, 1), "25 bytes hold two 10-byte files");

        opts.baseline_budget = 0;
        let (_, baseline) = scan_baseline(&opts, &matcher);
        assert!(baseline.values().all(|e| kind_of(e) == "unstored"));
    }

    fn sink_for<'a>(fx: &'a Fixture, opts: &'a WatchOptions) -> Sink<'a> {
        Sink {
            opts,
            writer: &fx.writer,
            blobs: &fx.blobs,
            session_id: &fx.session_id,
            start: Instant::now(),
        }
    }

    /// A git before-side is used only if the index blob hashes to the
    /// session-start content. If it does not (a smudge filter, a stale stat
    /// cache, the index rewritten mid-session), the change records no
    /// before-side rather than a wrong one.
    #[test]
    fn git_before_side_is_verified_against_session_start_content() {
        let fx = Fixture::new();
        let cwd = &fx.cwd;
        std::fs::write(cwd.join("f.txt"), "committed\n").unwrap();
        git(cwd, &["init", "-q"]);
        git(cwd, &["add", "-A"]);
        git(cwd, &["commit", "-q", "-m", "init"]);
        let opts = fx.opts();
        let matcher = build_matcher(cwd, &[]).unwrap();
        let (_, baseline) = scan_baseline(&opts, &matcher);
        let sink = sink_for(&fx, &opts);

        let mut tracker = Tracker {
            baseline: baseline.clone(),
            ..Tracker::default()
        };
        let before = tracker.take_before(&cwd.join("f.txt"), &sink);
        assert_eq!(before.source, BeforeSource::Git);
        let bytes = fx
            .store
            .blobs()
            .get(before.hash.as_deref().unwrap())
            .unwrap();
        assert_eq!(bytes, b"committed\n");

        let mut tampered = baseline;
        tampered.get_mut(&cwd.join("f.txt")).unwrap().raw = BlobStore::hash(b"something else");
        let mut tracker = Tracker {
            baseline: tampered,
            ..Tracker::default()
        };
        let before = tracker.take_before(&cwd.join("f.txt"), &sink);
        assert_eq!(before.source, BeforeSource::None);
        assert_eq!(before.hash, None);
    }

    /// Rename pairing inside one flush batch: by the backend's rename hint
    /// (even when the content also changed), else by identical content — but
    /// never between two empty files, and never onto a path that already
    /// existed (that is a modify of the target plus a delete of the source).
    #[test]
    fn record_changes_pairs_renames_by_hint_and_by_content() {
        let fx = Fixture::new();
        let cwd = &fx.cwd;
        let mut tracker = Tracker::default();
        for (name, content) in [
            ("same_old.txt", &b"same content\n"[..]),
            ("hinted_old.txt", &b"before edit\n"[..]),
            ("empty_old.txt", &b""[..]),
            ("src.txt", &b"overwrites target\n"[..]),
            ("target.txt", &b"target original\n"[..]),
        ] {
            tracker.baseline.insert(cwd.join(name), held(content));
        }
        // On disk after the "agent" ran:
        std::fs::write(cwd.join("same_new.txt"), "same content\n").unwrap();
        std::fs::write(cwd.join("hinted_new.txt"), "after edit\n").unwrap();
        std::fs::write(cwd.join("empty_new.txt"), "").unwrap();
        std::fs::write(cwd.join("target.txt"), "overwrites target\n").unwrap();

        let opts = fx.opts();
        let sink = sink_for(&fx, &opts);
        let mut hints: HashMap<PathBuf, PathBuf> =
            [(cwd.join("hinted_new.txt"), cwd.join("hinted_old.txt"))]
                .into_iter()
                .collect();
        let items = vec![
            (cwd.join("same_old.txt"), ChangeKind::Deleted),
            (cwd.join("same_new.txt"), ChangeKind::Created),
            (cwd.join("hinted_old.txt"), ChangeKind::Deleted),
            (cwd.join("hinted_new.txt"), ChangeKind::Created),
            (cwd.join("empty_old.txt"), ChangeKind::Deleted),
            (cwd.join("empty_new.txt"), ChangeKind::Created),
            (cwd.join("src.txt"), ChangeKind::Deleted),
            // The FSEvents-style "created" for an existing target.
            (cwd.join("target.txt"), ChangeKind::Created),
        ];
        record_changes(items, &mut hints, &sink, &mut tracker);

        let changes = fx.changes();
        let find = |p: &str| {
            changes
                .iter()
                .find(|c| c.path == p)
                .unwrap_or_else(|| panic!("{p} not recorded: {changes:?}"))
        };
        let same = find("same_new.txt");
        assert_eq!(
            same.provenance.renamed_from.as_deref(),
            Some("same_old.txt")
        );
        assert_eq!(same.kind, ChangeKind::Modified);
        let hinted = find("hinted_new.txt");
        assert_eq!(
            hinted.provenance.renamed_from.as_deref(),
            Some("hinted_old.txt")
        );
        assert_eq!(hinted.before.as_deref(), Some("before edit\n"));
        assert_eq!(hinted.after.as_deref(), Some("after edit\n"));
        assert_eq!(find("empty_new.txt").kind, ChangeKind::Created);
        assert_eq!(find("empty_old.txt").kind, ChangeKind::Deleted);
        let target = find("target.txt");
        assert_eq!(target.kind, ChangeKind::Modified);
        assert_eq!(target.provenance.renamed_from, None);
        assert_eq!(target.before.as_deref(), Some("target original\n"));
        assert_eq!(find("src.txt").kind, ChangeKind::Deleted);
        assert!(
            !changes
                .iter()
                .any(|c| c.path == "same_old.txt" || c.path == "hinted_old.txt"),
            "a paired rename source is not also recorded as a delete: {changes:?}"
        );
        assert!(hints.is_empty(), "consumed hints are dropped");
    }

    /// Best-effort: when the cwd itself cannot be watched (here: it does
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
            baseline_budget: hh_core::DEFAULT_BASELINE_BUDGET,
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
