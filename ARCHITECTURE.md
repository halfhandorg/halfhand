# Halfhand — Architecture (one page)

Halfhand is a local-first CLI flight recorder for AI agents. It records an
agent's execution (prompts, tool calls, MCP traffic, file changes, terminal
output) into a local SQLite database and replays it faithfully. It never makes
network calls (SRS NFR-2).

## Crates (SRS §6)

```
halfhand (bin `hh`)   ──depends on──>   hh-core
hh-record             ──depends on──>   hh-core
```

- **`hh-core`** — the data layer. Storage (SQLite + migrations), the
  content-addressed blob store, the event/session data model, and config/path
  resolution. Pure library: no I/O runtime, no CLI. `missing_docs` is denied;
  `clippy::pedantic` is on (workspace lints).
- **`hh-record`** — the recorder layer (PTY runner, FS watcher, MCP proxy).
  Stub in this skeleton; the recorder is not yet implemented. The
  threads-vs-tokio decision is recorded in
  [`docs/adr/0001-threads-vs-tokio.md`](docs/adr/0001-threads-vs-tokio.md).
- **`hh` (crate `halfhand`)** — the binary. clap-derive CLI, output rendering.
  Delegates to `hh-core` (and, later, `hh-record`).

## Data flow (recording)

```
PTY recorder ─┐
FS watcher   ─┼──> mpsc channel ──> writer task (own Connection, WAL) ──> hh.db
MCP proxy    ─┘                                                  └──> blobs/<h2>/<hash>.zst
adapter      ─┘
```

- One **writer task** owns one SQLite `Connection` and drains an `mpsc`
  channel. No `Connection` is shared across threads (CLAUDE.md).
- The store's main `Connection` handles session lifecycle (`create_session`,
  `finalize_session`) and reads (`list_sessions`, `resolve_session`); the
  writer's separate `Connection` handles `append_event`. WAL mode allows the
  main thread to read concurrently with the writer.
- Blobs are BLAKE3-keyed, zstd-compressed, refcounted in the `blobs` table.
  Deleting a session decrements refcounts and GCs blobs that reach zero.

## Storage (SRS §4.1)

- SQLite, WAL, `foreign_keys = ON`. Schema is applied as embedded migration
  `0001`, idempotently (DR-1): `schema_migrations` records the applied version;
  the runner probes `sqlite_master` before running the DDL (the DDL itself
  creates `schema_migrations`).
- `Store` exposes: `create_session`, `finalize_session`, `event_writer`
  (→ `EventWriter::append_event`), `list_sessions`, `resolve_session`,
  `delete_session`.
- `resolve_session` implements FR-3.1: `last` → newest session; short-id prefix
  → unique match; ambiguity → error listing candidates.

## Config (SRS §4.2)

- `Paths::resolve(config)` precedence: `HH_DATA_DIR` env > `[storage] data_dir`
  file > platform default (`directories` crate). Unknown keys warn on stderr
  but never fail.

## Security (NFR-4)

- DB and blob dirs created `0700`; blob files written `0600` (Unix). Atomic
  write via temp + rename + fsync. `--record-input` defaults off.

## Out of scope here

`hh run`, `hh mcp-proxy`, the TUI, and the Claude Code adapter are not
implemented in this skeleton — their subcommands return a structured "not
implemented" error. See the ADRs for the runtime decision.