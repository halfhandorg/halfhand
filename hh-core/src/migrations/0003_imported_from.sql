-- Migration 0003: track provenance for imported sessions (`hh import`).
--
-- Additive per the v1.0.0 addendum: a new nullable column, default NULL for
-- every existing row. `imported_from` holds the *original* session id (from
-- the source machine's `hh export --bundle`) when this session was created
-- via `hh import`; NULL for every locally-recorded session. `hh import`
-- always mints a fresh local id (see `Store::import`) rather than reusing the
-- original, so this column is the only record of where an imported session
-- came from.
ALTER TABLE sessions ADD COLUMN imported_from TEXT;
