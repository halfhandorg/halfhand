-- Migration 0004: FTS5 full-text search index over event summaries and bodies.
--
-- Built incrementally at record time by the writer thread (see `insert_event`
-- in `store.rs`). The FTS index is kept in sync with the events table by:
--   - INSERT via the writer thread (record time)
--   - DELETE when a session is deleted (Store::delete_session)
--   - UPDATE when an event is redacted (Store::redact_session)
--
-- The `porter unicode61` tokenizer provides English stemming (so "create"
-- matches "creating" and "created") and Unicode-aware tokenization.
--
-- Additive per the v1.0.0 addendum: a new virtual table, no breaking schema
-- change. Existing databases get the index on first open after upgrade.
CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
    session_id UNINDEXED,
    summary,
    body_text,
    kind UNINDEXED,
    step UNINDEXED,
    tokenize='porter unicode61'
);
