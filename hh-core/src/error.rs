//! Error types for hh-core.
//!
//! Per CLAUDE.md / NFR-5, library errors use [`thiserror`]. All user-facing
//! errors are actionable: they describe what failed, why, and a suggested fix
//! via the [`enum@Error`] variants' `Display` impl.

use std::path::PathBuf;
use thiserror::Error;

/// Top-level hh-core error.
///
/// `#[non_exhaustive]`: this and the other error enums below are expected to
/// gain variants over time (this PR added [`StorageError::StillRecording`]);
/// non-exhaustive keeps that additive under `cargo-semver-checks
/// --release-type minor` instead of registering as a break — a downstream
/// `match` must already carry a wildcard arm, matching CLAUDE.md's v1.0.0
/// addendum ("additive changes ... not breaking").
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// SQLite storage failure (IO, constraint, or migration).
    #[error("storage error: {0}\n  hint: check that the data directory is writable and not on a full disk")]
    Storage(#[from] StorageError),

    /// Blob store failure (IO, compression, or hash mismatch).
    #[error("blob store error: {0}")]
    Blob(#[from] BlobError),

    /// Configuration parsing failure. Per SRS §4.2 config never fails to start
    /// the program on unknown keys (those warn); this is only raised for a
    /// malformed TOML file or a value that cannot be interpreted.
    #[error("config error: {0}")]
    Config(#[from] ConfigError),

    /// Portable session bundle (`hh export --bundle` / `hh import`) build or
    /// parse failure.
    #[error("bundle error: {0}")]
    Bundle(#[from] BundleError),
}

/// Storage-layer error.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StorageError {
    /// SQLite returned an error.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The database file could not be opened or created.
    #[error("cannot open database at {path:?}: {source}")]
    Open {
        /// Path that failed.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },

    /// A migration could not be applied.
    #[error("migration {version} failed: {source}")]
    Migration {
        /// Migration version that failed.
        version: i64,
        /// Underlying SQLite error.
        source: rusqlite::Error,
    },

    /// A session id did not resolve to exactly one session.
    #[error("session not resolvable: {0}")]
    Resolve(#[from] ResolveError),

    /// A session id was not found.
    #[error("session {0} not found\n  hint: run `hh list` to see recorded sessions")]
    NotFound(String),

    /// A blob referenced by an event does not exist on disk.
    #[error("blob {0} referenced by event but missing from disk")]
    MissingBlob(String),

    /// `redact_session` was asked to rewrite a session that is still being
    /// recorded (a live writer could re-insert plaintext mid-rewrite).
    #[error("session {0} is still recording\n  hint: wait for the recording to finish (`hh list` shows status), then re-run `hh redact`")]
    StillRecording(String),

    /// The single-writer task is no longer reachable (closed or panicked).
    #[error("the writer task is closed (it may have crashed; check stderr for prior errors)")]
    WriterClosed,

    /// The single-writer task panicked while handling a request.
    #[error("the writer task panicked")]
    WriterPanic,
}

/// Failure to resolve a session id to exactly one session (FR-3.1).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ResolveError {
    /// The id prefix matched more than one session.
    #[error("ambiguous session id `{prefix}` matches {count} sessions:\n{candidates}\n  hint: use a longer prefix or the full id")]
    Ambiguous {
        /// The prefix the user supplied.
        prefix: String,
        /// Number of matching sessions.
        count: usize,
        /// One line per candidate (short id + started_at), already formatted.
        candidates: String,
    },
    /// `last` was requested but no sessions exist.
    #[error("no sessions recorded yet\n  hint: run `hh run -- <command>` to record one")]
    Empty,
}

/// Blob-store error.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BlobError {
    /// IO failure reading or writing a blob.
    #[error("blob io error at {path:?}: {source}")]
    Io {
        /// Blob path.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },

    /// zstd compression/decompression failure.
    #[error("zstd: {0}")]
    Zstd(String),

    /// A blob hash did not match its content (corruption).
    #[error("blob hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        /// Expected BLAKE3 hex.
        expected: String,
        /// Actual BLAKE3 hex.
        actual: String,
    },
}

/// Configuration error.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The TOML file could not be parsed.
    #[error("cannot parse config file {path:?}: {source}")]
    Parse {
        /// Path that failed.
        path: PathBuf,
        /// Underlying TOML error.
        source: toml::de::Error,
    },

    /// A config value could not be interpreted (e.g. a bad byte size).
    #[error("invalid config value: {0}")]
    Value(String),

    /// The config file could not be read.
    #[error("cannot read config file {path:?}: {source}")]
    Read {
        /// Path that failed.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
}

/// Portable session bundle (`hh-core::bundle`) build/parse error. Every
/// variant here is reachable from untrusted input (`hh import file.hh`) and
/// must carry enough detail to act on — "corrupt bundle" alone is not
/// actionable (v1.0.0 addendum: parsers over untrusted input must never
/// panic and must report precisely what failed).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BundleError {
    /// The bundle's zstd stream could not be decompressed.
    #[error("could not decompress bundle: {0}\n  hint: the file may be truncated or not an `hh` bundle at all")]
    Zstd(String),

    /// The decompressed bundle is not a valid tar archive, or contains an
    /// entry outside the allow-list (unexpected path, symlink, hardlink,
    /// absolute path, or `..` component).
    #[error("malformed bundle archive: {0}")]
    Tar(String),

    /// `manifest.json` is missing, unreadable, or not valid JSON.
    #[error("could not read bundle manifest: {0}")]
    Manifest(String),

    /// The bundle's `format_version` is newer than this build of `hh`
    /// understands.
    #[error("bundle format version {found} is newer than the highest version this build of hh supports ({max})\n  hint: upgrade hh, then retry `hh import`")]
    UnsupportedVersion {
        /// The bundle's declared format version.
        found: u32,
        /// The highest format version this build supports.
        max: u32,
    },

    /// `events.ndjson` is missing or one of its lines is not valid JSON.
    #[error("could not read bundle events: {0}")]
    Events(String),

    /// A blob's actual content hash did not match its expected hash (from
    /// its tar path or from an event/file_change reference).
    #[error("bundle blob hash mismatch: expected {expected}, got {actual}\n  hint: the bundle is corrupt or was tampered with — re-export it")]
    HashMismatch {
        /// Expected BLAKE3 hex.
        expected: String,
        /// Actual BLAKE3 hex.
        actual: String,
    },

    /// An event or file_change references a blob hash that the bundle does
    /// not carry.
    #[error("bundle is missing blob {0}, referenced by an event\n  hint: the bundle is corrupt or incomplete — re-export it")]
    MissingBlob(String),

    /// The bundle's `events.ndjson` digest did not match `manifest.integrity`.
    #[error("bundle events.ndjson does not match its recorded integrity hash\n  hint: the bundle is corrupt or was tampered with — re-export it")]
    IntegrityMismatch,
}

/// Result alias for hh-core.
pub type Result<T> = std::result::Result<T, Error>;

// Manual `From` impls that route leaf errors through the appropriate variant.
// thiserror's `#[from]` only generates one hop (e.g. `rusqlite::Error` →
// `StorageError`); it does not chain a second hop to `Error`. Without these,
// `?` on a `rusqlite::Error` inside a function returning `Result<_, Error>`
// would not compile. Routing through `Storage` keeps the layered model intact.

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Storage(StorageError::from(e))
    }
}

impl From<ResolveError> for Error {
    fn from(e: ResolveError) -> Self {
        Self::Storage(StorageError::from(e))
    }
}
