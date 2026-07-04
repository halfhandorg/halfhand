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
}

/// A `Result` alias for the recorder.
pub type Result<T> = std::result::Result<T, RecordError>;
