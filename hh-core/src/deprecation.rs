//! Standard deprecation-warning printer (STABILITY.md).
//!
//! Every CLI flag, config key, or env var that Halfhand ever deprecates
//! prints the same one-line stderr shape via [`warn_deprecated`], deduplicated
//! per unique `id` so a call site reached more than once in a single
//! invocation (a loop over config keys, a code path hit from two callers)
//! never prints the same warning twice.

use std::sync::Mutex;

/// Warning ids already printed in this process, so a repeat call with the
/// same `id` is a no-op. A `Vec` (not a `HashSet`) because the expected
/// cardinality is tiny (a handful of distinct deprecations per invocation at
/// most) and linear scan avoids pulling in a hasher for it.
static PRINTED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// Print a standard deprecation warning to stderr — `hh: warning: <what> is
/// deprecated; <guidance>` — unless `id` was already printed once in this
/// process.
///
/// `id` identifies *this* deprecation (a stable string constant such as
/// `"legacy-config-filename"`, not the formatted message), so callers that
/// may run more than once per invocation still print at most one line per
/// distinct deprecation. Returns `true` if this call actually printed.
pub fn warn_deprecated(id: &'static str, what: &str, guidance: &str) -> bool {
    if !mark_seen(id) {
        return false;
    }
    eprintln!("{}", format_deprecation_warning(what, guidance));
    true
}

/// Records `id` as seen; returns `true` the first time a given `id` is
/// passed in this process, `false` on every subsequent call with that `id`.
fn mark_seen(id: &'static str) -> bool {
    let mut printed = PRINTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if printed.contains(&id) {
        false
    } else {
        printed.push(id);
        true
    }
}

/// The standard deprecation-warning line, without printing it — split out
/// from [`warn_deprecated`] so the exact shape is testable without touching
/// stderr or the process-global dedup state.
#[must_use]
pub fn format_deprecation_warning(what: &str, guidance: &str) -> String {
    format!("hh: warning: {what} is deprecated; {guidance}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_matches_the_standard_shape() {
        assert_eq!(
            format_deprecation_warning("`--foo`", "use `--bar` instead"),
            "hh: warning: `--foo` is deprecated; use `--bar` instead"
        );
    }

    #[test]
    fn warn_deprecated_prints_at_most_once_per_id() {
        // A process-unique id: parallel test threads share `PRINTED`, so a
        // collision with another test's id would make this flaky.
        let id = "test-only:warn_deprecated_prints_at_most_once_per_id";
        assert!(
            warn_deprecated(id, "thing", "do X instead"),
            "first call for a fresh id must print"
        );
        assert!(
            !warn_deprecated(id, "thing", "do X instead"),
            "second call for the same id must be a no-op"
        );
    }

    #[test]
    fn distinct_ids_each_print_once() {
        assert!(warn_deprecated(
            "test-only:distinct_ids_each_print_once:a",
            "a",
            "guidance a"
        ));
        assert!(warn_deprecated(
            "test-only:distinct_ids_each_print_once:b",
            "b",
            "guidance b"
        ));
    }
}
