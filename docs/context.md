# Session context

`hh inspect --context` answers four questions about a recorded session:
which model ran, through which provider, for how long, and at what token
cost.

```bash
hh inspect last --context          # the panel
hh inspect a1b2c3 --context --json # the same data as one JSON object
```

```text
 a1b2c3 · context

 name            auth refactor
 model           Claude Opus 4.8 (claude-opus-4-8)
 provider        Anthropic
 context limit   1,000,000 tokens
 context used    51,200 tokens · 5.1% of limit · latest turn, self-reported
 capture         transcript · claude-code

 tokens          self-reported, accumulated
   input           2,400
   output            680
   cache read     96,000
   cache write     4,000
   reasoning           —
   total         103,080  derived
 estimated       ≈ 140 tokens · size-based (bytes ÷ 4), not self-reported

 messages        1 user · 3 agent · 1 tool call
 created         2023-11-14 22:13:20 UTC
 last activity   2023-11-14 22:13:23 UTC
 duration        3s wall · 3s active
 est. cost       $0.10 at list price
 models          claude-opus-4-8 (steps 5–6)
```

The panel uses one accent colour and aligned columns. It honours `NO_COLOR`
and prints plain text when stdout is not a terminal.

## Where each value comes from

| Row | Source |
|---|---|
| name | `hh run --name`, or `hh rename` afterwards. |
| model | The `model` field the agent's own transcript reports on each response. If the model changes mid-session, this row shows the **last** one and the `models` row lists every model with its step range. |
| provider, context limit | A table compiled into `hh`. Halfhand makes no network calls, so nothing is looked up at run time. |
| context used | The latest response's prompt size as the transcript reported it (input + cache read + cache write), and its share of the context limit. |
| tokens | The transcript's own `usage` objects, summed over the session as events are recorded. |
| total | Derived when you read it — never stored, so it cannot drift from its parts. Input + output + cache read + cache write. Reasoning tokens are a breakdown of output and are not added again. |
| estimated | Recorded payload bytes ÷ 4. See below. |
| messages | Counts of `user_message`, `agent_message` and `tool_call` events. |
| created, last activity | Session start, and the timestamp of its last recorded event. |
| duration | **Wall** is start to end. **Active** is the first to the last agent event (any event except terminal output and lifecycle markers), so idle time before the first prompt and after the last answer is excluded. |
| est. cost | Every response priced with the model that wrote it, at the table's list price. |
| capture | `transcript` when an adapter tailed the agent's transcript, `terminal` for PTY and file-system capture only, `mcp` for MCP proxy traffic; then the adapter that ran. |

## Two token families, never blended

Halfhand reports two kinds of token number. They answer different questions
and are never added together or put in the same field.

- **Self-reported** counts are what the agent's transcript says it used.
  They are exact for that agent's accounting. Only sessions recorded with a
  structured adapter have them (today: Claude Code and Claude Desktop).
  In JSON they live under `usage`, which carries `"source": "self_reported"`.
- **Estimated** counts come from the size of what Halfhand recorded (bytes
  ÷ 4). They are rough, they exist for every session, and they are always
  labelled as an estimate. In JSON they live under `estimated`, with
  `"source": "size_based"` and `"method": "bytes_div_4"`.

A session with no self-reported usage shows dashes in the token rows and
still shows its estimate. Halfhand never substitutes one family for the
other.

## The dash convention

A value Halfhand does not know prints as `—` in the terminal and `null` in
JSON. **It never prints as 0.** Zero asserts a known value; a dash admits
ignorance. In particular:

- A model that is not in the compiled-in table shows its id with "(not in
  the model table)", and provider, context limit, usage percentage and cost
  are dashes.
- A count the transcript never reported (Claude does not report reasoning
  tokens separately) is a dash, not 0.
- If any response in a session came from a model the table does not know,
  the cost is a dash. A partial sum would understate the cost, so the
  total is unknown rather than wrong.
- A session recorded before Halfhand tracked usage (migration 0007) keeps
  its model, but its token counts are dashes. The usage it stored back then
  covers only the last response, not the whole session.

## The model table

The table lists the Claude models whose provider, context window and list
prices are known when `hh` is built. Prices are first-party API list prices
per token, and cache writes use the 5-minute rate. They are not your invoice:
batch, partner-platform and negotiated pricing differ. A model the table
lacks is not an error. It gets dashes until a future `hh` release adds it.

## JSON

`hh inspect --context --json` prints one object:

| Field | Type | Description |
|---|---|---|
| `schema` | integer | `2`. |
| `session` | object | `{ id, short_id, adapter_status, adapter_degrade_reason }`, as on event objects. |
| `name` | string \| null | The session name. |
| `model` | object | `{ id, display, provider, context_limit }`. Each is `null` when unknown. |
| `context_used` | object | `{ source: "self_reported_latest_turn", tokens, percent_of_limit }`. `percent_of_limit` is a number with one decimal place, or `null`. |
| `usage` | object | As on the [session object](./json.md#session-object-hh-list---json): self-reported counts and the derived `total_tokens`. |
| `estimated` | object | `{ source: "size_based", method: "bytes_div_4", tokens }`. |
| `messages` | object | `{ user, agent, tool_calls }`. |
| `capture_tier`, `adapter_id` | string \| null | As on the session object. |
| `created_at`, `created` | integer, string | Session start: unix ms, and RFC 3339 UTC. |
| `last_activity_at`, `last_activity` | integer \| null, string \| null | Last recorded event: unix ms, and RFC 3339 UTC. |
| `duration_ms`, `duration` | integer \| null, string \| null | Wall-clock duration: ms, and ISO 8601 (`PT1H49M40S`). |
| `active_duration_ms`, `active_duration` | integer \| null, string \| null | First to last agent event: ms, and ISO 8601. |
| `estimated_cost_usd` | string \| null | Decimal US dollars with six places (`"0.102000"`). A string, so no precision is lost or invented. `null` when unknown. |
| `cost_basis` | string | `"list_price"`. |
| `models` | array | `[{ model_id, first_step, last_step }]`, in order. |

The same name, model, usage and duration fields are on every `hh list --json`
session object. See the [JSON reference](./json.md).
