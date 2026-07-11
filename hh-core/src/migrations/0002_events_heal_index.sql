-- Migration 0002: a partial index to make `Store::heal_steps` cheap.
--
-- `Store::open` self-heals sessions whose semantic events still have a NULL
-- step (a crashed finalize, or an attached MCP proxy's late events). The heal
-- probe is `SELECT DISTINCT session_id FROM events WHERE step IS NULL AND
-- kind != 'terminal_output'`. Without an index that is an O(all-events) table
-- scan on *every* `hh` invocation — including read-only `hh list`, which made
-- cold start scale with total recorded history instead of with what actually
-- needs healing. On a 100k+ event DB that scan dominated startup (Area 4).
--
-- This partial index stores exactly the rows the probe looks for, so the
-- planner scans O(rows-needing-heal) instead of O(all events): ~0 in steady
-- state (everything is healed), and bounded by the (small) set of crashed
-- sessions otherwise. Additive — a new index, no breaking schema change per
-- the v1.0.0 addendum.
CREATE INDEX IF NOT EXISTS idx_events_needs_heal
  ON events(session_id)
  WHERE step IS NULL AND kind != 'terminal_output';