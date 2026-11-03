//! Embedded, idempotent SQLite migrations (SRS §4.1, DR-1).
//!
//! Each migration's DDL lives in [`migrations/`](./migrations) and is applied
//! in order; the runner records applied versions in `schema_migrations` and
//! skips them on subsequent opens, so calling
//! [`crate::store::Store::open`] repeatedly never errors. Adding a migration
//! is additive and append-only: bump [`LATEST_VERSION`], append to
//! [`MIGRATIONS`], add the `migrations/00NN_*.sql` file. The v1.0.0 addendum
//! permits additive schema changes (a new index, a new table) without a
//! deprecation cycle; a *breaking* change to an existing table must go through
//! STABILITY.md's policy instead.

/// The highest migration version applied by a fresh [`crate::store::Store::open`].
pub const LATEST_VERSION: i64 = 8;

/// The ordered migration set: `(version, DDL)`. Applied in ascending order;
/// each is run only when the DB's recorded `schema_migrations.version` is below
/// it. Migration 0001 carries the `PRAGMA` statements that must run outside a
/// transaction, so each migration is executed with `execute_batch` (no
/// surrounding transaction), then its version is recorded.
///
/// Migration 0005 has a Rust-side companion: `Store::open` adds the
/// `adapter_degrade_reason` column via `PRAGMA table_info` check (SQLite has
/// no `ALTER TABLE ADD COLUMN IF NOT EXISTS`), then runs this DDL. Both halves
/// are idempotent (DR-4): re-opening a v1.1.0 DB no-ops the column check and
/// the `DROP TABLE IF EXISTS` / `CREATE ... IF NOT EXISTS` statements.
///
/// Migration 0006 follows the same pattern: `Store::open` adds
/// `file_changes.before_source` and `file_changes.renamed_from` behind a
/// `PRAGMA table_info` check after the migration runner has applied the DDL
/// (an `IF NOT EXISTS` index on `before_hash`, which does not reference the
/// new columns).
///
/// Migration 0007 is all Rust: `Store::open` adds the
/// [`SESSION_CONTEXT_COLUMNS`] behind `PRAGMA table_info` checks; its SQL file
/// only documents them.
///
/// Migration 0008 is all Rust too: `Store::open` adds the
/// [`RECORDER_IDENTITY_COLUMNS`] the same way.
pub const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("migrations/0001_initial.sql")),
    (2, include_str!("migrations/0002_events_heal_index.sql")),
    (3, include_str!("migrations/0003_imported_from.sql")),
    (4, include_str!("migrations/0004_events_fts.sql")),
    (
        5,
        include_str!("migrations/0005_v1_1_0_degrade_and_fts.sql"),
    ),
    (
        6,
        include_str!("migrations/0006_file_change_before_source.sql"),
    ),
    (7, include_str!("migrations/0007_session_context.sql")),
    (8, include_str!("migrations/0008_recorder_identity.sql")),
];

/// Migration 0007's `sessions` columns, as `(name, SQLite type)`, in the
/// order `Store::open` adds them. Each is added behind a `PRAGMA table_info`
/// check, so re-running is a no-op and a partially-applied 0007 (some
/// columns present) completes cleanly. All are nullable; NULL means unknown.
pub const SESSION_CONTEXT_COLUMNS: &[(&str, &str)] = &[
    ("name", "TEXT"),
    ("model_id", "TEXT"),
    ("model_display", "TEXT"),
    ("provider", "TEXT"),
    ("context_limit", "INTEGER"),
    ("input_tokens", "INTEGER"),
    ("output_tokens", "INTEGER"),
    ("cache_read_tokens", "INTEGER"),
    ("cache_write_tokens", "INTEGER"),
    ("reasoning_tokens", "INTEGER"),
    ("capture_tier", "TEXT"),
    ("adapter_id", "TEXT"),
];

/// Migration 0008's `sessions` columns, as `(name, SQLite type)`: the
/// recording process's id and its platform start-time token. Together they
/// let `Store::mark_stale_interrupted` tell a live recording from a crashed
/// one even when the pid has been reused. Added behind `PRAGMA table_info`
/// checks like [`SESSION_CONTEXT_COLUMNS`]; both nullable, and NULL (a
/// pre-0008 row) means the recorder is unknown.
pub const RECORDER_IDENTITY_COLUMNS: &[(&str, &str)] = &[
    ("recorder_pid", "INTEGER"),
    ("recorder_start_time", "INTEGER"),
];

/// Migration 0001: the v0.1.0-beta.1 schema, verbatim from SRS §4.1.
pub const MIGRATION_0001: &str = include_str!("migrations/0001_initial.sql");
