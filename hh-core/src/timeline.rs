//! Pure timeline-grouping for replay/inspect (FR-3.2/FR-3.4).
//!
//! Turns a session's flat event index into display rows: one row per step
//! (folding a correlated `tool_call`/`tool_result` or `mcp_request`/
//! `mcp_response` pair into a single row), plus, when terminal segments are
//! shown, runs of consecutive `terminal_output` events collapsed into one
//! segment row. Pure and I/O-free — it operates on [`EventIndexRow`]s already
//! loaded by [`crate::store::Store::list_event_index`], so it is unit
//! testable without a database or terminal.

use crate::event::{EventIndexRow, EventKind};
use std::collections::HashMap;

/// One row in the rendered timeline: either a semantic step or a collapsed
/// run of terminal output.
#[derive(Debug, Clone, PartialEq)]
pub enum TimelineRow {
    /// A semantic step (FR-3.4): one or more correlated events sharing a step.
    Step(StepEntry),
    /// A collapsed run of consecutive `terminal_output` events.
    Terminal(TerminalSegment),
}

/// A semantic step row for the timeline pane.
#[derive(Debug, Clone, PartialEq)]
pub struct StepEntry {
    /// 1-based step ordinal (0 for the defensive no-step fallback; see
    /// [`build_timeline`]).
    pub step: i64,
    /// Earliest timestamp among the step's events (ms since session start).
    pub ts_ms: i64,
    /// The badge kind shown for this row (FR-3.2): the call/request side of a
    /// correlated pair takes priority over its result/response.
    pub kind: EventKind,
    /// One-line summary of the primary (call/request-side) event.
    pub summary: String,
    /// Every event id sharing this step, ascending, chronological by id
    /// (usually one; two for a correlated call+result / request+response pair).
    pub event_ids: Vec<i64>,
}

/// A collapsed run of `terminal_output` events.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalSegment {
    /// Timestamp of the first event in the run.
    pub start_ts_ms: i64,
    /// Timestamp of the last event in the run.
    pub end_ts_ms: i64,
    /// The `terminal_output` event ids in this run, chronological.
    pub event_ids: Vec<i64>,
}

impl TimelineRow {
    /// The timestamp used to order/locate this row (its earliest event's ts).
    #[must_use]
    pub fn ts_ms(&self) -> i64 {
        match self {
            TimelineRow::Step(s) => s.ts_ms,
            TimelineRow::Terminal(t) => t.start_ts_ms,
        }
    }

    /// The event ids backing this row.
    #[must_use]
    pub fn event_ids(&self) -> &[i64] {
        match self {
            TimelineRow::Step(s) => &s.event_ids,
            TimelineRow::Terminal(t) => &t.event_ids,
        }
    }

    /// The badge kind for this row (`terminal_output` reuses its own kind).
    #[must_use]
    pub fn kind(&self) -> EventKind {
        match self {
            TimelineRow::Step(s) => s.kind,
            TimelineRow::Terminal(_) => EventKind::TerminalOutput,
        }
    }

    /// A one-line label for this row (the step's summary, or a terminal
    /// segment's synthetic "N lines" label).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            TimelineRow::Step(s) => s.summary.clone(),
            TimelineRow::Terminal(t) => {
                let n = t.event_ids.len();
                if n == 1 {
                    "1 terminal chunk".to_string()
                } else {
                    format!("{n} terminal chunks")
                }
            }
        }
    }
}

/// Build the timeline rows from a session's full event index (FR-3.2/FR-3.4).
///
/// `show_terminal` toggles whether `terminal_output` runs appear as
/// [`TimelineRow::Terminal`] rows interleaved by timestamp (the `t` key in the
/// replay TUI); when `false` they are omitted entirely and only step rows
/// remain. Rows are always returned in ascending timestamp order.
#[must_use]
pub fn build_timeline(events: &[EventIndexRow], show_terminal: bool) -> Vec<TimelineRow> {
    // Single forward pass, appending rows in true encounter order (so a
    // terminal run is only merged with the immediately preceding row when
    // that row is itself an adjacent terminal segment — an intervening step
    // row correctly breaks the run). A step's row is created on its first
    // member and updated in place when a later-arriving correlated member
    // (e.g. a deferred tool_result) joins the same step.
    let mut rows: Vec<TimelineRow> = Vec::new();
    let mut step_index: HashMap<i64, usize> = HashMap::new();

    // Sort a local index rather than assume the caller's order: this mirrors
    // `step::assign_steps`'s own defensive sort and is what makes the
    // terminal-run adjacency check below (rows.last_mut()) correct.
    let mut ordered: Vec<&EventIndexRow> = events.iter().collect();
    ordered.sort_by_key(|e| (e.ts_ms, e.id));

    for e in ordered {
        if e.kind == EventKind::TerminalOutput {
            if show_terminal {
                push_terminal(&mut rows, e);
            }
            continue;
        }
        let Some(step) = e.step else {
            // Defensive fallback: a semantic event with no step assigned
            // should not occur post-heal (ADR-0002 self-heals this on
            // Store::open), but render it as its own row rather than
            // dropping it silently.
            rows.push(TimelineRow::Step(StepEntry {
                step: 0,
                ts_ms: e.ts_ms,
                kind: e.kind,
                summary: e.summary.clone(),
                event_ids: vec![e.id],
            }));
            continue;
        };
        if let Some(&idx) = step_index.get(&step) {
            if let TimelineRow::Step(entry) = &mut rows[idx] {
                entry.event_ids.push(e.id);
                entry.event_ids.sort_unstable();
                entry.ts_ms = entry.ts_ms.min(e.ts_ms);
                // A call/request joining after its result/response was seen
                // first (concurrent source; see step::assign_steps docs)
                // takes over as the row's badge/summary primary.
                if badge_rank(e.kind) < badge_rank(entry.kind) {
                    entry.kind = e.kind;
                    entry.summary.clone_from(&e.summary);
                }
            }
            continue;
        }
        step_index.insert(step, rows.len());
        rows.push(TimelineRow::Step(StepEntry {
            step,
            ts_ms: e.ts_ms,
            kind: e.kind,
            summary: e.summary.clone(),
            event_ids: vec![e.id],
        }));
    }

    rows
}

/// Badge priority within a correlated group: the "opening" side of a pair
/// (call/request) is the row's primary kind/summary, never its result/
/// response, regardless of which sorts first chronologically (a concurrent
/// source can emit a result before its call — see `step::assign_steps` docs).
fn badge_rank(kind: EventKind) -> u8 {
    match kind {
        EventKind::ToolResult | EventKind::McpResponse => 1,
        _ => 0,
    }
}

/// Append `e` to the last row's segment if it is a `Terminal` row (extending
/// the run), otherwise start a new segment. Called only for
/// `terminal_output` events in iteration (ts_ms, id) order, so consecutive
/// calls with no intervening step row extend one run.
fn push_terminal(rows: &mut Vec<TimelineRow>, e: &EventIndexRow) {
    if let Some(TimelineRow::Terminal(seg)) = rows.last_mut() {
        seg.end_ts_ms = e.ts_ms;
        seg.event_ids.push(e.id);
        return;
    }
    rows.push(TimelineRow::Terminal(TerminalSegment {
        start_ts_ms: e.ts_ms,
        end_ts_ms: e.ts_ms,
        event_ids: vec![e.id],
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        id: i64,
        ts_ms: i64,
        kind: EventKind,
        step: Option<i64>,
        correlates: Option<i64>,
    ) -> EventIndexRow {
        EventIndexRow {
            id,
            ts_ms,
            kind,
            step,
            correlates,
            summary: format!("{kind}#{id}"),
        }
    }

    fn ids(rows: &[TimelineRow]) -> Vec<Vec<i64>> {
        rows.iter().map(|r| r.event_ids().to_vec()).collect()
    }

    #[test]
    fn groups_call_and_result_into_one_row() {
        let events = vec![
            row(1, 0, EventKind::ToolCall, Some(1), None),
            row(2, 5, EventKind::ToolResult, Some(1), Some(1)),
        ];
        let rows = build_timeline(&events, false);
        assert_eq!(rows.len(), 1);
        let TimelineRow::Step(s) = &rows[0] else {
            panic!("expected a step row")
        };
        assert_eq!(s.event_ids, vec![1, 2]);
        assert_eq!(
            s.kind,
            EventKind::ToolCall,
            "call is the primary badge kind"
        );
        assert_eq!(s.ts_ms, 0);
    }

    #[test]
    fn out_of_order_result_still_shows_call_as_primary() {
        // Concurrent source: result (id=2) sorts before its call (id=1) by
        // ts_ms but both already carry step=1 (assigned by the store's step
        // pass before list_event_index is ever called).
        let events = vec![
            row(2, 0, EventKind::ToolResult, Some(1), Some(1)),
            row(1, 10, EventKind::ToolCall, Some(1), None),
        ];
        let rows = build_timeline(&events, false);
        assert_eq!(rows.len(), 1);
        let TimelineRow::Step(s) = &rows[0] else {
            panic!("expected a step row")
        };
        assert_eq!(
            s.kind,
            EventKind::ToolCall,
            "call must win the badge, not whichever sorts first"
        );
        assert_eq!(s.ts_ms, 0, "row ts is still the earliest member");
        assert_eq!(s.event_ids, vec![1, 2]);
    }

    #[test]
    fn mcp_request_response_pair_prefers_request_badge() {
        let events = vec![
            row(1, 0, EventKind::McpRequest, Some(1), None),
            row(2, 8, EventKind::McpResponse, Some(1), Some(1)),
        ];
        let rows = build_timeline(&events, false);
        let TimelineRow::Step(s) = &rows[0] else {
            panic!("expected a step row")
        };
        assert_eq!(s.kind, EventKind::McpRequest);
    }

    #[test]
    fn terminal_hidden_by_default() {
        let events = vec![
            row(1, 0, EventKind::UserMessage, Some(1), None),
            row(2, 5, EventKind::TerminalOutput, None, None),
            row(3, 10, EventKind::AgentMessage, Some(2), None),
        ];
        let rows = build_timeline(&events, false);
        assert_eq!(
            rows.len(),
            2,
            "terminal_output must be excluded when show_terminal=false"
        );
        assert!(rows.iter().all(|r| !matches!(r, TimelineRow::Terminal(_))));
    }

    #[test]
    fn terminal_shown_and_collapsed_into_one_segment() {
        let events = vec![
            row(1, 0, EventKind::UserMessage, Some(1), None),
            row(2, 5, EventKind::TerminalOutput, None, None),
            row(3, 6, EventKind::TerminalOutput, None, None),
            row(4, 7, EventKind::TerminalOutput, None, None),
            row(5, 10, EventKind::AgentMessage, Some(2), None),
        ];
        let rows = build_timeline(&events, true);
        assert_eq!(
            rows.len(),
            3,
            "3 consecutive terminal chunks collapse to 1 segment row"
        );
        let TimelineRow::Terminal(seg) = &rows[1] else {
            panic!("expected a terminal segment in the middle")
        };
        assert_eq!(seg.event_ids, vec![2, 3, 4]);
        assert_eq!(seg.start_ts_ms, 5);
        assert_eq!(seg.end_ts_ms, 7);
    }

    #[test]
    fn terminal_runs_split_by_an_intervening_step() {
        let events = vec![
            row(1, 0, EventKind::TerminalOutput, None, None),
            row(2, 1, EventKind::TerminalOutput, None, None),
            row(3, 2, EventKind::UserMessage, Some(1), None),
            row(4, 3, EventKind::TerminalOutput, None, None),
        ];
        let rows = build_timeline(&events, true);
        assert_eq!(ids(&rows), vec![vec![1, 2], vec![3], vec![4]]);
    }

    #[test]
    fn rows_are_ts_ordered() {
        let events = vec![
            row(1, 20, EventKind::AgentMessage, Some(2), None),
            row(2, 0, EventKind::UserMessage, Some(1), None),
        ];
        let rows = build_timeline(&events, false);
        assert_eq!(
            rows.iter().map(TimelineRow::ts_ms).collect::<Vec<_>>(),
            vec![0, 20]
        );
    }

    #[test]
    fn no_step_event_gets_its_own_defensive_row() {
        let events = vec![row(1, 0, EventKind::AgentMessage, None, None)];
        let rows = build_timeline(&events, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_ids(), &[1]);
    }
}
