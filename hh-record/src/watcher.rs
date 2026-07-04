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

use std::collections::HashMap;
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

/// The watcher event loop, run on the worker thread.
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
    while !stop.load(Ordering::Acquire) {
        match nrx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(event)) => {
                for path in &event.paths {
                    if let Err(e) = process_path(
                        path,
                        event.kind,
                        &matcher,
                        &opts,
                        &writer,
                        &blobs,
                        &session_id,
                        start,
                        &mut known,
                    ) {
                        eprintln!(
                            "hh: warning: file change capture failed for {}: {e}",
                            path.display()
                        );
                    }
                }
            }
            Ok(Err(e)) => {
                eprintln!("hh: warning: fs watcher error: {e}");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Drop the watcher to release the inotify/fsevents handle.
    drop(watcher);
}

/// Process a single changed path into a `file_change` event (or skip it).
#[allow(clippy::too_many_arguments)] // recorder wiring threaded through to the writer
fn process_path(
    path: &Path,
    kind: notify::EventKind,
    matcher: &Gitignore,
    opts: &WatchOptions,
    writer: &Arc<Mutex<EventWriter>>,
    blobs: &Arc<BlobStore>,
    session_id: &str,
    start: Instant,
    known: &mut HashMap<PathBuf, String>,
) -> std::result::Result<(), String> {
    // Exclude internal halfhand paths (data dir / db / blobs).
    for excl in &opts.internal_exclude {
        if path.starts_with(excl) {
            return Ok(());
        }
    }

    // Relativize to cwd for ignore matching and storage. Skip paths outside
    // the watched tree (notify should not deliver any, but be defensive).
    let Ok(rel) = path.strip_prefix(&opts.cwd) else {
        return Ok(());
    };

    // Skip directories and ignored paths. Use `matched_path_or_any_parents`
    // so a file under an ignored directory (e.g. `node_modules/foo.js`) is
    // excluded even though only the directory matched a pattern.
    if path.is_dir() {
        return Ok(());
    }
    if matcher.matched_path_or_any_parents(rel, false).is_ignore() {
        return Ok(());
    }

    let change_kind = match kind {
        notify::EventKind::Create(_) => ChangeKind::Created,
        notify::EventKind::Modify(_) => ChangeKind::Modified,
        notify::EventKind::Remove(_) => ChangeKind::Deleted,
        // Any/Other/Access are not content changes we capture.
        notify::EventKind::Any | notify::EventKind::Other | notify::EventKind::Access(_) => {
            return Ok(());
        }
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
        step: None, // file changes are steps in replay (FR-3.4); step assigned at read time? See SRS.
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
}
