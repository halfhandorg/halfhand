//! Git metadata capture (FR-1.2: branch, HEAD sha, dirty flag).
//!
//! Shells out to `git` — the SRS does not list a git library in §6, and the
//! recorder already spawns subprocesses. All failures are non-fatal: a missing
//! `git` binary or a non-repo cwd simply yield `None` fields (best-effort, per
//! "if the cwd is a repo"). We never let git problems fail a recording.

use std::path::Path;
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
            assert!(!sha.is_empty());
        }
        // `dirty` should at least be determined when sha is present.
        if meta.sha.is_some() {
            assert!(meta.dirty.is_some());
        }
    }
}
