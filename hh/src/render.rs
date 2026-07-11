//! Shared output-formatting helpers for the `hh` binary (NFR-7).
//!
//! Glyph/color/time formatting used by more than one subcommand lives here so
//! the CLI speaks one visual language: consistent glyphs (`✓ ✗ ●`), one
//! accent color, aligned columns, and humanized times (CLAUDE.md CLI/UX
//! standards). Color is gated on `NO_COLOR` and a TTY via [`use_color`]; every
//! helper has a plain-text fallback so piped output stays pipe-safe.

use std::io::IsTerminal;

use hh_core::SessionStatus;
use owo_colors::OwoColorize;

/// Whether the terminal can render ANSI escapes. Always true on Unix; on
/// Windows this asks crossterm, which enables virtual terminal processing on
/// the console as a side effect (and caches the result). Modern terminals
/// (Windows Terminal) have it on already; legacy conhost needs the enable
/// call, and if even that fails we fall back to plain output.
fn ansi_supported() -> bool {
    #[cfg(windows)]
    {
        crossterm::ansi_support::supports_ansi()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Whether to emit ANSI color on stdout: disabled by `NO_COLOR`, non-TTY
/// output (CLAUDE.md: plain, pipe-safe), or a console without ANSI support.
pub(crate) fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() && ansi_supported()
}

/// Whether to emit ANSI color on stderr (error/hint rendering): disabled by
/// `NO_COLOR`, a non-TTY stderr, or a console without ANSI support.
pub(crate) fn use_color_stderr() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal() && ansi_supported()
}

/// The status glyph (CLAUDE.md: `✓ ✗ ●`).
pub(crate) fn status_glyph(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Ok => "✓",
        SessionStatus::Error => "✗",
        SessionStatus::Interrupted | SessionStatus::Recording => "●",
    }
}

/// Render the `glyph status` field (e.g. `✓ ok`), colored when `color` is
/// true. The color map is the single accent/error convention shared by the
/// epilogue, `hh list`, and `hh inspect`:
/// - `ok` → green `✓`
/// - `error` → red `✗`
/// - `interrupted` → yellow `●`
/// - `recording` → cyan `●` (transient; distinct from a finished state)
pub(crate) fn render_status(status: SessionStatus, color: bool) -> String {
    let field = format!("{} {status}", status_glyph(status));
    if !color {
        return field;
    }
    match status {
        SessionStatus::Ok => field.green().to_string(),
        SessionStatus::Error => field.red().to_string(),
        SessionStatus::Interrupted => field.yellow().to_string(),
        SessionStatus::Recording => field.cyan().to_string(),
    }
}

/// Humanize a millisecond duration as `4m32s` / `12s` / `350ms` (NFR-7).
pub(crate) fn humanize_ms(ms: i64) -> String {
    if ms < 0 {
        return "0ms".to_string();
    }
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{hours}h{minutes}m{secs}s")
    } else if minutes > 0 {
        format!("{minutes}m{secs}s")
    } else {
        format!("{secs}s")
    }
}

/// Format a millisecond offset as `HH:MM:SS` (clamped non-negative), used for
/// per-step timestamps in `hh inspect` and the replay timeline.
pub(crate) fn format_hms(ms: i64) -> String {
    let total_secs = ms.max(0) / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Humanize a `started_at` (unix-ms) relative to `now` (FR-5.1). Recent
/// sessions show "just now"/"5m ago"; older than a week show an absolute
/// `YYYY-MM-DD`.
pub(crate) fn humanize_relative(started_at: i64, now: i64) -> String {
    let delta = now - started_at;
    if delta < 0 {
        return "just now".to_string();
    }
    let secs = delta / 1000;
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    format_date_from_unix_days(started_at / 86_400_000)
}

/// Format a unix-day count as `YYYY-MM-DD` using the proleptic Gregorian
/// calendar. Good enough for the list view's "older than a week" column.
pub(crate) fn format_date_from_unix_days(days: i64) -> String {
    // Algorithm from Howard Hinnant's `civil_from_days`.
    let z = days + 719_468; // days since 0000-03-01
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Truncate `s` to `max` chars, appending `…` if it was longer.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}…")
}

/// Current unix-ms UTC timestamp (used for relative "started" times).
pub(crate) fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
