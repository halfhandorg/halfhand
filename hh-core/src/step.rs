//! Step-assignment pass (FR-3.4): 1-based step ordinals for semantic events.
//!
//! The pass is a pure function over a slice of [`EventRow`]s: it assigns each
//! event's `step` in place. The store reads rows via
//! [`crate::store::Store::list_events`], runs this pass, and writes the
//! ordinals back in one transaction
//! ([`crate::store::Store::assign_steps`]).
//!
//! ## Rules (FR-3.4)
//! - `terminal_output` events are not steps → `step = None`.
//! - Every other semantic event gets its own incrementing 1-based step, *except*
//!   a `tool_result` whose `correlates` points to a `tool_call` present in the
//!   session: that result shares the call's step (a call and its result are one
//!   step).
//! - A `tool_result` with `correlates = None`, or pointing to an id absent from
//!   the session (orphan / concurrent result-before-call from another source),
//!   gets its own step.
//!
//! The pass is **order-independent for correlation**: it builds an id→step map
//! of the calls in pass 1 and resolves deferred results against it in pass 2,
//! so a result that sorts before its call (e.g. a tool_result block emitted
//! ahead of the tool_use block in the same record, or concurrent sources) still
//! resolves to the call's step.
//!
//! ## Deviation (flagged, ADR-0002)
//!
//! ADR-0001 said step ordinals are "derived at read time". Storing them at
//! finalize (authoritative `events.step` column) + self-healing on
//! [`crate::store::Store::open`] is a deliberate deviation recorded in
//! `docs/adr/0002-stored-step-ordinals.md`: it makes `hh list`'s step count a
//! trivial `COUNT(DISTINCT step)`, and self-heal repairs both a crashed
//! finalize and the attached-MCP-proxy late-event race on the next `hh`
//! invocation.

use crate::event::{EventKind, EventRow};
use std::collections::{HashMap, HashSet};

/// Assign 1-based step ordinals to `events` in place (FR-3.4). See the module
/// docs for the rules. Idempotent: re-running on already-assigned rows yields
/// the same ordinals.
pub fn assign_steps(events: &mut [EventRow]) {
    // Order by (ts_ms, id) so ordinals are stable and chronological. `id` is
    // the rowid PK, so (ts_ms, id) is a strict total order (no ties).
    events.sort_by_key(|e| (e.ts_ms, e.id));

    // Upfront knowledge of which ids are present and which are tool calls, so a
    // result can decide to defer even when its call sorts later in the order.
    let mut present: HashSet<i64> = HashSet::with_capacity(events.len());
    let mut is_call: HashSet<i64> = HashSet::new();
    for e in events.iter() {
        present.insert(e.id);
        if e.kind == EventKind::ToolCall {
            is_call.insert(e.id);
        }
    }

    let mut call_step: HashMap<i64, i64> = HashMap::new();
    let mut deferred: Vec<usize> = Vec::new();
    let mut counter: i64 = 0;

    // Pass 1: assign steps to terminal-excluded + non-deferred events; record
    // each call's step for pass 2.
    for (i, e) in events.iter_mut().enumerate() {
        if e.kind == EventKind::TerminalOutput {
            e.step = None;
            continue;
        }
        // Defer a tool_result only if it points to a tool_call present in this
        // session; otherwise it gets its own step now (orphan / no correlation).
        let defer = e.kind == EventKind::ToolResult
            && matches!(e.correlates, Some(cid) if present.contains(&cid) && is_call.contains(&cid));
        if defer {
            deferred.push(i);
            continue;
        }
        counter += 1;
        e.step = Some(counter);
        if e.kind == EventKind::ToolCall {
            call_step.insert(e.id, counter);
        }
    }

    // Pass 2: deferred results borrow their call's step. The deferral guard
    // guarantees the call exists and is a ToolCall (which received a step in
    // pass 1), so the lookup succeeds; the let-else + fallback are defensive
    // only — no unwrap/expect, per CLAUDE.md.
    for &i in &deferred {
        let e = &mut events[i];
        let Some(cid) = e.correlates else {
            counter += 1;
            e.step = Some(counter);
            continue;
        };
        if let Some(&step) = call_step.get(&cid) {
            e.step = Some(step);
        } else {
            counter += 1;
            e.step = Some(counter);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, EventRow};

    /// Build an EventRow with the given (id, ts_ms, kind, correlates); step
    /// starts as `None` (the pass assigns it).
    fn row(id: i64, ts_ms: i64, kind: EventKind, correlates: Option<i64>) -> EventRow {
        EventRow {
            id,
            session_id: "s".into(),
            ts_ms,
            kind,
            step: None,
            correlates,
        }
    }

    fn steps(rows: &[EventRow]) -> Vec<Option<i64>> {
        rows.iter().map(|r| r.step).collect()
    }

    #[test]
    fn assign_steps_normal_call_result_share() {
        let mut rows = vec![
            row(1, 0, EventKind::ToolCall, None),
            row(2, 5, EventKind::ToolResult, Some(1)),
        ];
        assign_steps(&mut rows);
        // Call gets step 1; result shares step 1.
        assert_eq!(steps(&rows), vec![Some(1), Some(1)]);
    }

    #[test]
    fn assign_steps_result_before_call() {
        // Concurrent source: result sorts before the call by ts_ms.
        let mut rows = vec![
            row(2, 0, EventKind::ToolResult, Some(1)),
            row(1, 10, EventKind::ToolCall, None),
        ];
        assign_steps(&mut rows);
        // After sort by (ts_ms, id): result(id=2,ts=0) then call(id=1,ts=10).
        // The result defers (correlates → a present call); pass 2 gives it the
        // call's step. The call is the first step assigned → step 1; result → 1.
        assert_eq!(steps(&rows), vec![Some(1), Some(1)]);
    }

    #[test]
    fn assign_steps_orphan_result_own_step() {
        // Result points to a call id not present in the session → orphan, own step.
        let mut rows = vec![
            row(1, 0, EventKind::UserMessage, None),
            row(2, 5, EventKind::ToolResult, Some(99)),
        ];
        assign_steps(&mut rows);
        // UserMessage → step 1; orphan result → step 2 (not deferred).
        assert_eq!(steps(&rows), vec![Some(1), Some(2)]);
    }

    #[test]
    fn assign_steps_none_correlates_own_step() {
        let mut rows = vec![
            row(1, 0, EventKind::ToolCall, None),
            row(2, 5, EventKind::ToolResult, None),
        ];
        assign_steps(&mut rows);
        // Call → 1; result with no correlation → 2.
        assert_eq!(steps(&rows), vec![Some(1), Some(2)]);
    }

    #[test]
    fn assign_steps_terminal_output_null() {
        let mut rows = vec![
            row(1, 0, EventKind::UserMessage, None),
            row(2, 1, EventKind::TerminalOutput, None),
            row(3, 2, EventKind::AgentMessage, None),
        ];
        assign_steps(&mut rows);
        // terminal_output is not a step; the two semantic events are 1 and 2.
        assert_eq!(steps(&rows), vec![Some(1), None, Some(2)]);
    }

    #[test]
    fn assign_steps_multiple_pairs() {
        // Two independent call/result pairs → 2 distinct steps.
        let mut rows = vec![
            row(1, 0, EventKind::ToolCall, None),
            row(2, 1, EventKind::ToolResult, Some(1)),
            row(3, 10, EventKind::ToolCall, None),
            row(4, 11, EventKind::ToolResult, Some(3)),
        ];
        assign_steps(&mut rows);
        assert_eq!(steps(&rows), vec![Some(1), Some(1), Some(2), Some(2)]);
    }

    #[test]
    fn assign_steps_thinking_own_step() {
        let mut rows = vec![
            row(1, 0, EventKind::Thinking, None),
            row(2, 5, EventKind::AgentMessage, None),
        ];
        assign_steps(&mut rows);
        assert_eq!(steps(&rows), vec![Some(1), Some(2)]);
    }

    #[test]
    fn assign_steps_idempotent() {
        let mut rows = vec![
            row(1, 0, EventKind::ToolCall, None),
            row(2, 5, EventKind::ToolResult, Some(1)),
            row(3, 6, EventKind::AgentMessage, None),
        ];
        assign_steps(&mut rows);
        let first = steps(&rows);
        assign_steps(&mut rows);
        assert_eq!(steps(&rows), first, "re-running must be idempotent");
        assert_eq!(first, vec![Some(1), Some(1), Some(2)]);
    }

    #[test]
    fn assign_steps_result_pointing_at_non_call_present_id_gets_own_step() {
        // correlates points to an id that IS present but is not a ToolCall —
        // must not defer (only tool_call targets share); gets its own step.
        let mut rows = vec![
            row(1, 0, EventKind::UserMessage, None),
            row(2, 5, EventKind::ToolResult, Some(1)),
        ];
        assign_steps(&mut rows);
        assert_eq!(steps(&rows), vec![Some(1), Some(2)]);
    }
}
