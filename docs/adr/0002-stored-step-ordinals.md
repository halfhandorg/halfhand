# ADR-0002: Store step ordinals at finalize (not derive them at read time)

- **Status:** Accepted
- **Date:** 2026-07-06
- **Deciders:** Halfhand engineering
- **SRS context:** FR-3.4 (step assignment: 1-based ordinals, `terminal_output`
  excluded, a `tool_result` shares its `tool_call`'s step). ADR-0001 states the
  project's general "derived at read time" stance for view-layer values; this
  ADR records a deliberate exception for `events.step`.

## Context

FR-3.4 requires every step-bearing event to carry a 1-based ordinal, with
`tool_result` sharing its correlated `tool_call`'s ordinal. Two designs were
available:

- **A. Derive at read time.** `hh replay`/`hh inspect`/`hh list` compute step
  numbers on the fly from `(ts_ms, id, correlates)` whenever a session is read.
  `events.step` would not exist as a stored column.
- **B. Store at finalize.** Run the FR-3.4 pass once, when a session's writer
  has flushed and closed, and persist the result in `events.step` (already
  present in the schema). Self-heal on `Store::open` for the sessions that
  never got a chance to finalize cleanly.

## Decision

We choose **B: store the ordinals**, computed by a pure pass
(`hh_core::step::assign_steps`) over `EventRow`s and written back in one
transaction via `Store::assign_steps`.

## Justification

1. **Session-row counts need `step` as a column.** `hh list`/`hh inspect`
   report a step count and a files-changed count per session without replaying
   the whole event stream. `COUNT(DISTINCT step) WHERE step IS NOT NULL` is a
   single indexed aggregate; deriving it at read time would mean re-running the
   FR-3.4 pass (a full table scan + correlation resolution) just to print a
   number in `hh list`.
2. **The correlation pass is O(n) but not free, and it is reused.** `hh
   replay`, `hh inspect`, and `hh list` all want step numbers; recomputing the
   same pass in three call sites — or a shared read-time helper re-run on every
   invocation — costs more than computing it once and writing it down.
3. **Idempotent by construction.** The pass is pure
   (`assign_steps_pass(&mut [EventRow])`) and deterministic given
   `(ts_ms, id, correlates, kind)`; re-running it (e.g. on self-heal) always
   converges to the same assignment, so "store vs. derive" is not a
   correctness trade — it is purely a caching decision with no staleness risk
   as long as every code path that appends events also re-runs the pass before
   the session is considered readable.
4. **Crash / cross-process races are bounded, not eliminated, by design.**
   `hh run` runs the pass at finalize (writer flushed + joined, no intra-process
   contention). A crashed `hh run` leaves `step IS NULL` on unfinalized
   sessions; `Store::open` self-heals by re-running the pass for any session
   with a NULL-stepped semantic event before the caller can read further
   (`heal_steps`, called after `mark_stale_interrupted`). This mirrors the
   already-accepted FR-1.7 "reconcile on next open" pattern.

## Consequences

- `events.step` is authoritative once set; nothing derives it independently.
  `Store::list_sessions`/`session_step_count` compute
  `COUNT(DISTINCT step) WHERE step IS NOT NULL`, which is correct under the
  FR-3.4 sharing rule (a `tool_call`+`tool_result` pair is one step, one
  distinct value) — the prior `COUNT(*) WHERE kind != 'terminal_output'` query
  over-counted shared steps and has been replaced.
- **Attached `hh mcp-proxy` late events are a known v0.1 limitation.** The
  proxy runs `assign_steps` best-effort on its own attached session at exit
  (so MCP events usually get ordinals immediately), but if the parent `hh run`
  finalizes concurrently and the proxy's write loses the race, the affected
  events keep `step = NULL` until the next `hh` invocation's self-heal. No data
  is lost — only the ordinal is deferred. This is a cross-process race the
  child proxy process cannot join against; documented in `docs/mcp-proxy.md`
  and the plan's deviations list rather than solved with cross-process locking
  (out of proportion for a display-ordinal race).
- `Store::open` does one extra `SELECT DISTINCT session_id` probe on every
  invocation (`heal_steps`); this is a cheap, usually-empty scan and is the
  price of the self-heal guarantee.

## SRS deviation flagged

ADR-0001 (and the general engineering-standards framing) leans toward "derive
at read time" for view-layer values not backed by a requirement to persist
them. `events.step` is schema-backed (SRS §4.1 already has the column) and
FR-3.4 does not mandate either storage strategy, so this is a deviation from
the project's general read-time-derivation instinct, not from an explicit SRS
requirement — recorded here per CLAUDE.md's "flag deviations explicitly."
