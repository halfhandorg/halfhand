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

/// Spawn the watcher thread. Setup errors (building the matcher or the
/// notify watcher, or the initial `watch` call) propagate as `Pty`-style
/// errors; runtime errors in the event loop are written to stderr and the
/// loop continues (FR-1.4 best-effort).
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
) -> crate::Result<WatcherHandle> {
    let matcher = build_matcher(&opts.cwd, &opts.extra_ignore).map_err(crate::RecordError::Pty)?;
    let stop = Arc::new(AtomicBool::new(false));

    let (ntx, nrx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(
        move |res: std::result::Result<notify::Event, notify::Error>| {
            // Channel send fails only when the watcher thread exited; ignore.
            let _ = ntx.send(res);
        },
    )
    .map_err(|e| crate::RecordError::Pty(format!("notify watcher init: {e}")))?;
    watcher
        .watch(&opts.cwd, RecursiveMode::Recursive)
        .map_err(|e| {
            crate::RecordError::Pty(format!("notify watch `{}`: {e}", opts.cwd.display()))
        })?;

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
            );
        })
        .map_err(|e| crate::RecordError::Pty(format!("spawn watcher thread: {e}")))?;

    Ok(WatcherHandle {
        thread: Some(thread),
        stop,
    })
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
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
// recorder wiring; values are moved into the spawning closure and owned for
// the thread's lifetime (thread entry point, see runner::run_reader).
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
) {
    let mut known: HashMap<PathBuf, String> = HashMap::new();
    let mut pending: HashMap<PathBuf, Pending> = HashMap::new();
    // Baseline of files that exist under `cwd` before the watch starts
    // observing changes, so a raw `Created` event on one of them can be
    // recognized as spurious (see `resolve_first_seen`).
    let mut existing: HashSet<PathBuf> = HashSet::new();
    scan_existing_files(
        &opts.cwd,
        &opts.cwd,
        &matcher,
        &opts.internal_exclude,
        &mut existing,
    );
    while !stop.load(Ordering::Acquire) {
        match nrx.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(event)) => {
                for path in &event.paths {
                    if let Some(kind) = classify(path, event.kind, &matcher, &opts) {
                        let kind = resolve_first_seen(kind, path, &mut existing);
                        coalesce(&mut pending, path.clone(), kind);
                    }
                }
            }
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
    // Drop the watcher to release the inotify/fsevents handle.
    drop(watcher);
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
/// reported as a genuine `Created`.
fn resolve_first_seen(
    kind: ChangeKind,
    path: &Path,
    existing: &mut HashSet<PathBuf>,
) -> ChangeKind {
    match kind {
        ChangeKind::Created if existing.contains(path) => ChangeKind::Modified,
        ChangeKind::Created | ChangeKind::Modified => {
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
    let rel_str = rel_to_slash(rel);

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

    let writer = writer
        .lock()
        .map_err(|e| format!("writer lock poisoned: {e}"))?;
    writer
        .append_file_change(event, file_change)
        .map_err(|e| format!("append file_change: {e}"))?;
    Ok(())
}

/// Render a relative path with `/` separators regardless of platform, so
/// `file_changes.path` (DR-2: a public, documented schema) stores one canonical
/// form. On Windows `Path::to_string_lossy` would yield `sub\nested.txt`,
/// breaking cross-platform queries like `path LIKE 'target/%'` and making the
/// same recording render differently per OS.
fn rel_to_slash(rel: &Path) -> String {
    let mut out = String::new();
    for component in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    out
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
    fn rel_to_slash_joins_components_with_forward_slashes() {
        // Typed PathBuf fixtures: built with join() so the intermediate
        // separator is the platform-native one, yet the stored form must
        // always be `/`-separated.
        let nested = PathBuf::from("sub").join("dir").join("nested.txt");
        assert_eq!(rel_to_slash(&nested), "sub/dir/nested.txt");
        let flat = PathBuf::from("fixture_output.txt");
        assert_eq!(rel_to_slash(&flat), "fixture_output.txt");
        assert_eq!(rel_to_slash(Path::new("")), "");
    }

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
        // real creation.
        let path = PathBuf::from("/tmp/modified.txt");
        let mut existing: HashSet<PathBuf> = [path.clone()].into_iter().collect();
        let kind = resolve_first_seen(ChangeKind::Created, &path, &mut existing);
        assert_eq!(kind, ChangeKind::Modified);
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
}
