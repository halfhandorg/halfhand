//! Structured adapter errors (SRS v1.1.0 BUG-1.4).
//!
//! Every adapter failure is routed through [`AdapterError`] so the writer
//! task can record both a machine-readable [`code`](AdapterError::code) and
//! a human-readable [`Display`](std::fmt::Display) message atomically with
//! the session row (`adapter_status = 'degraded'` +
//! `adapter_degrade_reason = err.code()`). This replaces the v1.0.0
//! "stringly-typed" degrade reason with an enum the DB can carry verbatim,
//! making post-hoc diagnosis possible (`hh inspect --json` shows the code;
//! `hh doctor` groups degraded sessions by code).
//!
//! Per the v1.1.0 addendum, adapters must never panic on malformed input —
//! every untrusted-input path (JSONL parsing, discovery) returns an
//! `AdapterError` instead. The codes match the table in SRS BUG-1.1.

use thiserror::Error;

/// A structured adapter failure (SRS BUG-1.1, BUG-1.4).
///
/// Every variant carries a machine-readable [`code`](Self::code) (stable
/// across releases — part of the `--json` contract) and a human-readable
/// message (the `Display` impl, for the one-line stderr warning). The
/// writer task persists the code to `sessions.adapter_degrade_reason`.
///
/// The underlying error's `Display` text is captured as a [`String`] (not
/// the error value itself) so that [`AdapterError`] is [`Clone`] —
/// [`AdapterOutcome`](crate::adapter::AdapterOutcome) derives `Clone` so the
/// recorder can `.clone()` the outcome for the `RunOutcome` it returns to
/// the binary. Capturing the text loses nothing: the code is the stable
/// machine-readable part, and the text is only ever displayed, never
/// re-matched.
///
/// `#[non_exhaustive]`: new degrade causes may be added over time (a new
/// adapter, a new failure mode); non-exhaustive keeps that additive under
/// the v1.0.0 stability addendum.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum AdapterError {
    /// No JSONL file was discovered within the discovery window (BUG-1.3).
    /// The agent may not have written a transcript yet, or the projects
    /// directory is missing.
    #[error("JSONL file not found within discovery window")]
    JsonlNotFound,

    /// A JSONL line failed to deserialize into the expected schema
    /// (BUG-1.2). Carries the 1-based line number and the parse error
    /// message (captured as a string so the error is `Clone`).
    #[error("JSONL parse error at line {line}: {msg}")]
    JsonlParseError {
        /// The 1-based line number in the transcript file.
        line: usize,
        /// The parse error message (the `Display` text of the underlying
        /// `serde_json::Error`, captured at construction time).
        msg: String,
    },

    /// A required field is absent or has an unexpected type (BUG-1.2).
    /// Distinct from [`Self::JsonlParseError`]: the line parsed as JSON
    /// but the schema does not match what the adapter expects.
    #[error("JSONL schema drift: field {field} missing or wrong type")]
    JsonlSchemaDrift {
        /// The field name that was missing or had the wrong type.
        field: String,
    },

    /// Multiple candidate JSONL files matched the discovery criteria and
    /// none was unambiguously selected (BUG-1.3).
    #[error("Multiple candidate JSONL files matched; none selected")]
    DiscoveryAmbiguous,

    /// The JSONL file exists but cannot be read (permission denied, IO
    /// error). Carries the path that was denied.
    #[error("Permission denied reading JSONL: {path}")]
    PermissionDenied {
        /// The path that could not be read.
        path: String,
    },

    /// Other I/O failure (disk error, file vanished mid-read, etc.).
    /// The error message is captured as a string so the error is `Clone`.
    #[error("I/O error: {msg}")]
    IoError {
        /// The I/O error message (the `Display` text of the underlying
        /// `std::io::Error`, captured at construction time).
        msg: String,
    },
}

impl AdapterError {
    /// The machine-readable degrade code (SRS BUG-1.1), stable across
    /// releases and persisted to `sessions.adapter_degrade_reason`. This
    /// is part of the `--json` output contract (ENH-2.3, `hh inspect
    /// --json`) and must not change without a deprecation cycle.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::JsonlNotFound => "jsonl_not_found",
            Self::JsonlParseError { .. } => "jsonl_parse_error",
            Self::JsonlSchemaDrift { .. } => "jsonl_schema_drift",
            Self::DiscoveryAmbiguous => "discovery_ambiguous",
            Self::PermissionDenied { .. } => "permission_denied",
            Self::IoError { .. } => "io_error",
        }
    }
}

impl From<std::io::Error> for AdapterError {
    /// Capture the `Display` text of an [`std::io::Error`] into
    /// [`Self::IoError`]. The error value itself is not `Clone`, so we keep
    /// only its message — sufficient for the one-line stderr warning and
    /// for `hh doctor`'s diagnosis; the [`code`](Self::code) is the stable
    /// machine-readable part.
    fn from(e: std::io::Error) -> Self {
        Self::IoError { msg: e.to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_variant_has_a_stable_code() {
        // The codes are part of the --json contract (SRS BUG-1.1) and must
        // not change without a deprecation cycle. Lock them here.
        assert_eq!(AdapterError::JsonlNotFound.code(), "jsonl_not_found");
        assert_eq!(
            AdapterError::DiscoveryAmbiguous.code(),
            "discovery_ambiguous"
        );
        // Variants with payload — construct with dummy values to read the code.
        assert_eq!(
            AdapterError::JsonlParseError {
                line: 42,
                msg: "unexpected token".into(),
            }
            .code(),
            "jsonl_parse_error"
        );
        assert_eq!(
            AdapterError::JsonlSchemaDrift {
                field: "message".into(),
            }
            .code(),
            "jsonl_schema_drift"
        );
        assert_eq!(
            AdapterError::PermissionDenied {
                path: "/foo".into(),
            }
            .code(),
            "permission_denied"
        );
        assert_eq!(
            AdapterError::IoError { msg: "disk".into() }.code(),
            "io_error"
        );
    }

    #[test]
    fn display_messages_are_human_readable() {
        // The Display impl is what the recorder prints to stderr after the
        // child exits (FR-1.5). It must be a one-line actionable message.
        let msg = format!(
            "{}",
            AdapterError::JsonlParseError {
                line: 7,
                msg: "unexpected token".into(),
            }
        );
        assert!(msg.contains("line 7"), "message must name the line: {msg}");
        let drift = format!(
            "{}",
            AdapterError::JsonlSchemaDrift {
                field: "message".into(),
            }
        );
        assert!(
            drift.contains("message"),
            "message must name the field: {drift}"
        );
    }

    #[test]
    fn from_io_error_captures_message() {
        let io_err = std::io::Error::other("disk full");
        let adapter_err = AdapterError::from(io_err);
        assert_eq!(adapter_err.code(), "io_error");
        let msg = format!("{adapter_err}");
        assert!(
            msg.contains("disk full"),
            "io_error message preserved: {msg}"
        );
    }

    #[test]
    fn adapter_error_is_clone() {
        // AdapterOutcome derives Clone and holds Option<AdapterError>, so
        // AdapterError must be Clone. The recorder clones the outcome for
        // RunOutcome (returned to the binary). If this stops compiling, the
        // recorder's clone path breaks.
        let err = AdapterError::JsonlParseError {
            line: 1,
            msg: "x".into(),
        };
        let cloned = err.clone();
        assert_eq!(err.code(), cloned.code());
    }
}
