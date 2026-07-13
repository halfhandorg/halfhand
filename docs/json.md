# Halfhand JSON schema

`hh` emits stable, documented JSON for machine consumption. There are two
core shapes: a **session object** (one row in `hh list --json`) and an
**event object** (the unit of `hh inspect --json`). Every object carries a
`schema` field naming the version of this document it conforms to.

| Command | Shape | Form |
|---|---|---|
| `hh list --json` | array of session objects | one JSON array |
| `hh inspect --json` | event objects, one per event, in `(ts_ms, id)` order | NDJSON — one object per line |
| `hh inspect --json --step N` | a single step object | one JSON object |

The diagnostic/lifecycle commands also emit `schema:1` objects; they are
documented on their own pages and linked from [Other JSON outputs](#other-json-outputs).

Consumers should gate on `schema` before reading any other field. Unknown
fields must be ignored (additive changes bump `schema`; we never silently
rename or remove a documented field within a version).

## Versioning

`schema` is a monotonically increasing integer. The current version is **1**.
A change that adds a field bumps `schema` and is documented here. A change
that removes or renames a field, or changes a field's type, is a breaking
change and is *not* done within a version — it ships as a new `schema` value
with both shapes documented.

---

## Session object (`hh list --json`)

One object per recorded session, newest-first.

| Field | Type | Always present? | Description |
|---|---|---|---|
| `schema` | integer | yes | `1`. |
| `id` | string | yes | Full UUIDv7 session id. |
| `short_id` | string | yes | 6-hex-char short id (the random tail of the UUIDv7; see `NewSession::short_id`). |
| `status` | string | yes | One of `recording`, `ok`, `error`, `interrupted`. |
| `agent_kind` | string | yes | One of `claude-code`, `generic`, `mcp-only`. |
| `adapter_status` | string | yes | One of `none`, `active`, `degraded`. `active` while a structured adapter tailed events; `degraded` if the adapter failed but PTY/FS recording continued; `none` for a generic PTY-only session. |
| `started_at` | integer | yes | Session start, unix-ms UTC. |
| `ended_at` | integer \| null | yes | Session end, unix-ms UTC. `null` while recording or if the session was interrupted before finalizing. |
| `exit_code` | integer \| null | yes | The wrapped process's exit code, if finalized. `null` while recording or interrupted. |
| `duration_ms` | integer \| null | yes | `ended_at - started_at`, clamped non-negative. `null` when `ended_at` is `null`. |
| `steps` | integer | yes | Count of distinct stored step ordinals (semantic events). `terminal_output` events are not steps. |
| `files_changed` | integer | yes | Count of distinct file paths in the session's `file_changes` rows. |
| `command` | array of strings | yes | The recorded command line as an argv vector. |
| `cwd` | string | yes | The session's working directory. |

---

## Event object (`hh inspect --json`)

The unit of the NDJSON stream (`hh inspect --json`) and of each entry in a
step object's `events` array (`hh inspect --json --step N`).

| Field | Type | Always present? | Description |
|---|---|---|---|
| `schema` | integer | yes | `1`. |
| `session` | object | yes | `{ "id": <full uuid>, "short_id": <6 hex> }`. |
| `id` | integer | yes | The `events.id` row id (rowid PK). Stable within a session; used by `correlates`. |
| `ts_ms` | integer | yes | Milliseconds since the session's `started_at`. |
| `kind` | string | yes | One of `lifecycle`, `user_message`, `agent_message`, `thinking`, `tool_call`, `tool_result`, `mcp_request`, `mcp_response`, `mcp_notification`, `file_change`, `terminal_output`, `error`. |
| `step` | integer \| null | yes | 1-based step ordinal, or `null` for non-step events (`terminal_output`, and `lifecycle` markers that never received a step). |
| `correlates` | integer \| null | yes | The `id` of the event this one is paired with (a `tool_result` → its `tool_call`; an `mcp_response` → its `mcp_request`), or `null`. |
| `summary` | string | yes | One-line summary (≤ 120 chars). |
| `body` | object \| null | yes | The kind-specific structured payload, with internal-only keys (currently `correlate_key`) stripped. `null` if the event had no body. Large bodies that overflowed inline storage are resolved here — the `{"overflow": true, ...}` envelope is replaced by the real, blob-resolved content, so consumers never see the envelope. |
| `file_change` | object \| null | yes | Present (non-null) only when `kind == "file_change"`. |

### `body` by kind

The `body` field is free-form JSON whose shape depends on `kind`. The shapes
the built-in adapters emit today:

| `kind` | `body` shape |
|---|---|
| `user_message` (text prompt) | `{ "text": "…" }` |
| `agent_message` | `{ "text": "…" }` |
| `thinking` | `{ "text": "…" }` |
| `terminal_output` | `{ "text": "…" }` (UTF-8 chunk); binary chunks are stored as a blob and `body` carries the resolved bytes' lossy-UTF-8 text. |
| `tool_call` | `{ "name": "Bash", "input": { … } }` |
| `tool_result` | `{ "content": "…", "is_error": false }` (`is_error` is `true` for a failed tool call) |
| `mcp_request` | the forwarded JSON-RPC request object, plus `{ "latency_ms": … }` on the paired response |
| `mcp_response` | the forwarded JSON-RPC response object, plus `{ "latency_ms": <int> }` |
| `mcp_notification` | the forwarded JSON-RPC notification object |
| `error` | `{ "message": "…" }` |
| `file_change` | `null` (the structured change is in `file_change`) |
| `lifecycle` | `null`, or `{ "event": "start" | "exit" }` |

Future adapter bodies are additive: new keys may appear within `body` without
bumping `schema`.

### `file_change` object

| Field | Type | Description |
|---|---|---|
| `path` | string | Path relative to the session's `cwd`. |
| `change_kind` | string | One of `created`, `modified`, `deleted`. |
| `before_hash` | string \| null | BLAKE3 hex of the pre-change content blob, if captured. `null` for a created file, or when the pre-change content exceeded `max_file_size` and was not stored. |
| `after_hash` | string \| null | BLAKE3 hex of the post-change content blob, if captured. `null` for a deleted file. |
| `is_binary` | boolean | Whether the file was detected as binary. |

---

## Step object (`hh inspect --json --step N`)

A single JSON object describing one step.

| Field | Type | Description |
|---|---|---|
| `schema` | integer | `1`. |
| `session` | object | `{ "id", "short_id" }` as in the event object. |
| `step` | integer | The 1-based step ordinal. |
| `kind` | string | The step's badge kind — the "opening" side of a correlated pair (a `tool_call` over its `tool_result`; an `mcp_request` over its `mcp_response`). |
| `summary` | string | The primary event's one-line summary. |
| `ts_ms` | integer | The earliest `ts_ms` among the step's events. |
| `events` | array of event objects | The events sharing this step, in `id` order. Usually one entry; two for a correlated call+result or request+response pair. |

---

## Resolving blob content

`body` is always returned already-resolved: if an event's payload overflowed
inline storage, `hh inspect --json` fetches and decompresses the blob and
places the real content in `body`, so consumers never handle the
`{"overflow": true, ...}` envelope. To fetch raw file-change content
yourself, read the blob at `blobs/<hash[0..2]>/<hash>.zst` (zstd-compressed,
BLAKE3-keyed) using `before_hash` / `after_hash`.

---

## Other JSON outputs

These commands emit their own `schema:1` objects (not session/event objects).
They are documented in full on their own pages:

| Command | Shape | Docs |
|---|---|---|
| `hh doctor --json` | `{ schema, status, checks: [{ name, status, detail }] }` | [`docs/doctor.md`](doctor.md) |
| `hh gc --json` | `{ schema, orphan_files_removed, orphan_bytes_reclaimed, orphan_rows_removed, vacuumed }` | [`docs/gc.md`](gc.md) |
| `hh stats --json` | `{ schema, sessions, events, blobs: {…}, disk: {…}, largest_sessions: [{ id, short_id, events }] }` | [`docs/stats.md`](stats.md) |
| `hh scan --json` | `{ schema, total_findings, sessions: [{ id, short_id, findings: [{ type, hash8, count, event_id, step, event_kind, location }] }] }` — findings never contain the secret; `hash8` correlates one secret across rows | [`docs/redaction.md`](redaction.md) |
| `hh export` | `{ schema, kind: "hh-export", hh_version, session, events }` — `session` is the `hh list --json` session object; `events` are event objects with resolved bodies; redacted by default | [`docs/redaction.md`](redaction.md) |