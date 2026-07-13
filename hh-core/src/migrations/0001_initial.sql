-- Halfhand migration 0001 — v0.1.0-beta.1 schema (SRS §4.1).
-- Applied idempotently by the migration runner (DR-1); do not edit in place —
-- add a new migration for future schema changes.

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE sessions (
  id            TEXT PRIMARY KEY,          -- UUIDv7
  short_id      TEXT NOT NULL UNIQUE,      -- first 6 hex chars
  started_at    INTEGER NOT NULL,          -- unix ms UTC
  ended_at      INTEGER,
  exit_code     INTEGER,
  status        TEXT NOT NULL DEFAULT 'recording',
                 -- recording | ok | error | interrupted
  agent_kind    TEXT NOT NULL,             -- claude-code | claude-desktop | codex-cli | gemini-cli | generic | mcp-only
  adapter_status TEXT NOT NULL DEFAULT 'none', -- none | active | degraded
  command       TEXT NOT NULL,             -- JSON array of argv
  cwd           TEXT NOT NULL,
  hostname      TEXT,
  hh_version    TEXT NOT NULL,
  model         TEXT,
  git_branch    TEXT,
  git_sha       TEXT,
  git_dirty     INTEGER,
  usage_json    TEXT                        -- token usage etc., adapter-provided
);

CREATE TABLE events (
  id          INTEGER PRIMARY KEY,          -- rowid
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  ts_ms       INTEGER NOT NULL,             -- ms since session start
  kind        TEXT NOT NULL,
               -- lifecycle | user_message | agent_message | thinking |
               -- tool_call | tool_result | mcp_request | mcp_response |
               -- mcp_notification | file_change | terminal_output | error
  step        INTEGER,                      -- NULL for non-step events
  summary     TEXT NOT NULL,                -- one-line, ≤ 120 chars
  body_json   TEXT,                         -- kind-specific structured payload
  blob_hash   TEXT,                         -- large payload in blob store
  correlates  INTEGER REFERENCES events(id) -- e.g. tool_result → tool_call
);
CREATE INDEX idx_events_session_step ON events(session_id, step);
CREATE INDEX idx_events_session_ts   ON events(session_id, ts_ms);

CREATE TABLE blobs (
  hash        TEXT PRIMARY KEY,             -- BLAKE3 hex
  size        INTEGER NOT NULL,             -- uncompressed bytes
  refcount    INTEGER NOT NULL DEFAULT 1
);                                          -- content on disk: blobs/<h[0..2]>/<hash>.zst

CREATE TABLE file_changes (
  event_id    INTEGER PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
  path        TEXT NOT NULL,
  change_kind TEXT NOT NULL,                -- created | modified | deleted
  before_hash TEXT,
  after_hash  TEXT,
  is_binary   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);