//! Property tests for step assignment (FR-3.4, `hh_core::step::assign_steps`).
//!
//! For any interleaving of events (arbitrary `ts_ms`/`kind`/`correlates`,
//! including out-of-order call/result pairs and orphaned correlations), the
//! assigned step ordinals must be: `terminal_output` -> `None`, everything
//! else -> `Some`; the distinct assigned values are dense (`1..=k`, no gaps);
//! and each value is shared by either exactly one non-deferred event, or one
//! `tool_call` plus every `tool_result` correlating to it (never a stray
//! kind, never two unrelated calls sharing a step).

use hh_core::{assign_steps, EventKind, EventRow};
use proptest::prelude::*;
use std::collections::HashMap;

fn kind_from_u8(v: u8) -> EventKind {
    match v % 12 {
        0 => EventKind::Lifecycle,
        1 => EventKind::UserMessage,
        2 => EventKind::AgentMessage,
        3 => EventKind::Thinking,
        4 => EventKind::ToolCall,
        5 => EventKind::ToolResult,
        6 => EventKind::McpRequest,
        7 => EventKind::McpResponse,
        8 => EventKind::McpNotification,
        9 => EventKind::FileChange,
        10 => EventKind::TerminalOutput,
        _ => EventKind::Error,
    }
}

fn rows_strategy() -> impl Strategy<Value = Vec<EventRow>> {
    prop::collection::vec(
        (any::<i64>(), any::<u8>(), prop::option::of(0i64..25)),
        1..30,
    )
    .prop_map(|raw| {
        raw.into_iter()
            .enumerate()
            .map(|(i, (ts_ms, k, correlates))| EventRow {
                #[allow(clippy::cast_possible_wrap)]
                id: i as i64,
                session_id: "s".to_string(),
                ts_ms,
                kind: kind_from_u8(k),
                step: None,
                correlates,
            })
            .collect()
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn assign_steps_invariants(mut rows in rows_strategy()) {
        assign_steps(&mut rows);

        // terminal_output never gets a step; everything else always does.
        for r in &rows {
            if r.kind == EventKind::TerminalOutput {
                prop_assert_eq!(r.step, None);
            } else {
                prop_assert!(r.step.is_some());
            }
        }

        // Group by assigned step and check each group's shape.
        let mut groups: HashMap<i64, Vec<&EventRow>> = HashMap::new();
        for r in &rows {
            if let Some(s) = r.step {
                groups.entry(s).or_default().push(r);
            }
        }
        for members in groups.values() {
            if members.len() > 1 {
                let calls: Vec<_> = members
                    .iter()
                    .filter(|r| r.kind == EventKind::ToolCall)
                    .collect();
                let others: Vec<_> = members
                    .iter()
                    .filter(|r| r.kind != EventKind::ToolCall && r.kind != EventKind::ToolResult)
                    .collect();
                prop_assert_eq!(
                    calls.len(), 1,
                    "a multi-member step group must have exactly one tool_call"
                );
                prop_assert!(
                    others.is_empty(),
                    "a multi-member step group may only hold a tool_call + tool_results"
                );
                let call_id = calls[0].id;
                for r in members.iter().filter(|r| r.kind == EventKind::ToolResult) {
                    prop_assert_eq!(r.correlates, Some(call_id));
                }
            }
        }

        // Distinct step values are dense: 1..=k, no gaps.
        let mut distinct: Vec<i64> = groups.keys().copied().collect();
        distinct.sort_unstable();
        #[allow(clippy::cast_possible_wrap)]
        let expected: Vec<i64> = (1..=distinct.len() as i64).collect();
        prop_assert_eq!(distinct, expected);

        // Idempotent: re-running yields the same assignment.
        let before: Vec<Option<i64>> = rows.iter().map(|r| r.step).collect();
        assign_steps(&mut rows);
        let after: Vec<Option<i64>> = rows.iter().map(|r| r.step).collect();
        prop_assert_eq!(before, after);
    }
}
