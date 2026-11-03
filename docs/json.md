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

The diagnostic/lifecycle commands also emit `schema:2` objects; they are
documented on their own pages and linked from [Other JSON outputs](#other-json-outputs).

Consumers should gate on `schema` before reading any other field. Unknown
fields — and unknown values within a documented field, such as a new
`agent_kind` — must be ignored; we never silently rename or remove a
documented field within a version.

## Versioning

`schema` is a single integer shared by every JSON-emitting command (session
objects, event/step objects, and the `doctor`/`gc`/`stats`/`scan`/`export`
objects on their own pages). The current version is **2**, frozen for the
1.0 series ([`STABILITY.md`](../STABILITY.md)):

- **Additive changes — a new field, a new enum value, a new `body` shape —
  do not bump `schema` further.** They are documented here (and on the
  relevant page) as they land, and the field they extend is always present.
  This is a change from the pre-1.0 (`0.1.x-beta`) policy, under which every
  additive change bumped the integer.
- A change that removes or renames a field, or changes a field's type, is
  breaking and is never made within `schema:2`. It ships as a new `schema`
  value with both shapes documented side by side, gated behind a major
  version of `hh` per `STABILITY.md`.

### Diff from schema 1

`schema:2` folds in additive drift from the `0.1.x-beta` series that never
got its own version bump, so `schema:1` output is missing:

- Three `agent_kind` values: `claude-desktop`, `codex-cli`, `gemini-cli`
  (only `claude-code`, `generic`, `mcp-only` existed under `schema:1`).
- The `lifecycle` event's `redaction_audit` body shape (produced by `hh
  redact`; see [`body` by kind](#body-by-kind) below).

No field was removed, renamed, or retyped — every `schema:1` consumer that
already ignores unknown fields/values reads `schema:2` output unchanged.

---

## Session object (`hh list --json`)

One object per recorded session, newest-first.

| Field | Type | Always present? | Description |
|---|---|---|---|
| `schema` | integer | yes | `2`. |
| `id` | string | yes | Full UUIDv7 session id. |
| `short_id` | string | yes | 6-hex-char short id (the random tail of the UUIDv7; see `NewSession::short_id`). |
| `status` | string | yes | One of `recording`, `ok`, `error`, `interrupted`. |
| `agent_kind` | string | yes | One of `claude-code`, `claude-desktop`, `codex-cli`, `gemini-cli`, `generic`, `mcp-only`. New values are additive (added in `schema:2`: `claude-desktop`, `codex-cli`, `gemini-cli` — see [Diff from schema 1](#diff-from-schema-1)); consumers must ignore a value they don't recognize rather than error. |
| `adapter_status` | string | yes | One of `none`, `active`, `degraded`. `active` while a structured adapter tailed events; `degraded` if the adapter failed but PTY/FS recording continued; `none` for a generic PTY-only session. |
| `adapter_degrade_reason` | string \| null | yes | When `adapter_status` is `degraded`, a machine-readable code explaining why (`jsonl_not_found`, `jsonl_parse_error`, `jsonl_schema_drift`, `discovery_ambiguous`, `permission_denied`, `io_error`). `null` for `active`/`none` sessions and for degraded sessions whose tailer panicked (no structured error). Added additively in v1.1.0. |
| `dropped_events` | integer | yes | Number of events the recorder failed to write to the database (disk full, I/O error, …). Non-zero means the session is **degraded**: its timeline has gaps, and `hh list` shows `⚠` after its status. `0` for a clean session and for sessions recorded before this field existed. Added additively. See [Recording → Dropped events](recording.md#dropped-events-and-the--marker). |
| `dropped_events_reason` | string \| null | yes | The first write failure's cause (what failed and why), human-readable and not a stable code. `null` when `dropped_events` is `0`. Added additively. |
| `started_at` | integer | yes | Session start, unix-ms UTC. |
| `ended_at` | integer \| null | yes | Session end, unix-ms UTC. `null` while recording or if the session was interrupted before finalizing. |
| `exit_code` | integer \| null | yes | The wrapped process's exit code, if finalized. `null` while recording or interrupted. |
| `duration_ms` | integer \| null | yes | `ended_at - started_at`, clamped non-negative. `null` when `ended_at` is `null`. |
| `steps` | integer | yes | Count of distinct stored step ordinals (semantic events). `terminal_output` events are not steps. |
| `files_changed` | integer | yes | Count of distinct file paths in the session's `file_changes` rows. |
| `command` | array of strings | yes | The recorded command line as an argv vector. Empty (`[]`) for a [`hh watch`](./watch.md) session, which wraps no command. |
| `cwd` | string | yes | The session's working directory. |
| `imported_from` | string \| null | yes | The original session id, if this session was created by `hh import` (see [`docs/export-import.md`](export-import.md)). `null` for a locally-recorded session. Added additively — older clients that don't read it are unaffected. |
| `name` | string \| null | yes | The session's name (`hh run --name`, `hh rename`), or `null`. Added additively (migration 0007). |
| `model_id` | string \| null | yes | The last model id the transcript self-reported. `null` when it never reported one (any non-adapter session). Added additively (migration 0007). |
| `model_display` | string \| null | yes | Display name of `model_id` from Halfhand's compiled-in model table; `null` when the model is not in the table. Added additively. |
| `provider` | string \| null | yes | Provider of `model_id` from the table, or `null`. Added additively. |
| `context_limit` | integer \| null | yes | Context window of `model_id` in tokens, from the table, or `null`. Added additively. |
| `capture_tier` | string \| null | yes | `transcript` (a structured adapter tailed the transcript), `terminal` (PTY and file system only), `mcp` (MCP proxy) or `effects` (file changes only: a [`hh watch`](./watch.md) session, set when it starts). Otherwise `null` until finalized. Added additively. |
| `adapter_id` | string \| null | yes | The adapter that ran (`claude-code`, `claude-desktop`, …), or `null`. Added additively. |
| `usage` | object | yes | Self-reported token usage, accumulated over the session: `{ "source": "self_reported", "input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "reasoning_tokens", "total_tokens" }`. Each count is an integer or `null` — `null` means not reported, never 0. `total_tokens` is derived (input + output + cache read + cache write; reasoning is a breakdown of output) and never stored. Size-based estimates are never in this object; see [Session context](./context.md). Added additively. |
| `duration` | string \| null | yes | `duration_ms` as an ISO-8601 duration (`PT1H49M40S`). Added additively. |
| `active_duration_ms` | integer \| null | yes | First to last agent event (any event except `terminal_output` and `lifecycle`). `null` when there are none. Added additively. |
| `active_duration` | string \| null | yes | `active_duration_ms` as an ISO-8601 duration. Added additively. |

---

## Event object (`hh inspect --json`)

The unit of the NDJSON stream (`hh inspect --json`) and of each entry in a
step object's `events` array (`hh inspect --json --step N`).

| Field | Type | Always present? | Description |
|---|---|---|---|
| `schema` | integer | yes | `2`. |
| `session` | object | yes | `{ "id": <full uuid>, "short_id": <6 hex>, "adapter_status": <string>, "adapter_degrade_reason": <string\|null> }`. The `adapter_status`/`adapter_degrade_reason` fields (v1.1.0) let a consumer detect a degraded session from any event's NDJSON line without a separate `hh list` call. |
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
| `file_change` | `{ "path", "change_kind", "before_source" }` — the structured change is in `file_change`; `before_source` (one of `live`, `git`, `baseline`, `none`; absent on sessions recorded before it was introduced) says where the before-side came from (see [Recording](./recording.md#file-changes-where-the-before-side-comes-from)). A rename adds `"renamed_from": "<old path>"`. |
| `lifecycle` | `null`; `{ "event": "start" | "exit" }`; or (added in `schema:2`, produced by `hh redact`) `{ "redaction_audit": { "secrets": [{ "type", "hash8", "count" }], "events_rewritten", "blobs_rewritten" } }` — the session self-documenting what a redaction removed, never the secret material itself. |

Response metadata (added additively, migration 0007): the first event an
adapter emits for a model response — usually its `agent_message`, `thinking`
or first `tool_call` — also carries `"model"` (the self-reported model id),
`"message_id"` (the response id) and `"usage"` (the response's usage object,
verbatim). A transcript writes one response's usage on each of its content
blocks; Halfhand counts a `message_id` once. Today the Claude Code and Claude
Desktop adapters emit these keys.

Future adapter bodies are additive: new keys may appear within `body` without
bumping `schema`.

### `file_change` object

| Field | Type | Description |
|---|---|---|
| `path` | string | Path relative to the session's `cwd`. |
| `change_kind` | string | One of `created`, `modified`, `deleted`. |
| `before_hash` | string \| null | BLAKE3 hex of the pre-change content blob, if captured. For the first change to a file that existed before the session, this is its session-start content (from the git index for a clean file, else captured at start). `null` for a created file, or when the pre-change content was not captured (over `max_file_size` or `baseline_budget`, or binary with `record_binary` off). For a rename (`body.renamed_from` set; `change_kind` is `modified`) it is the old path's content. |
| `after_hash` | string \| null | BLAKE3 hex of the post-change content blob, if captured. `null` for a deleted file. |
| `is_binary` | boolean | Whether the file was detected as binary. |

---

## Step object (`hh inspect --json --step N`)

A single JSON object describing one step.

| Field | Type | Description |
|---|---|---|
| `schema` | integer | `2`. |
| `session` | object | `{ "id", "short_id" }` as in the event object. |
| `step` | integer | The 1-based step ordinal. |
| `kind` | string | The step's badge kind — the "opening" side of a correlated pair (a `tool_call` over its `tool_result`; an `mcp_request` over its `mcp_response`). |
| `summary` | string | The primary event's one-line summary. |
| `ts_ms` | integer | The earliest `ts_ms` among the step's events. |
| `elapsed_ms` | integer \| null | Time from this step's `ts_ms` to the next step's (the last step runs to the session's last agent event). `null` when the end is unknown. Added additively. |
| `elapsed` | string \| null | `elapsed_ms` as an ISO-8601 duration. Added additively. |
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
| `hh inspect --context --json` | `{ schema, session, name, model: {…}, context_used: {…}, usage: {…}, estimated: {…}, messages: {…}, …, estimated_cost_usd, models: […] }` | [`docs/context.md`](context.md) |
| `hh doctor --json` | `{ schema, status, checks: [{ name, status, detail }], init: { initialized, would_create: [{ label, path }] } }` | [`docs/doctor.md`](doctor.md) |
| `hh doctor --capture --json` | the `hh doctor --json` object plus `capture: { session, watched_roots, events_live, events_reconciled, file_changes, dropped_events, dropped_events_reason, permission_denied, before_side_coverage, platform }`; unknown facts are `null` | [`docs/capture.md`](capture.md) |
| `hh gc --json` | `{ schema, orphan_files_removed, orphan_bytes_reclaimed, orphan_rows_removed, vacuumed }` | [`docs/gc.md`](gc.md) |
| `hh stats --json` | `{ schema, sessions, events, blobs: {…}, disk: {…}, largest_sessions: [{ id, short_id, events }] }` | [`docs/stats.md`](stats.md) |
| `hh scan --json` | `{ schema, total_findings, sessions: [{ id, short_id, findings: [{ type, hash8, count, event_id, step, event_kind, location }] }] }` — findings never contain the secret; `hash8` correlates one secret across rows | [`docs/redaction.md`](redaction.md) |
| `hh export` | `{ schema, kind: "hh-export", hh_version, session, events }` — `session` is the `hh list --json` session object; `events` are event objects with resolved bodies; redacted by default | [`docs/redaction.md`](redaction.md) |
| `hh export --bundle` | a zstd-compressed tar (`manifest.json` + `events.ndjson` + `blobs/`), not a single JSON object — see [`docs/export-import.md`](export-import.md) for the bundle's own schema | [`docs/export-import.md`](export-import.md) |