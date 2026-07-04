//! `hh-record` — the Halfhand recorder layer (SRS §6).
//!
//! Drives a PTY-recorded agent session: spawns the agent in a PTY
//! ([`runner`]), watches the working tree for file changes ([`watcher`]),
//! and feeds both into the single-writer SQLite store via `hh-core`.
//! Out of scope for v0.1 (tracked separately): the Claude Code JSONL
//! adapter (FR-1.5) and the MCP stdio proxy (FR-2).
//!
//! The threads-vs-tokio decision is recorded in
//! `docs/adr/0001-threads-vs-tokio.md`.

#![deny(missing_docs)]

mod agent;
mod error;
mod git;
mod runner;
mod watcher;

pub use agent::detect_agent;
pub use error::{RecordError, Result};
pub use git::GitMeta;
pub use runner::{run, RunOptions, RunOutcome};
pub use watcher::{spawn_watcher, WatchOptions, WatcherHandle};

// Re-export the core types the binary needs to construct RunOptions without
// reaching into hh-core directly (keeps the binary's `use` surface small).
pub use hh_core::store::Store;
