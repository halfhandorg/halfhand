//! hh-core — storage, blob store, event model, and config for Halfhand.
//!
//! This crate implements the local-first data layer (SRS §4) used by the `hh`
//! binary and the recorder. It is intentionally free of any I/O runtime or
//! CLI concern: the binary drives it, not the other way around.
//!
//! Per CLAUDE.md, every public item is documented (`missing_docs` is denied
//! here) and `clippy::pedantic` is enabled via the workspace lint table.

#![deny(missing_docs)]

pub mod adapter;
pub mod blob;
pub mod config;
pub mod error;
pub mod event;
pub mod migrations;
pub mod step;
pub mod store;

// Re-export the most commonly used types at the crate root for ergonomics.
pub use adapter::{
    resolve_override as resolve_adapter_override, select as select_adapter, Adapter,
    AdapterContext, AdapterHandle, AdapterOutcome,
};
pub use blob::{BlobStore, PutOutcome};
pub use config::{parse_bytes, Config, Paths, RecordConfig, ReplayConfig, StorageConfig, Theme};
pub use error::{BlobError, ConfigError, Error, ResolveError, Result, StorageError};
pub use event::{
    truncate_summary, AdapterStatus, AgentKind, ChangeKind, Event, EventKind, EventRow, FileChange,
    NewSession, SessionRow, SessionStatus,
};
pub use step::assign_steps;
pub use store::{CreatedSession, EventWriter, Store};
