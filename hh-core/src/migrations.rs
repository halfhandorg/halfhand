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
pub const LATEST_VERSION: i64 = 3;

/// The ordered migration set: `(version, DDL)`. Applied in ascending order;
/// each is run only when the DB's recorded `schema_migrations.version` is below
/// it. Migration 0001 carries the `PRAGMA` statements that must run outside a
/// transaction, so each migration is executed with `execute_batch` (no
/// surrounding transaction), then its version is recorded.
pub const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("migrations/0001_initial.sql")),
    (2, include_str!("migrations/0002_events_heal_index.sql")),
    (3, include_str!("migrations/0003_imported_from.sql")),
];

/// Migration 0001: the v0.1.0-beta.1 schema, verbatim from SRS §4.1.
pub const MIGRATION_0001: &str = include_str!("migrations/0001_initial.sql");
