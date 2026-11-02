//! Git metadata capture (FR-1.2: branch, HEAD sha, dirty flag).
//!
//! Shells out to `git` — the SRS does not list a git library in §6, and the
//! recorder already spawns subprocesses. All failures are non-fatal: a missing
//! `git` binary or a non-repo cwd simply yield `None` fields (best-effort, per
//! "if the cwd is a repo"). We never let git problems fail a recording.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Git metadata captured for a session (FR-1.2). All fields are `None` if the
/// cwd is not a git repo or `git` is unavailable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitMeta {
    /// Current branch name (e.g. `main`); `None` in a detached-HEAD repo.
    pub branch: Option<String>,
    /// HEAD commit sha (short or full — whatever `git` prints).
    pub sha: Option<String>,
    /// Whether the working tree had uncommitted changes.
    pub dirty: Option<bool>,
}

impl GitMeta {
    /// Capture git metadata for `cwd`, best-effort (FR-1.2). Never errors.
    #[must_use]
    pub fn capture(cwd: &Path) -> Self {
        let branch = git_output(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
        // `--abbrev-ref HEAD` prints "HEAD" when detached; treat that as no
        // branch (the SRS wants the branch name, not a sentinel).
        let branch = branch.and_then(|b| {
            let trimmed = b.trim();
            if trimmed.is_empty() || trimmed == "HEAD" {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        let sha = git_output(cwd, &["rev-parse", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let dirty = git_exit_ok(cwd, &["diff", "--quiet"]).map(|clean| !clean);
        Self { branch, sha, dirty }
    }
}

/// Git index snapshot taken at session start, used to resolve the before-side
/// of the first change to a pre-existing file without copying it (see
/// docs/recording.md, "Where the before-side comes from").
///
/// Maps the absolute path (`cwd` joined with the cwd-relative name git
/// prints) of every regular file under `cwd` that is **clean against the
/// index** to the object id of its index blob. A file is clean when it is
/// tracked at stage 0 (not mid-merge), is a regular file (mode `100644` or
/// `100755` — not a symlink or submodule), and `git diff-files` does not list
/// it. Anything else — dirty, untracked, conflicted, or no repository at all —
/// is absent, and the caller must capture its content itself: diffing a dirty
/// file against the index (or `HEAD`) would attribute the developer's own
/// uncommitted edits to the agent.
///
/// `git diff-files` does not refresh the index's stat cache (and we never
/// write the user's index), so a file whose stat data changed but whose
/// content did not may be reported dirty. That errs on the safe side: the
/// file is captured as a baseline instead of read from git. Returns `None` if
/// `cwd` is not inside a work tree or `git` cannot run.
pub(crate) fn index_clean_blobs(cwd: &Path) -> Option<HashMap<PathBuf, String>> {
    let staged = git_output_bytes(cwd, &["ls-files", "--stage", "-z"])?;
    let dirty: HashSet<Vec<u8>> =
        git_output_bytes(cwd, &["diff-files", "--name-only", "-z", "--relative"])?
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
    let mut conflicted: HashSet<Vec<u8>> = HashSet::new();
    let mut clean: HashMap<Vec<u8>, String> = HashMap::new();
    for record in staged.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        // `<mode> SP <oid> SP <stage> TAB <path>`.
        let Some(tab) = record.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let (meta, name) = (&record[..tab], &record[tab + 1..]);
        let Ok(meta) = std::str::from_utf8(meta) else {
            continue;
        };
        let mut fields = meta.split(' ');
        let (Some(mode), Some(oid), Some(stage)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if stage != "0" {
            conflicted.insert(name.to_vec());
            continue;
        }
        if (mode == "100644" || mode == "100755") && !dirty.contains(name) {
            clean.insert(name.to_vec(), oid.to_string());
        }
    }
    Some(
        clean
            .into_iter()
            .filter(|(name, _)| !conflicted.contains(name))
            .filter_map(|(name, oid)| Some((cwd.join(path_from_git_bytes(&name)?), oid)))
            .collect(),
    )
}

/// Read the content of index blob `oid` as it would appear in the work tree
/// at `rel_path` (relative to `cwd`): `git cat-file --filters` applies the
/// same end-of-line conversion and smudge filters a checkout would, so the
/// result is comparable byte-for-byte with what was on disk. Falls back to
/// the raw blob if `--filters` is unsupported (git < 2.11); the caller
/// verifies the bytes against the session-start content hash either way, so
/// a filter mismatch can never surface wrong content. `None` if git fails.
pub(crate) fn read_blob(cwd: &Path, oid: &str, rel_path: &Path) -> Option<Vec<u8>> {
    let path_arg = format!("--path={}", rel_path.to_string_lossy());
    git_output_bytes(cwd, &["cat-file", "--filters", &path_arg, oid])
        .or_else(|| git_output_bytes(cwd, &["cat-file", "blob", oid]))
}

/// A path as git printed it under `-z` (raw bytes, no quoting). On Unix this
/// is exact; elsewhere it must be UTF-8 (a non-UTF-8 name is skipped, which
/// only means that file is captured as a baseline instead).
// Justification: the `None` is reachable on non-Unix targets (a non-UTF-8
// name); clippy only sees the Unix arm, which always succeeds.
#[allow(clippy::unnecessary_wraps)]
fn path_from_git_bytes(bytes: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

/// Run `git` in `cwd` and return raw stdout bytes on success (stderr
/// discarded, see [`git_output`]).
fn git_output_bytes(cwd: &Path, args: &[&str]) -> Option<Vec<u8>> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
}

/// Run `git` in `cwd` with the given args and return stdout on success.
/// stderr is discarded so a non-repo cwd does not leak `fatal: not a git
/// repository` to the user's terminal.
fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
}

/// Run `git` in `cwd` with the given args; return `Some(true)` if it exits
/// success (e.g. `diff --quiet` → clean), `Some(false)` on nonzero (dirty),
/// `None` if `git` could not be run at all. stderr is discarded (see
/// [`git_output`]).
fn git_exit_ok(cwd: &Path, args: &[&str]) -> Option<bool> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stderr(Stdio::null())
        .status()
        .ok()
        .map(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    /// Smoke test: capture in the repo root (CI runs inside this repo, so git
    /// is present and `GitMeta` should be fully populated). If git is missing
    /// (unlikely on CI) the fields are `None` and we still pass — best-effort.
    #[test]
    fn capture_in_repo_yields_metadata_or_none() {
        let cwd = env::current_dir().unwrap();
        let meta = GitMeta::capture(&cwd);
        if let Some(sha) = &meta.sha {
            assert_ne!(sha, "");
        }
        // `dirty` should at least be determined when sha is present.
        if meta.sha.is_some() {
            assert!(meta.dirty.is_some());
        }
    }
}
