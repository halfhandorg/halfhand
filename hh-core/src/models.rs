//! The static, compiled-in model table: display name, provider, context
//! limit and per-token prices for models whose transcripts self-report a
//! `model` id.
//!
//! # Rules
//!
//! - **No network.** This table is the only source; there is no runtime or
//!   build-time fetch (Halfhand makes zero network calls, NFR-2).
//! - **Absent means unknown.** A model id that is not in the table resolves to
//!   `None`, and every surface renders that as an em dash, never as `0`. Zero
//!   asserts a known value; a dash admits ignorance. Do not add a row with
//!   guessed numbers to make a dash go away.
//! - **Prices are list prices** for the first-party API, in nano-dollars per
//!   token (USD × 10⁻⁹), so cost arithmetic stays in integers. Cache writes use
//!   the 5-minute-TTL rate. Partner platforms (Bedrock, Vertex) price
//!   separately; the estimate does not try to model them.
//!
//! Model ids are matched exactly, or with a trailing date snapshot
//! (`<id>-YYYYMMDD` or `<id>@YYYYMMDD`), so `claude-haiku-4-5-20251001`
//! resolves to the `claude-haiku-4-5` row.

/// One row of the compiled-in model table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelInfo {
    /// The canonical model id, as transcripts self-report it.
    pub id: &'static str,
    /// Human display name (`Claude Opus 4.8`).
    pub display: &'static str,
    /// The provider that serves the model (`Anthropic`).
    pub provider: &'static str,
    /// Context window, in tokens.
    pub context_limit: u64,
    /// Price per uncached input token, nano-USD.
    pub input_nano_usd: u64,
    /// Price per output token, nano-USD.
    pub output_nano_usd: u64,
    /// Price per cache-read input token, nano-USD.
    pub cache_read_nano_usd: u64,
    /// Price per cache-write input token (5-minute TTL), nano-USD.
    pub cache_write_nano_usd: u64,
}

/// Build an Anthropic row. Prices are nano-USD per token, which is the same
/// number as milli-USD per million tokens: `$3/MTok` is `3000`.
const fn row(
    id: &'static str,
    display: &'static str,
    context_limit: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
) -> ModelInfo {
    ModelInfo {
        id,
        display,
        provider: "Anthropic",
        context_limit,
        input_nano_usd: input,
        output_nano_usd: output,
        cache_read_nano_usd: cache_read,
        cache_write_nano_usd: cache_write,
    }
}

const M1: u64 = 1_000_000;
const K200: u64 = 200_000;

/// The table. Prices in nano-USD per token (= milli-USD per MTok): input,
/// output, cache read, cache write (5-minute TTL, 1.25× input). Claude Haiku 5.5 is listed at
/// its ≤100K-token-prompt tier.
pub const MODELS: &[ModelInfo] = &[
    row(
        "claude-fable-5-1",
        "Claude Fable 5.1",
        M1,
        10000,
        50000,
        250,
        12500,
    ),
    row(
        "claude-mythos-5-1",
        "Claude Mythos 5.1",
        M1,
        10000,
        50000,
        250,
        12500,
    ),
    row(
        "claude-fable-5",
        "Claude Fable 5",
        M1,
        10000,
        50000,
        1000,
        12500,
    ),
    row(
        "claude-mythos-5",
        "Claude Mythos 5",
        M1,
        10000,
        50000,
        1000,
        12500,
    ),
    row(
        "claude-opus-5-5",
        "Claude Opus 5.5",
        M1,
        4000,
        20000,
        200,
        5000,
    ),
    row("claude-opus-5", "Claude Opus 5", M1, 5000, 25000, 500, 6250),
    row(
        "claude-opus-4-8",
        "Claude Opus 4.8",
        M1,
        5000,
        25000,
        500,
        6250,
    ),
    row(
        "claude-opus-4-7",
        "Claude Opus 4.7",
        M1,
        5000,
        25000,
        500,
        6250,
    ),
    row(
        "claude-opus-4-6",
        "Claude Opus 4.6",
        M1,
        5000,
        25000,
        500,
        6250,
    ),
    row(
        "claude-sonnet-5-5",
        "Claude Sonnet 5.5",
        M1,
        2000,
        10000,
        200,
        2500,
    ),
    row(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        M1,
        2000,
        10000,
        200,
        2500,
    ),
    row(
        "claude-sonnet-4-6",
        "Claude Sonnet 4.6",
        M1,
        3000,
        15000,
        300,
        3750,
    ),
    row(
        "claude-haiku-5-5",
        "Claude Haiku 5.5",
        M1,
        100,
        500,
        10,
        125,
    ),
    row(
        "claude-haiku-4-5",
        "Claude Haiku 4.5",
        K200,
        1000,
        5000,
        100,
        1250,
    ),
];

/// Look up a self-reported model id. `None` when the model is not in the
/// table — render that as a dash, never as zero.
#[must_use]
pub fn lookup(model_id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| matches_id(m.id, model_id))
}

/// `candidate` is `id` exactly, or `id` plus a `-YYYYMMDD` / `@YYYYMMDD`
/// snapshot suffix.
fn matches_id(id: &str, candidate: &str) -> bool {
    let Some(rest) = candidate.strip_prefix(id) else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let mut chars = rest.chars();
    matches!(chars.next(), Some('-' | '@')) && rest.len() == 9 && chars.all(|c| c.is_ascii_digit())
}

/// Token counts priced together by [`cost_nano_usd`]. Fields a transcript did
/// not report are `0` here: the caller has already decided the usage is
/// known; unknown *models* are what make a cost unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PricedTokens {
    /// Uncached input tokens.
    pub input: u64,
    /// Output tokens (including any reasoning tokens the provider bills as output).
    pub output: u64,
    /// Cache-read input tokens.
    pub cache_read: u64,
    /// Cache-write input tokens.
    pub cache_write: u64,
}

impl PricedTokens {
    /// Build from the four billed counts.
    #[must_use]
    pub fn new(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write,
        }
    }
}

/// The list-price cost of `tokens` on `model`, in nano-USD (saturating).
#[must_use]
pub fn cost_nano_usd(model: &ModelInfo, tokens: PricedTokens) -> u128 {
    u128::from(tokens.input) * u128::from(model.input_nano_usd)
        + u128::from(tokens.output) * u128::from(model.output_nano_usd)
        + u128::from(tokens.cache_read) * u128::from(model.cache_read_nano_usd)
        + u128::from(tokens.cache_write) * u128::from(model.cache_write_nano_usd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_snapshot_ids_resolve() {
        assert_eq!(
            lookup("claude-opus-4-8").map(|m| m.id),
            Some("claude-opus-4-8")
        );
        assert_eq!(
            lookup("claude-haiku-4-5-20251001").map(|m| m.id),
            Some("claude-haiku-4-5")
        );
        assert_eq!(
            lookup("claude-haiku-4-5@20251001").map(|m| m.id),
            Some("claude-haiku-4-5")
        );
    }

    #[test]
    fn prefixes_do_not_cross_match() {
        // `claude-opus-5` must not swallow `claude-opus-5-5`, and vice versa.
        assert_eq!(
            lookup("claude-opus-5-5").map(|m| m.id),
            Some("claude-opus-5-5")
        );
        assert_eq!(lookup("claude-opus-5").map(|m| m.id), Some("claude-opus-5"));
        assert_eq!(lookup("claude-sonnet-5-55"), None);
        assert_eq!(lookup("claude-opus-5-2026"), None);
    }

    #[test]
    fn unknown_models_are_none_not_zero() {
        assert!(lookup("glm-5.2").is_none());
        assert!(lookup("").is_none());
        assert!(lookup("claude-sonnet-4-5").is_none());
    }

    #[test]
    fn cost_is_integer_exact() {
        let m = lookup("claude-sonnet-4-6").expect("in table");
        // 1M input at $3 + 1M output at $15 = $18 = 18e9 nano-USD.
        let c = cost_nano_usd(m, PricedTokens::new(1_000_000, 1_000_000, 0, 0));
        assert_eq!(c, 18_000_000_000);
    }

    #[test]
    fn ids_are_unique() {
        for (i, a) in MODELS.iter().enumerate() {
            for b in &MODELS[i + 1..] {
                assert_ne!(a.id, b.id);
            }
        }
    }
}
