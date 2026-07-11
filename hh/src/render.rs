//! Shared output-formatting helpers for the `hh` binary (NFR-7).
//!
//! Glyph/color/time formatting used by more than one subcommand lives here so
//! the CLI speaks one visual language: consistent glyphs (`✓ ✗ ●`), one
//! accent color, aligned columns, and humanized times (CLAUDE.md CLI/UX
//! standards). Color is gated on `NO_COLOR` and a TTY via [`use_color`]; every
//! helper has a plain-text fallback so piped output stays pipe-safe.

use std::io::IsTerminal;

use hh_core::{AdapterStatus, SessionStatus};
use owo_colors::OwoColorize;

/// Whether to emit ANSI color on stdout: disabled by `NO_COLOR` or non-TTY
/// output (CLAUDE.md: plain, pipe-safe).
pub(crate) fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Whether to emit ANSI color on stderr (error/hint rendering): disabled by
/// `NO_COLOR` or a non-TTY stderr.
pub(crate) fn use_color_stderr() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
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

/// Render the `glyph status` field, appending a visible `⚠` warning glyph when
/// the adapter ended `Degraded` (FR-1.5). A session that finalized `ok` but
/// whose adapter never found a transcript must not look identical to a clean
/// one in `hh list`; the glyph is plain unicode (visible even with `NO_COLOR`
/// and in piped output) and yellow when color is enabled.
pub(crate) fn render_status_with_adapter(
    status: SessionStatus,
    adapter: AdapterStatus,
    color: bool,
) -> String {
    let base = render_status(status, color);
    if adapter == AdapterStatus::Degraded {
        if color {
            format!("{base} {}", "⚠".yellow())
        } else {
            format!("{base} ⚠")
        }
    } else {
        base
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

/// Humanize a byte count as `1.4 KiB` / `980 B` / `3.2 MiB` using binary
/// units (1024-based), one decimal place for KiB and above and plain bytes
/// below. Used by `hh gc` and `hh stats` (NFR-7 / Area 3). Computed with
/// integer math (no `f64` cast) so it never loses precision and never trips
/// `clippy::cast_precision_loss`.
pub(crate) fn humanize_bytes(bytes: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("PiB", 1u64 << 50),
        ("TiB", 1u64 << 40),
        ("GiB", 1u64 << 30),
        ("MiB", 1u64 << 20),
        ("KiB", 1u64 << 10),
    ];
    for &(unit, scale) in UNITS {
        if bytes >= scale {
            // tenths = floor(bytes / (scale / 10)); integer scale/10 keeps this
            // overflow-free (no bytes*10). The slight floor rounding is
            // acceptable for a human display.
            let tenths = bytes / (scale / 10);
            return format!("{}.{} {unit}", tenths / 10, tenths % 10);
        }
    }
    format!("{bytes} B")
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

/// The visible (printed) width of `s` in terminal columns, i.e. the char count
/// with ANSI CSI escape sequences (`\x1b[...m`) stripped. The list/inspect
/// tables pad columns by width, so they must measure what the user actually
/// sees — measuring byte length instead lets color escapes inflate a cell's
/// "width" and misalign every column to the right of a colored cell (the
/// "headers don't line up with the data" bug). Char count is an approximation
/// (a few glyphs like `⚠` render double-wide on some terminals) but it is
/// stable and far closer than raw byte length; perfect alignment under color
/// is not attainable without a unicode-width table dependency CLAUDE.md
/// steers us away from.
pub(crate) fn visible_width(s: &str) -> usize {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Normal,
        Esc, // just saw ESC; next char decides the escape kind
        Csi, // inside ESC [ ... ; skip until the final byte 0x40..=0x7E
    }
    let mut count = 0usize;
    let mut state = State::Normal;
    for c in s.chars() {
        match state {
            State::Normal => {
                if c == '\u{1b}' {
                    state = State::Esc;
                } else {
                    count += 1;
                }
            }
            State::Esc => {
                // `ESC [` opens a CSI sequence (color codes use this); anything
                // else is a one-char escape that ended at this control byte.
                state = if c == '[' { State::Csi } else { State::Normal };
            }
            State::Csi => {
                // Skip parameter/intermediate bytes; the final byte (0x40..=0x7E,
                // e.g. 'm') closes the sequence.
                if ('\u{40}'..='\u{7E}').contains(&c) {
                    state = State::Normal;
                }
            }
        }
    }
    count
}
