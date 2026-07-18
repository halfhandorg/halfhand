//! `hh-record` — the Halfhand recorder layer (SRS §6).
//!
//! Drives a PTY-recorded agent session: spawns the agent in a PTY
//! (`runner`), watches the working tree for file changes (`watcher`),
//! feeds both into the single-writer SQLite store via `hh-core`, and — when a
//! structured-event adapter applies (FR-1.5, Claude Code today) — drains its
//! parsed events into the same store with `tool_call`→`tool_result` correlation.
//! The MCP stdio proxy (FR-2) lives in `mcp_proxy`.
//!
//! The threads-vs-tokio decision is recorded in
//! `docs/adr/0001-threads-vs-tokio.md`.

// BUG-3.2 (SRS v1.1.0 §3): `clippy::pedantic` is enabled explicitly at the
// crate root so the lint set is enforced regardless of workspace-lint
// inheritance — the SRS wording requires the enablement live at the crate
// level, not only in `[workspace.lints]`. A crate-level group `warn` would
// override the workspace's per-lint `allow`s (clippy gives the closer scope
// precedence), so the same set of pedantic lints the workspace allows
// project-wide is re-allowed here with one-line justifications, matching the
// CLAUDE.md pattern ("Enable clippy::pedantic at crate level and #[allow]
// individual lints with a one-line justification comment").
#![warn(clippy::pedantic)]
// Justification: naming types `RecordError`, `RunOptions` etc. inside a crate
// named `hh-record` reads better than the redundant `hh_record::HhRecordError`.
#![allow(clippy::module_name_repetitions)]
// Justification: most builders/fallible functions already return Result; tagging
// every one with #[must_use] is noise that does not catch real bugs here.
#![allow(clippy::must_use_candidate)]
// Justification: full rustdoc on every public item is enforced by `missing_docs`
// (deny); the separate "Errors"/"Panics" doc sections pedantic wants are
// covered inline where relevant, not on every fn.
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
// Justification: the docs reference many tech proper nouns (PTY, FSEvents,
// inotify, JSON, MCP, SQLite, …) as plain text, not doc-links. Backticking
// every occurrence is noise that does not improve the rendered docs.
#![allow(clippy::doc_markdown)]
#![deny(missing_docs)]

mod agent;
mod error;
mod git;
mod mcp_proxy;
mod runner;
mod watcher;

pub use agent::detect_agent;
pub use error::{RecordError, Result};
pub use git::GitMeta;
pub use mcp_proxy::{run_mcp_proxy, McpProxyOptions, McpProxyOutcome};
pub use runner::{run, RunOptions, RunOutcome};
pub use watcher::{spawn_watcher, watcher_smoke_test, WatchOptions, WatcherHandle};

/// Fuzz-only entry points into the MCP JSON-RPC line classifier; see
/// [`mcp_proxy::fuzzing`].
#[cfg(feature = "fuzzing")]
pub use mcp_proxy::fuzzing;

// Re-export the core types the binary needs to construct RunOptions without
// reaching into hh-core directly (keeps the binary's `use` surface small).
pub use hh_core::store::Store;

/// Build the recorder's shared blob store handle: redacting when a
/// record-time redactor is configured (`[redaction] at_record`), plain
/// otherwise. Centralized so the PTY runner and the MCP proxy cannot drift —
/// every recorder blob write goes through the same enforcement point
/// (docs/redaction-design.md).
pub(crate) fn make_blob_store(
    store: &Store,
    redactor: Option<std::sync::Arc<hh_core::redact::Detectors>>,
) -> hh_core::blob::BlobStore {
    let root = store.blobs().root().to_path_buf();
    match redactor {
        Some(r) => hh_core::blob::BlobStore::with_redactor(root, r),
        None => hh_core::blob::BlobStore::new(root),
    }
}
