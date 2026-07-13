# ADR 0003: FTS5 Full-Text Search Index Strategy

## Status

Accepted (2026-07-14)

## Context

Halfhand needs cross-session full-text search (`hh search <query>`) over event
summaries and message bodies. The events table already stores structured data
in SQLite, but SQLite's `LIKE` operator on `body_json` would be too slow for
interactive search across thousands of events.

SQLite provides FTS5 — a full-text search virtual table — as a bundled extension
(via `rusqlite`). The question is *when* to build the FTS index:

1. **At record time** (in the writer thread, alongside event insertion)
2. **On first search** (lazy, scanning all events)
3. **Via SQLite triggers** (on INSERT/UPDATE/DELETE on the events table)

## Decision

**Build the FTS index incrementally at record time in the writer thread.**

## Rationale

### Why not on first search?

- The first search on a large store would need to scan every event, extract
  text from `body_json`, and insert into the FTS table — potentially taking
  seconds for a 100k-event store.
- The index build would block the search response, making `hh search` feel
  slow on its first use.
- We would need to track which events have been indexed (a flag column or a
  separate tracking table), adding complexity.

### Why not via SQLite triggers?

- Triggers on the `events` table would fire on every INSERT, including bulk
  imports and the step-assignment pass — adding overhead to operations that
  don't need FTS indexing.
- Triggers cannot easily call Rust code to extract text from `body_json`
  (which is a JSON blob, not plain text).
- The single-writer pattern means the writer thread already serializes all
  event writes; adding an FTS insert there is simpler than maintaining
  trigger logic.

### Why the writer thread?

- The writer thread already has the event data in memory (summary + body_json)
  after inserting the event row. Adding an FTS insert is one extra SQL
  statement with no additional reads.
- The writer thread is the single point of serialization for all event writes,
  so the FTS index is always consistent with the events table.
- No background jobs, no catch-up scans, no tracking state.
- The FTS5 INSERT is cheap (it's just indexing the summary + extracted body
  text, which is already in memory).

### Crash safety

If `hh` crashes mid-write, the FTS index may be missing the last few events.
This is acceptable because:
- The events themselves are durable (WAL mode).
- On next `Store::open`, `heal_steps` already re-processes events with NULL
  steps; a similar catch-up for FTS could be added if needed, but the window
  is at most 50 ms of events (the terminal chunk flush interval).
- A missing FTS entry means the event won't appear in search results — it
  does not corrupt the index or the events table.

## Consequences

- **Positive**: Search is always fast, no first-search delay, no background
  jobs, no trigger complexity.
- **Positive**: The FTS index is always consistent with the events table
  (modulo crash window).
- **Negative**: Slightly more work per event insert (one extra INSERT into
  the FTS table). This is negligible — FTS5 is designed for this pattern.
- **Negative**: The FTS index adds disk space proportional to the total text
  content of all events. For a typical session with ~1000 events, this is
  well under 1 MiB.

## Implementation

The FTS5 virtual table is created by migration 0004:

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
    session_id UNINDEXED,
    summary,
    body_text,
    kind UNINDEXED,
    step UNINDEXED,
    tokenize='porter unicode61'
);
```

The writer thread inserts into `events_fts` after every `INSERT INTO events`:

```rust
let body_text = extract_fts_text(&event.body_json);
conn.execute(
    "INSERT INTO events_fts (rowid, session_id, summary, body_text, kind, step)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    params![new_id, event.session_id, event.summary, body_text,
            event.kind.to_string(), event.step],
)?;
```

The `extract_fts_text` function extracts a flat text string from `body_json`:
- For text events: extract the `text` field
- For tool calls: extract `name` + `input`
- For tool results: extract `content`
- For error events: extract `reason`
- For overflow envelopes: return empty string (blob content is not indexed)

The FTS index is maintained on:
- **Delete**: `DELETE FROM events_fts WHERE rowid IN (SELECT id FROM events WHERE session_id = ?)`
- **Redact**: `UPDATE events_fts SET summary = ?, body_text = ? WHERE rowid = ?`
