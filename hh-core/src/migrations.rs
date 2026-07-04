//! Embedded, idempotent SQLite migrations (SRS §4.1, DR-1).
//!
//! Migration `0001` applies the exact DDL from SRS §4.1. The runner is
//! idempotent: it records applied versions in `schema_migrations` and skips
//! them on subsequent opens, so calling [`crate::store::Store::open`] repeatedly never errors.

/// The single migration's version number.
pub const LATEST_VERSION: i64 = 1;

/// Migration 0001: the v0.1.0-beta.1 schema, verbatim from SRS §4.1.
pub const MIGRATION_0001: &str = include_str!("migrations/0001_initial.sql");
