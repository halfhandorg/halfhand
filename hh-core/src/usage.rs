//! Session context: model identity, self-reported token usage, and the
//! read-time figures derived from them (migration 0007).
//!
//! # Two token families, never blended
//!
//! - **Self-reported** counts come from the transcript's own `usage` objects.
//!   The writer task accumulates them into `sessions.input_tokens` and
//!   friends as events arrive ([`TokenSample`]), deduplicating the repeated
//!   usage a transcript writes once per content block of the same message.
//! - **Estimated** counts are derived from recorded payload size
//!   ([`SessionContext::estimated_tokens`], bytes ÷ 4). They exist for
//!   sessions with no self-reported usage and are always labelled as an
//!   estimate. They are never added to, or substituted for, a self-reported
//!   count.
//!
//! `total_tokens` is derived at read time ([`crate::SessionRow::total_tokens`])
//! and never stored, so it cannot drift from its parts.
//!
//! # How usage reaches the writer
//!
//! An adapter that parses a model response attaches three keys to the body of
//! the first event it emits for that response: [`BODY_MODEL_KEY`] (the
//! self-reported model id), [`BODY_USAGE_KEY`] (the usage object, verbatim)
//! and [`BODY_MESSAGE_ID_KEY`] (the response id, for de-duplication). The
//! keys are additive in `body` (`docs/json.md`).

use serde_json::Value;

/// Body key holding the self-reported model id of the response an event came from.
pub const BODY_MODEL_KEY: &str = "model";
/// Body key holding the response's self-reported usage object, verbatim.
pub const BODY_USAGE_KEY: &str = "usage";
/// Body key holding the response id; repeated usage for one id replaces, not adds.
pub const BODY_MESSAGE_ID_KEY: &str = "message_id";

/// Bytes per token for the size-based estimate. A coarse, provider-neutral
/// rule of thumb; the estimate is labelled as such everywhere it appears.
pub const ESTIMATE_BYTES_PER_TOKEN: u64 = 4;

/// One response's self-reported token counts. `None` means the transcript did
/// not report that count — it is not the same as `Some(0)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TokenSample {
    /// Uncached input tokens.
    pub input: Option<u64>,
    /// Output tokens.
    pub output: Option<u64>,
    /// Input tokens read from the prompt cache.
    pub cache_read: Option<u64>,
    /// Input tokens written to the prompt cache.
    pub cache_write: Option<u64>,
    /// Reasoning tokens, when the provider reports them separately.
    pub reasoning: Option<u64>,
}

impl TokenSample {
    /// Read a usage object. Accepts the Anthropic field names
    /// (`input_tokens`, `output_tokens`, `cache_read_input_tokens`,
    /// `cache_creation_input_tokens`) and a `reasoning_tokens` count either at
    /// the top level or under `output_tokens_details`. Non-numeric or
    /// negative values are treated as absent. Never panics.
    #[must_use]
    pub fn from_usage(usage: &Value) -> Self {
        let n = |key: &str| usage.get(key).and_then(Value::as_u64);
        Self {
            input: n("input_tokens"),
            output: n("output_tokens"),
            cache_read: n("cache_read_input_tokens"),
            cache_write: n("cache_creation_input_tokens"),
            reasoning: n("reasoning_tokens").or_else(|| {
                usage
                    .get("output_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(Value::as_u64)
            }),
        }
    }

    /// Whether any count is present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input.is_none()
            && self.output.is_none()
            && self.cache_read.is_none()
            && self.cache_write.is_none()
            && self.reasoning.is_none()
    }

    /// Prompt tokens this response consumed: input + cache read + cache
    /// write. `None` when none of the three was reported. This is the
    /// context-window occupancy at that turn.
    #[must_use]
    pub fn context_tokens(&self) -> Option<u64> {
        if self.input.is_none() && self.cache_read.is_none() && self.cache_write.is_none() {
            return None;
        }
        Some(self.input.unwrap_or(0) + self.cache_read.unwrap_or(0) + self.cache_write.unwrap_or(0))
    }
}

/// Signed per-field difference `new - old` for the writer's in-place update.
/// A field `new` does not report yields `None` (leave the column alone).
pub(crate) fn sample_delta(new: &TokenSample, old: &TokenSample) -> [Option<i64>; 5] {
    let d = |n: Option<u64>, o: Option<u64>| {
        n.map(|n| {
            let n = i64::try_from(n).unwrap_or(i64::MAX);
            let o = i64::try_from(o.unwrap_or(0)).unwrap_or(i64::MAX);
            n.saturating_sub(o)
        })
    };
    [
        d(new.input, old.input),
        d(new.output, old.output),
        d(new.cache_read, old.cache_read),
        d(new.cache_write, old.cache_write),
        d(new.reasoning, old.reasoning),
    ]
}

/// A contiguous run of steps answered by one self-reported model.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelSpan {
    /// The self-reported model id.
    pub model_id: String,
    /// First step of the run (`None` if its events have no step yet).
    pub first_step: Option<i64>,
    /// Last step of the run.
    pub last_step: Option<i64>,
}

/// Read-time context for one session, derived from its events: model runs,
/// message counts, the latest turn's context occupancy, a size-based token
/// estimate, and a list-price cost. Built by
/// [`crate::store::Store::session_context`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionContext {
    /// Every model the transcript reported, in order, with its step range.
    /// Consecutive responses from the same model collapse into one span.
    pub models: Vec<ModelSpan>,
    /// `user_message` events.
    pub user_messages: i64,
    /// `agent_message` events.
    pub agent_messages: i64,
    /// `tool_call` events.
    pub tool_calls: i64,
    /// Self-reported prompt tokens of the most recent response that reported
    /// usage (input + cache read + cache write). `None` when no response did.
    pub last_turn_context_tokens: Option<u64>,
    /// Size-based estimate: semantic event payload bytes ÷
    /// [`ESTIMATE_BYTES_PER_TOKEN`]. Never a self-reported number.
    pub estimated_tokens: u64,
    /// List-price cost in nano-USD, priced per response with that response's
    /// model. `None` when no usage was reported or any priced response came
    /// from a model absent from [`crate::models`] — a partial sum would
    /// understate the cost, so it is unknown rather than wrong.
    pub cost_nano_usd: Option<u128>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_anthropic_usage() {
        let s = TokenSample::from_usage(&json!({
            "input_tokens": 10, "output_tokens": 5,
            "cache_read_input_tokens": 100, "cache_creation_input_tokens": 20,
            "service_tier": "standard"
        }));
        assert_eq!(s.input, Some(10));
        assert_eq!(s.cache_write, Some(20));
        assert_eq!(s.reasoning, None);
        assert_eq!(s.context_tokens(), Some(130));
    }

    #[test]
    fn malformed_usage_is_absent_not_zero() {
        for v in [
            json!(null),
            json!("x"),
            json!([1]),
            json!({"input_tokens": -3}),
        ] {
            let s = TokenSample::from_usage(&v);
            assert!(s.is_empty(), "{v}");
            assert_eq!(s.context_tokens(), None);
        }
    }

    #[test]
    fn nested_reasoning_tokens() {
        let s = TokenSample::from_usage(&json!({
            "output_tokens": 9, "output_tokens_details": {"reasoning_tokens": 4}
        }));
        assert_eq!(s.reasoning, Some(4));
    }

    #[test]
    fn delta_replaces_a_repeated_message() {
        let old = TokenSample {
            input: Some(10),
            output: Some(3),
            ..TokenSample::default()
        };
        let new = TokenSample {
            input: Some(10),
            output: Some(8),
            ..TokenSample::default()
        };
        assert_eq!(
            sample_delta(&new, &old),
            [Some(0), Some(5), None, None, None]
        );
    }
}
