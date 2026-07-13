# Software Requirements Specification
## Halfhand v0.1.0-beta.1 — Local-First Flight Recorder for AI Agents

**Document version:** 1.0
**Target release:** v0.1.0-beta.1
**Language:** Rust (edition 2021, MSRV 1.75)
**License:** MIT
**Binary name:** `hh` (crate: `halfhand`)

---

## 1. Introduction

### 1.1 Purpose
This SRS defines the requirements for the first public beta of Halfhand, a CLI tool that records the execution of AI coding agents (prompts, tool calls, MCP traffic, file modifications, terminal output) into a local SQLite database, and lets developers replay and inspect those sessions after the fact.

### 1.2 Scope of v0.1.0-beta.1
Three core commands, one supporting command, one adapter:

1. `hh run <command...>` — record an agent session
2. `hh replay <session|last>` — interactive TUI playback of a recorded session
3. `hh inspect <session|last>` — non-interactive detail output (human and `--json`)
4. `hh list` — enumerate recorded sessions
5. Claude Code adapter — structured event capture from Claude Code's JSONL session logs
6. `hh mcp-proxy` — a stdio MCP proxy that records JSON-RPC traffic between an agent and an MCP server

Explicitly **out of scope** for this release (see §8): re-execution ("mock replay"), cloud sync, team features, web UI, CI report generation, non-Claude structured adapters (Codex/Gemini/Aider get PTY-level capture only), Windows PTY support beyond best-effort via `portable-pty`.

### 1.3 Definitions
- **Session** — one recorded invocation of `hh run`, identified by a 6-hex-char short ID (full ID: UUIDv7).
- **Event** — a single timestamped record within a session (agent message, tool call, MCP request/response, file change, terminal output chunk, lifecycle marker).
- **Step** — the 1-based ordinal of a *semantic* event within a session (terminal output chunks are events but not steps; see FR-3.4).
- **Adapter** — a source-specific ingester that converts an agent's native logs/hooks into Halfhand events.
- **Faithful replay** — playback of the recorded event stream exactly as captured. Halfhand v0.1 does NOT re-execute agents or models.

### 1.4 Terminology note (product requirement)
All user-facing text (CLI help, docs, README) MUST describe replay as "faithful playback of recorded execution," never "deterministic re-execution."

---

## 2. Overall Description

### 2.1 Product architecture

```
┌────────────────────────────────────────────────────────┐
│ hh run <agent cmd>                                     │
│                                                        │
│  ┌──────────────┐   ┌──────────────┐  ┌─────────────┐ │
│  │ PTY Recorder │   │ FS Watcher   │  │ Adapter:    │ │
│  │ (portable-   │   │ (notify +    │  │ Claude Code │ │
│  │  pty)        │   │  blob store) │  │ JSONL tail  │ │
│  └──────┬───────┘   └──────┬───────┘  └──────┬──────┘ │
│         └──────────────────┼─────────────────┘        │
│                     event channel (mpsc)               │
│                            │                           │
│                    ┌───────▼────────┐                  │
│                    │ Writer task    │                  │
│                    │ (rusqlite, WAL)│                  │
│                    └───────┬────────┘                  │
│                            │                           │
│              ~/.local/share/halfhand/hh.db             │
│              ~/.local/share/halfhand/blobs/            │
└────────────────────────────────────────────────────────┘
         hh replay ──────► ratatui TUI reader
         hh inspect ─────► plain/JSON reader
         hh mcp-proxy ───► JSON-RPC stdio middleman → events
```

### 2.2 Operating environment
- macOS (aarch64, x86_64) and Linux (x86_64, aarch64): fully supported.
- Windows: compiles and runs; PTY capture via `portable-pty`/ConPTY is best-effort; FS watching supported.
- No network access is ever initiated by `hh` in v0.1. This is a hard requirement (NFR-2).

### 2.3 Data location
- Config: `$XDG_CONFIG_HOME/halfhand/config.toml` (default `~/.config/halfhand/config.toml`), platform-appropriate via the `directories` crate.
- Data: `$XDG_DATA_HOME/halfhand/` containing `hh.db` (SQLite) and `blobs/` (content-addressed, zstd-compressed file snapshots).
- `HH_DATA_DIR` env var overrides the data directory.

---

## 3. Functional Requirements

### FR-1: `hh run` — Record

**FR-1.1** `hh run [OPTIONS] -- <command> [args...]` spawns `<command>` inside a PTY, transparently proxying the user's terminal (stdin, stdout, window resize, signals). The wrapped agent MUST behave interactively exactly as if run directly: colors, cursor addressing, Ctrl-C forwarding, and terminal resize must all work.

**FR-1.2** On start, `hh run` creates a session row with: UUIDv7 id, short id (first 6 hex of the UUID), start timestamp (UTC, ms precision), working directory, full command line, detected agent kind (`claude-code`, `generic`), hostname, `hh` version, and git metadata if the cwd is a repo (branch, HEAD sha, dirty flag).

**FR-1.3 Terminal capture.** All PTY output bytes are recorded as `terminal_output` events, chunked (flush on 8 KiB or 50 ms, whichever first), with monotonic timestamps relative to session start. Raw bytes are stored (ANSI preserved). User keystrokes are NOT recorded by default (privacy); `--record-input` opts in.

**FR-1.4 File change capture.** A recursive watcher (via `notify`) monitors the working directory (honoring `.gitignore` plus a built-in ignore list: `.git/`, `node_modules/`, `target/`, `~/.local/share/halfhand`). On create/modify/delete of a file ≤ 4 MiB (configurable `max_file_size`):
- The new content is stored in the blob store keyed by BLAKE3 hash, zstd-compressed.
- A `file_change` event records: path (relative to cwd), change kind, before-hash (if known), after-hash. The pre-session baseline is captured lazily: the first time a file changes, its pre-change content is read and stored as the "before" blob if the change event allows (best effort; deletes store tombstones).
- Binary files (heuristic: NUL byte in first 8 KiB) store hashes and sizes but content storage is optional (`--record-binary`, default off).

**FR-1.5 Claude Code adapter.** When the wrapped command is detected as Claude Code (binary name `claude`, or `--adapter claude-code` forced):
- `hh` tails the session JSONL file Claude Code writes under `~/.claude/projects/<project-slug>/` (discovering the newest file created after session start that matches the cwd's slug).
- Each JSONL entry is converted to structured events: `agent_message` (assistant text), `user_message` (prompts), `tool_call` (name + input JSON), `tool_result` (output, truncated at 256 KiB with truncation flag), `thinking` (if present in the log), token/usage metadata attached to the session.
- Adapter failures (file not found, parse errors) MUST degrade gracefully: log a single warning line to stderr, continue PTY/FS recording, and mark the session `adapter_status = 'degraded'`.

**FR-1.6 Session end.** On agent exit (or `hh` receiving SIGTERM), the session row is finalized with end timestamp, duration, exit code, event count, and a summary of files changed. A one-line epilogue is printed:
`✓ Recorded session a1b2c3 · 4m32s · 42 steps · 7 files changed · hh replay a1b2c3`

**FR-1.7 Crash safety.** Events are written continuously (SQLite WAL). If `hh` itself crashes or is killed, the session must be readable and marked `status = 'interrupted'` on next `hh list`/`hh replay` (detected via missing end timestamp + no live PID).

### FR-2: `hh mcp-proxy` — MCP traffic capture

**FR-2.1** `hh mcp-proxy --session-hint <name> -- <mcp-server-cmd> [args...]` runs an MCP server as a child process, forwarding stdio JSON-RPC in both directions unmodified, while recording each request/response/notification as `mcp_request` / `mcp_response` / `mcp_notification` events (method, params, result/error, correlated by JSON-RPC id, latency in ms).

**FR-2.2** Proxied events attach to the active `hh run` session when one exists (discovery via `HH_SESSION_ID` env var that `hh run` sets for its child process tree); otherwise they create a standalone `mcp-only` session.

**FR-2.3** Payloads larger than 256 KiB are stored in the blob store and referenced by hash. The proxy MUST add < 5 ms p50 latency per message (NFR-1).

**FR-2.4** Documentation MUST include a copy-paste example of editing an agent's MCP config to interpose the proxy (e.g. wrapping a server command with `hh mcp-proxy --`).

### FR-3: `hh replay` — Interactive playback

**FR-3.1** `hh replay <session-id|last>` opens a full-screen ratatui TUI. `last` resolves to the most recently started session. Prefix matching on short ids is supported (ambiguity is an error listing candidates).

**FR-3.2 Layout.** Three regions:
- Header: session id, agent, model (if known), start time, duration, step position `[step 7/42]`.
- Timeline pane (left, scrollable): one line per step — relative timestamp, colored event-kind badge (AGENT/USER/TOOL/MCP/FILE/ERR), and a one-line summary.
- Detail pane (right/bottom): full content of the selected step — message text, pretty-printed tool input/output JSON, unified diff for file changes (via the `similar` crate, syntax-colored ± lines), MCP request/response pair.

**FR-3.3 Keybindings.** `n`/`j`/`↓` next step · `p`/`k`/`↑` previous · `g`/`G` first/last · `J` jump-to-timestamp prompt (`mm:ss` or `hh:mm:ss`) · `/` filter steps by kind or substring · `Enter` toggle full-screen detail · `d` next diff · `Tab` switch pane focus · `?` help overlay · `q`/`Esc` quit. All navigation is keyboard-only; no mouse required.

**FR-3.4 Step semantics.** Steps are the ordered semantic events: agent/user messages, tool calls+results (a call and its result are one step, shown together), MCP request/response pairs (one step), file changes, and error/lifecycle markers. Raw terminal chunks are not steps but are viewable via a toggle (`t`) that shows the terminal output between the current and next step.

**FR-3.5 Performance.** Opening a session with 10,000 events must render in < 300 ms; navigation must feel instant (< 16 ms per step change) via lazy loading of detail bodies (NFR-1).

**FR-3.6** Degraded sessions (PTY-only, no adapter) still replay: steps are derived from file changes plus terminal-output segments, so the command is never useless.

### FR-4: `hh inspect` — Non-interactive analysis

**FR-4.1** `hh inspect <session|last>` with no flags prints a session summary: metadata, step table (timestamp, kind, summary), files-changed list with +/- line counts.

**FR-4.2** `--step <N>` prints the full detail of step N (same content as the replay detail pane, rendered for a plain terminal; diffs in unified format, colored when stdout is a TTY, plain when piped).

**FR-4.3** `--json` emits stable, documented JSON (single object for `--step`, NDJSON stream of events for whole-session) suitable for `jq`. The JSON schema is versioned (`"schema": 1`) and documented in `docs/json.md`.

**FR-4.4** `--diff` prints the concatenated unified diff of all file changes in the session (a "what did the agent do to my repo" view). `--diff --step N` limits to that step.

**FR-4.5** `hh inspect <session> --failed` jumps to the first step whose kind is `error` or whose tool result has nonzero exit / `is_error`, if any.

### FR-5: `hh list`

**FR-5.1** Tabular listing (newest first): short id, start time (humanized), agent, duration, steps, files changed, status (`ok`/`error`/`interrupted`/`recording`). `--json` supported. `--limit N` (default 20).

### FR-6: Housekeeping (minimal for beta)

**FR-6.1** `hh delete <session>` removes a session and garbage-collects unreferenced blobs (with `--yes` to skip confirmation).
**FR-6.2** `hh --version`, `hh --help`, and per-subcommand `--help` with examples. Help output is the product's first impression: it MUST include one usage example per subcommand.

---

## 4. Data Requirements

### 4.1 SQLite schema (DDL, migration 0001)

```sql
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
  agent_kind    TEXT NOT NULL,             -- claude-code | generic | mcp-only
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
```

**DR-1** Migrations are embedded in the binary and applied automatically and idempotently on any command that opens the DB.
**DR-2** Users may query `hh.db` directly with sqlite3; the schema above is a public, documented interface for the beta (documented as "may change before 1.0").

### 4.2 Config file (`config.toml`)

```toml
[record]
max_file_size = "4MiB"
record_input = false
record_binary = false
ignore = ["dist/", "*.lock"]      # extends built-in + .gitignore

[storage]
data_dir = ""                     # empty = platform default

[replay]
theme = "auto"                    # auto | dark | light
```

All keys optional; unknown keys warn, never fail.

---

## 5. Non-Functional Requirements

**NFR-1 Performance.** Recording overhead ≤ 5% wall-clock on a typical Claude Code session; PTY passthrough latency imperceptible (< 5 ms p99 per chunk); MCP proxy < 5 ms p50 added latency; `hh replay` open < 300 ms for 10k-event sessions; sustained event ingest ≥ 5,000 events/s.

**NFR-2 Privacy & network.** `hh` makes zero network calls. No telemetry, no update checks, no crash reporting in v0.1. Enforced in CI by a test that the binary links no HTTP client and by code review policy.

**NFR-3 Reliability.** No data loss on agent crash or `hh` SIGKILL beyond the last unflushed ≤ 50 ms of terminal output. All DB writes go through a single writer task; WAL mode; fsync on session finalize.

**NFR-4 Security.** DB and blobs created with 0600/0700 permissions. `--record-input` defaults off because keystrokes may contain secrets. Docs include a prominent note that recorded sessions can contain secrets present in prompts/outputs, plus guidance on `hh delete`.

**NFR-5 Code quality.** `cargo clippy -- -D warnings` (with `clippy::pedantic` selectively enabled), `cargo fmt --check`, no `unwrap()`/`expect()` outside tests and `main` bootstrap; errors via `thiserror` in the library, `anyhow` (with context) in the binary; every public item documented; `cargo deny` for license/advisory checks.

**NFR-6 Testing.** Unit tests for parsers/store; integration tests that run `hh run -- <fixture script>` end-to-end in a temp dir and assert on the resulting DB; snapshot tests (`insta`) for `inspect` output and JSONL-adapter conversion using committed fixture files of real Claude Code JSONL (sanitized); TUI logic tested via ratatui's `TestBackend`. Target ≥ 75% line coverage on `core`.

**NFR-7 UX polish.** Startup and epilogue messages use color (respecting `NO_COLOR` and TTY detection), a consistent glyph set (✓ ✗ ●), and humanized durations. All errors are actionable: what failed, why, and one suggested next step.

**NFR-8 Distribution.** `cargo install halfhand` works from a clean checkout; release CI builds static-ish binaries for mac/linux (musl where feasible); `--version` embeds git sha.

---

## 6. Crate & dependency plan

Workspace with three crates:

| Crate | Purpose | Key deps |
|---|---|---|
| `hh-core` | store, blob store, event model, adapters, diffing | `rusqlite` (bundled), `serde`/`serde_json`, `blake3`, `zstd`, `similar`, `uuid` (v7), `time`, `ignore`, `notify`, `thiserror` |
| `hh-record` | PTY runner, FS watcher, MCP proxy | `portable-pty`, `tokio` (or threads + channels — implementer's choice, justify in ADR), `signal-hook` |
| `hh` (bin) | CLI, TUI, output rendering | `clap` (derive), `ratatui`, `crossterm`, `anyhow`, `owo-colors`, `directories`, `indicatif` (sparingly) |

An `ARCHITECTURE.md` and one ADR per major decision (async runtime or not; JSONL tailing strategy) are deliverables.

---

## 7. Acceptance Criteria (release gate for v0.1.0-beta.1)

1. `hh run -- claude` on a real Claude Code session produces a session with structured steps (messages, tool calls, results), file diffs, and terminal output; `hh replay last` plays it back with all keybindings functional.
2. `hh run -- python3 fixture_agent.py` (a scripted fake agent that prints, writes files, and exits nonzero) yields a `generic` session where replay shows file diffs and terminal segments, and `inspect --failed` finds the error step.
3. MCP proxy demo: wrapping a stdio MCP server records paired request/response events visible in replay.
4. Kill -9 the agent mid-run: session is readable and marked `interrupted`.
5. `hh inspect last --json | jq` round-trips; schema documented.
6. Fresh machine test: `cargo install --path .` → record → replay in under 2 minutes of user effort with only README guidance.
7. CI green: fmt, clippy -D warnings, tests, cargo-deny, on macOS + Linux (+ Windows build-only job).

---

## 8. Out of Scope / Roadmap pointers (do not build in beta)

- Mock re-execution against recorded tool outputs ("true deterministic replay") — roadmap, likely a paid differentiator.
- Session export/import (`hh export` tarball) — v0.2.
- Adapters for Codex CLI, Gemini CLI, Aider, OpenHands — v0.2+ behind a trait already defined in `hh-core` (`trait Adapter`).
- Cloud sync, team sharing, web viewer, CI PR reports, retention policies, SSO — commercial tier.
- Redaction/secret-scrubbing pipeline — high priority for v0.2 (design doc requested, not implementation).
