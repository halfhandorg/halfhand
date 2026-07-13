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
pub mod bundle;
pub mod config;
pub mod error;
pub mod event;
pub mod migrations;
pub mod redact;
pub mod step;
pub mod store;
pub mod timeline;

// Re-export the most commonly used types at the crate root for ergonomics.
pub use adapter::{
    resolve_override as resolve_adapter_override, select as select_adapter, Adapter,
    AdapterContext, AdapterHandle, AdapterOutcome,
};
pub use blob::{BlobStore, PutOutcome};
pub use bundle::{Bundle, FORMAT_VERSION};
pub use config::{
    parse_bytes, Config, Paths, RecordConfig, RedactionConfig, RedactionRule, ReplayConfig,
    StorageConfig, Theme,
};
pub use error::{BlobError, BundleError, ConfigError, Error, ResolveError, Result, StorageError};
pub use event::{
    truncate_summary, AdapterStatus, AgentKind, ChangeKind, Event, EventDetail, EventIndexRow,
    EventKind, EventRow, FileChange, NewSession, RawEventRow, SessionRow, SessionStatus,
};
pub use redact::{Detectors, Finding, SecretKind};
// `Uuid` is part of the public surface already (`now_v7` returns it,
// `NewSession::id` holds it); re-exporting it lets downstream crates name the
// type without adding `uuid` as a direct dependency.
pub use step::assign_steps;
pub use store::{
    CreatedSession, EventWriter, FindingLocation, LargestSession, PruneStats, RedactOutcome,
    ScanFinding, SecretSummary, Store, StoreStats,
};
pub use timeline::{build_timeline, StepEntry, TerminalSegment, TimelineRow};
pub use uuid::Uuid;
