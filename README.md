# Halfhand 

Halfhand is a local-first CLI flight recorder for AI agents. It captures an agent session on your own machine, stores it in a local SQLite database, and lets you replay or inspect it later without sending the data to a remote service.


![](docs/assets/halfhand-cli.png)

The tool is designed for privacy and reproducibility: recordings stay local, and you can point Halfhand at a custom data directory with `HH_DATA_DIR`.

## What it does

- Records agent runs from the command line.
- Captures terminal output and file changes during a session.
- Stores sessions locally so you can replay, inspect, and delete them later.
- Keeps data on disk rather than relying on a hosted service.

## Quick start

Build the CLI from the repository root:

```bash
cargo build -p hh
```

Run a recorded session:

```bash
HH_DATA_DIR=/tmp/halfhand cargo run -p hh -- run -- your-command-here
```

Example:

```bash
HH_DATA_DIR=/tmp/halfhand cargo run -p hh -- run -- python3 my_agent.py
```

## Usage

After the binary is built, the main commands are:

### 1. Record a session

```bash
hh run -- <command> [args...]
```

Examples:

```bash
hh run -- claude
hh run --record-input -- python3 my_agent.py
hh run -- --help
```

Flags:

- `--record-input` captures user keystrokes as well as terminal output.
- `--adapter <name>` forces a specific adapter instead of auto-detection.

### 2. Replay the latest session

```bash
hh replay last
```

You can also replay a specific session by id:

```bash
hh replay a1b2c3
```

### 3. Inspect a session

```bash
hh inspect last
```

Useful flags:

```bash
hh inspect last --step 7
hh inspect last --json
hh inspect last --diff
hh inspect last --failed
```

### 4. List recorded sessions

```bash
hh list
```

Useful flags:

```bash
hh list --limit 50
hh list --json
```

### 5. Delete a session

```bash
hh delete <session-id> --yes
```

Example:

```bash
hh delete last --yes
```

### 6. Run an MCP proxy session

```bash
hh mcp-proxy -- <command> [args...]
```

Example:

```bash
hh mcp-proxy -- uvx my-mcp-server
```

## Configuration

Halfhand stores its data under a directory resolved from the following sources, in order:

1. `HH_DATA_DIR`
2. A configured `[storage] data_dir` value
3. The platform default data directory

Example:

```bash
HH_DATA_DIR=/tmp/halfhand hh list
```

## Structured capture with adapters

Every `hh run` captures terminal output and file changes regardless of what
you run. On top of that, Halfhand can detect certain agents and record their
internal turns as structured events (prompts, thinking, tool calls, tool
results) instead of just raw terminal bytes.

- **Claude Code** is auto-detected when the wrapped command's basename is
  `claude` (e.g. `hh run -- claude`). Halfhand tails Claude's own transcript
  (`~/.claude/projects/<project>/*.jsonl`) as it's written and turns each
  record into an event, correlating a `tool_result` back to the `tool_call`
  that produced it.
- Force a specific adapter instead of relying on auto-detection:

  ```bash
  hh run --adapter claude-code -- claude
  ```

- If the adapter can't locate a transcript (e.g. no `~/.claude/projects`
  directory), it degrades gracefully: you still get the full terminal and
  file-change recording, just without the structured breakdown. Nothing about
  the session fails because of this.
- A session's `agent_kind` and `adapter_status` (`active` / `degraded` /
  `none`) show up in `hh list --json` and `hh inspect`, so you can tell at a
  glance whether structured events were captured for a given run.

## Recording MCP traffic with `hh mcp-proxy`

`hh mcp-proxy` sits between an MCP client and an MCP server on stdio,
forwarding every JSON-RPC message verbatim in both directions while quietly
recording it. It's a drop-in wrapper: point your MCP client at `hh mcp-proxy
-- <server command>` instead of the server directly.

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

What gets recorded:

- Requests and responses are correlated by JSON-RPC `id`, with the measured
  round-trip latency stored alongside the response.
- Notifications (no `id`) are recorded on their own.
- Large payloads (≥ 256 KiB) are spilled to the local blob store instead of
  bloating the events table; the wire traffic itself is always forwarded
  byte-for-byte regardless of size.

**Standalone vs. attached:** run `hh mcp-proxy -- <server>` on its own and it
creates its own `mcp-only` session you can `hh list`/`hh replay` independently.
Run it as a child of `hh run` (for example, because your agent spawns the MCP
proxy itself while `hh run -- claude` is recording it) and it automatically
attaches to that parent session instead — via the `HH_SESSION_ID` environment
variable `hh run` sets for its child process tree — so the MCP traffic shows
up interleaved with the rest of that session's timeline rather than as a
separate recording.

## Current status

`hh run` (PTY + file-change recording, with structured capture for detected
adapters), `hh list`, and `hh mcp-proxy` are implemented. `hh replay`, `hh
inspect`, and `hh delete` are still on the roadmap and currently return a
"not implemented" message.
