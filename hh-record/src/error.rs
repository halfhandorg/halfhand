//! Recorder error types (CLAUDE.md: library errors via `thiserror`).

use thiserror::Error;

/// A specialization of [`hh_core::Error`] for recorder-originated failures,
/// keeping the cause chain readable for the binary's `anyhow` context.
///
/// `portable-pty` returns `anyhow::Error`; we stringify it at the boundary
/// (its `Display` form) rather than taking an `anyhow` dependency in this
/// library, per CLAUDE.md ("Do not add a dependency without stating why").
#[derive(Debug, Error)]
pub enum RecordError {
    /// Failed to spawn the child in the PTY (FR-1.1).
    #[error("failed to spawn command `{command}` in pty: {reason}")]
    Spawn {
        /// The command line that failed.
        command: String,
        /// The stringified portable-pty error.
        reason: String,
    },
    /// A PTY I/O or configuration error (resize, reader/writer, wait).
    #[error("pty error: {0}")]
    Pty(String),
    /// A storage call failed while recording (session create, event append,
    /// blob put, finalize). Wraps the `hh-core` error so the binary can attach
    /// `anyhow::Context`.
    #[error(transparent)]
    Store(#[from] hh_core::Error),
    /// The child could not be waited on, or its exit status was unreadable.
    #[error("child process error: {0}")]
    Child(String),
    /// An unknown or unsupported adapter name was passed to `hh run --adapter`
    /// (FR-1.5). Names the bad value and what's available so the user can fix it.
    #[error("unknown adapter: {0}")]
    Adapter(String),
    /// A generic MCP proxy failure (server spawn, stdio I/O, store). The string
    /// is a human-readable, actionable description (FR-2).
    #[error("mcp proxy error: {0}")]
    Mcp(String),
    /// The `HH_SESSION_ID` referenced by an attached `hh mcp-proxy` does not
    /// resolve to a recorded session (FR-2). Actionable: tells the user to run
    /// standalone or re-run inside `hh run` rather than creating an orphan.
    #[error("mcp attach failed: {0}")]
    McpSession(String),
}

/// A `Result` alias for the recorder.
pub type Result<T> = std::result::Result<T, RecordError>;
