-- Migration 0005: v1.1.0 additions (SRS §5.1 "migration 0002").
--
-- Implements two additive schema changes required by SRS v1.1.0:
--
--   1. `sessions.adapter_degrade_reason TEXT` (BUG-1): a machine-readable
--      degrade code persisted alongside `adapter_status = 'degraded'`. The
--      column is added by Rust code in `Store::open` (PRAGMA table_info
--      check) because SQLite does not support `ALTER TABLE ADD COLUMN IF
--      NOT EXISTS`. The Rust check makes this idempotent.
--
--   2. FTS5 external-content search index (ENH-4): replaces the v1.0.0
--      standalone FTS5 table (migration 0004) with the SRS-mandated
--      external-content pattern. The FTS table is derived data (`events` is
--      the source of truth), so dropping and recreating is safe; the
--      backfill reindexes every existing event.
--
-- Idempotency (DR-4): `DROP TABLE IF EXISTS` + `CREATE VIRTUAL TABLE` +
-- `CREATE TRIGGER IF NOT EXISTS` make the FTS parts safely re-runnable.
-- The `adapter_degrade_reason` column is checked in Rust before adding.

-- FTS5 full-text search index over event summaries and extracted body text
-- (SRS §5.1, ENH-4).
--
-- SRS deviation (flagged): the SRS specifies `content=events, content_rowid=id`
-- (external-content pattern). External-content FTS5 requires the content
-- table (`events`) to have columns matching the FTS table's column names
-- (`summary`, `body_text`). The `events` table has `summary` and `body_json`
-- — no `body_text` column — so `content=events` fails on any content-table
-- read (`SELECT COUNT(*) FROM events_fts`, `'delete'`-command lookups,
-- `snippet()`) with "no such column: T.body_text". Adding a `body_text`
-- column to `events` would be a non-additive schema change (violating the
-- v1.0.0 addendum) and would duplicate the extracted text on every row.
--
-- We use a **standalone** FTS5 table (no `content=`), kept in sync by
-- triggers only (CLAUDE.md v1.1.0 addendum: "FTS5 stays in sync via
-- triggers only. Never write to events_fts from application code"). This
-- supports `snippet()` (required by ENH-4.2) and the `'delete'` command,
-- and avoids the column-name mismatch. The FTS index stores its own copy
-- of the extracted text (the trade-off vs external-content: ~2x storage
-- for the indexed text, acceptable for a local-first tool). The v1.0.0
-- standalone table from migration 0004 is dropped first; its data is
-- derived from `events` and is reindexed by the backfill below.
DROP TABLE IF EXISTS events_fts;

CREATE VIRTUAL TABLE events_fts USING fts5(
  summary,
  body_text,
  tokenize='porter unicode61'
);

-- Triggers to keep FTS in sync (SRS §5.1 + CLAUDE.md: "FTS5 stays in sync
-- via triggers only. Never write to events_fts from application code").
-- The FTS rowid is set to `events.id` so `JOIN events e ON e.id =
-- events_fts.rowid` (the search query) resolves correctly.
CREATE TRIGGER IF NOT EXISTS events_fts_ai AFTER INSERT ON events BEGIN
  INSERT INTO events_fts(rowid, summary, body_text)
    VALUES (new.id, new.summary, hh_body_to_text(new.body_json));
END;

-- For a standalone (non-external-content) FTS5 table, the 'delete' command
-- requires the exact values that were stored — fragile if `hh_body_to_text`
-- ever returns a different value (e.g. NULL vs empty). A plain `DELETE FROM`
-- by rowid is simpler and robust, and is the standard pattern for standalone
-- FTS5 tables.
CREATE TRIGGER IF NOT EXISTS events_fts_ad AFTER DELETE ON events BEGIN
  DELETE FROM events_fts WHERE rowid = old.id;
END;

-- AFTER UPDATE OF summary, body_json: the FTS index only depends on these
-- two columns. Narrowing to `OF summary, body_json` avoids firing on
-- `step`/`correlates` updates (heal_steps, correlate resolution) which
-- would needlessly delete+reinsert FTS rows with identical content.
-- Standard FTS5 update = delete-old + insert-new.
CREATE TRIGGER IF NOT EXISTS events_fts_au AFTER UPDATE OF summary, body_json ON events BEGIN
  DELETE FROM events_fts WHERE rowid = old.id;
  INSERT INTO events_fts(rowid, summary, body_text)
    VALUES (new.id, new.summary, hh_body_to_text(new.body_json));
END;

-- Backfill FTS for existing events (run once at migration time).
-- `hh_body_to_text` must be registered on the connection before this runs;
-- `Store::open` registers it before calling `run_migrations`.
INSERT INTO events_fts(rowid, summary, body_text)
  SELECT id, summary, hh_body_to_text(body_json) FROM events;