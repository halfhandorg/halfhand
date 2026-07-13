# `hh search` — Full-text search across recorded sessions

`hh search <query>` uses SQLite FTS5 to search event summaries and message bodies across all recorded sessions. Results show the session, step, event kind, and a highlighted snippet.

## Usage

```bash
# Search for a word
hh search "error"

# Search for a phrase
hh search "create file"

# Filter by event kind
hh search "error" --kind error

# Filter by agent kind
hh search "Bash" --agent claude-code

# Filter by session start time (unix-ms)
hh search "api key" --since 1700000000000

# Filter by path
hh search "config" --path /home/user/project

# JSON output
hh search "tool_call" --json | jq

# Limit results
hh search "test" --limit 10
```

## Query Syntax

The search uses FTS5 query syntax:

- **Words**: `error` — matches any event containing "error"
- **Phrases**: `"create file"` — matches the exact phrase
- **Prefix**: `creat*` — matches "create", "creating", "created"
- **AND/OR/NOT**: `error AND warning`, `error OR warning`, `error NOT warning`
- **Column search**: `summary:error` — search only in summaries
- **Grouping**: `(error OR warning) AND "file not found"`

## Filters

| Flag | Description |
|------|-------------|
| `--agent <kind>` | Filter by agent kind (`claude-code`, `claude-desktop`, `codex-cli`, `gemini-cli`, `generic`, `mcp-only`) |
| `--kind <kind>` | Filter by event kind (`user_message`, `agent_message`, `tool_call`, etc.) |
| `--since <ms>` | Only sessions started after this unix-ms timestamp |
| `--path <path>` | Only sessions whose cwd contains this path fragment |
| `--limit <N>` | Maximum results (default 50, max 500) |
| `--json` | Emit JSON instead of a table |

## JSON Output

```json
[
  {
    "event_id": 42,
    "session_id": "019c95c9-00ce-7aa3-b767-0ff0551a85d5",
    "session_short_id": "a1b2c3",
    "step": 7,
    "kind": "tool_call",
    "summary": "tool_call: Bash",
    "snippet": "...<b>Bash</b> command to list...",
    "ts_ms": 5000
  }
]
```

## How It Works

The FTS5 index is built incrementally at record time by the writer thread. Every event's summary and body text are indexed as the event is written. This means:

- **No first-search delay** — the index is always up to date
- **No background jobs** — indexing happens during recording
- **Correct on delete** — deleting a session removes its events from the index
- **Correct on redact** — redacting a session updates the index with the redacted text

The index uses the `porter unicode61` tokenizer, which provides English stemming (so "create" matches "creating" and "created") and Unicode-aware tokenization.

## Performance

Search latency is typically <10 ms for a 100-session, 100k-event store. The index is bounded by the total number of events, so search time scales linearly with the number of matching results, not the total event count.
