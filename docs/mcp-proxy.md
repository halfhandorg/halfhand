# `hh mcp-proxy` — stdio JSON-RPC middleman (FR-2)

`hh mcp-proxy -- <server command>` sits between an MCP client (which talks to
`hh` over stdin/stdout) and an MCP server (the child process `hh` spawns). It
forwards every newline-delimited JSON-RPC message **verbatim** in both
directions, and as a side effect records each message as an Halfhand event so a
later `hh replay`/`hh inspect` can show the model's tool traffic.

## What it records

| Direction | Message shape | Recorded event |
| --- | --- | --- |
| client → server | `{id, method, …}` (request) | `mcp_request` with `correlate_key = id` |
| client → server | `{method, …}` (notification, no `id`) | `mcp_notification` |
| server → client | `{id, result/error}` (response) | `mcp_response` with `correlates` → the matching request's event id and `latency_ms` in the body |
| server → client | `{method, …}` (notification) | `mcp_notification` |
| either | a JSON-array batch (MCP 2025-03-26) | one `mcp_notification` (the raw line is still forwarded verbatim; v0.1 records no per-element correlation) |

Payloads whose serialized JSON is ≥ 256 KiB spill to the blob store and the
event body becomes an overflow envelope `{"overflow": true, "size", "blob_hash",
"encoding": "blob"}` — the same contract the Claude Code adapter and PTY capture
use. Unparseable lines are forwarded but not recorded (wire correctness has
priority over recording completeness).

## Sessions

- **Standalone** (no `HH_SESSION_ID`): the proxy creates a fresh `mcp-only`
  session and finalizes it with the server's exit code. The epilogue prints
  `✓ Recorded mcp-only session <short-id> · exit <n> · hh replay <short-id>`.
- **Attached** (`HH_SESSION_ID` set, e.g. when launched by `hh run`): the proxy
  resolves that session, records MCP events into it on the parent's timeline
  (event `ts_ms` is relative to the parent's `started_at`), and **leaves the
  parent's lifecycle untouched** — it does not finalize. A missing id is an
  actionable error: it does not create an orphan session. The epilogue prints
  `✓ MCP proxy attached to session <id> · exit <n>`.

Within the proxy process a single `EventWriter` behind a `Mutex` serializes
appends (the intra-process single-writer rule). Attached mode is a separate
process from the parent `hh run`, so cross-process SQLite/WAL concurrency
applies — `busy_timeout(5s)` is already set.

`--session-hint <name>` is accepted (and parsed) but not yet persisted
anywhere — there is no session-name column in the schema today, so it has no
visible effect on a standalone session. It exists for forward compatibility;
adding a name column is additive and can land without a flag change.

## Wiring it into a client

Wrap `hh mcp-proxy --` as a stdio transport in the client's `mcpServers`
config. The proxy is transport-agnostic: it does not implement any MCP method
itself, it only forwards.

### Generic client

```jsonc
{
  "mcpServers": {
    "example": {
      "command": "hh",
      "args": ["mcp-proxy", "--", "python3", "/path/to/server.py"]
    }
  }
}
```

### Claude Code (`.claude.json` / managed settings)

```jsonc
{
  "mcpServers": {
    "example": {
      "command": "hh",
      "args": ["mcp-proxy", "--", "python3", "/path/to/server.py"]
    }
  }
}
```

### Attaching to a `hh run` session

To correlate an MCP server's traffic with the agent session that drove it, run
the proxy as a child of `hh run`. `hh run` exports `HH_SESSION_ID` to the
child process tree (FR-2.2), so a nested `hh mcp-proxy` attaches automatically:

```sh
hh run -- claude   # claude spawns `hh mcp-proxy -- <server>` internally → attached
```

You can also attach manually by exporting the id of a live `recording` session:

```sh
HH_SESSION_ID=<full-session-uuid> hh mcp-proxy -- python3 server.py
```