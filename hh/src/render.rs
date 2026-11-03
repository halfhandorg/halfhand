//! Shared output-formatting helpers for the `hh` binary.
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
/// the session is degraded (see [`session_is_degraded`]). A session that
/// finalized `ok` but whose adapter never found a transcript, or whose recorder
/// failed to write some events, must not look identical to a clean one; the
/// glyph is plain unicode (visible even with `NO_COLOR` and in piped output)
/// and yellow when color is enabled.
pub(crate) fn render_status_marked(status: SessionStatus, degraded: bool, color: bool) -> String {
    let base = render_status(status, color);
    if degraded {
        if color {
            format!("{base} {}", "⚠".yellow())
        } else {
            format!("{base} ⚠")
        }
    } else {
        base
    }
}

/// A session is degraded — shown with `⚠`, never as a clean `ok` — when its
/// adapter ended `Degraded` or the recorder dropped events it could
/// not write (`dropped_events > 0`, so the timeline has gaps).
pub(crate) fn session_is_degraded(row: &hh_core::SessionRow) -> bool {
    row.adapter_status == AdapterStatus::Degraded || row.dropped_events > 0
}

/// Humanize a millisecond duration as `4m32s` / `12s` / `350ms`.
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

/// An ISO-8601 duration for `--json` (`PT1H49M40S`, `PT0.35S`, `PT0S`).
/// Zero components are omitted; milliseconds become a fractional second
/// with trailing zeros trimmed. Negative input is clamped to zero.
pub(crate) fn iso8601_duration(ms: i64) -> String {
    use std::fmt::Write as _;
    let ms = ms.max(0);
    let total_secs = ms / 1000;
    let frac = ms % 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    let mut out = String::from("PT");
    if hours > 0 {
        let _ = write!(out, "{hours}H");
    }
    if minutes > 0 {
        let _ = write!(out, "{minutes}M");
    }
    if secs > 0 || frac > 0 || (hours == 0 && minutes == 0) {
        if frac > 0 {
            let f = format!("{frac:03}");
            let _ = write!(out, "{secs}.{}S", f.trim_end_matches('0'));
        } else {
            let _ = write!(out, "{secs}S");
        }
    }
    out
}

/// Group an integer's digits in threes (`1,234,567`).
pub(crate) fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A unix-ms timestamp as `YYYY-MM-DD HH:MM:SS UTC`.
pub(crate) fn format_utc(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1000);
    let day_secs = secs.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02}:{:02} UTC",
        format_date_from_unix_days(secs.div_euclid(86_400)),
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60
    )
}

/// A unix-ms timestamp as RFC 3339 UTC (`2026-11-02T10:14:03.250Z`) for JSON.
pub(crate) fn rfc3339_utc(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1000);
    let day_secs = secs.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}.{:03}Z",
        format_date_from_unix_days(secs.div_euclid(86_400)),
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60,
        unix_ms.rem_euclid(1000)
    )
}

/// A nano-USD amount as dollars: `$12.34` at a cent or more, four decimals
/// below a cent (`$0.0042`) so a cheap session does not read as free.
/// Integer arithmetic only; rounds half up.
pub(crate) fn format_usd(nano: u128) -> String {
    const CENT: u128 = 10_000_000; // nano-USD in one cent
    if nano >= CENT {
        let cents = (nano + CENT / 2) / CENT;
        format!("${}.{:02}", cents / 100, cents % 100)
    } else {
        let tenth_mills = (nano + 50_000) / 100_000; // units of $0.0001
        format!("$0.{tenth_mills:04}")
    }
}

/// A nano-USD amount as a decimal USD string for JSON (`"12.345678"`, six
/// decimals, rounded half up). A string, not a float, so no precision is
/// lost or invented.
pub(crate) fn usd_decimal(nano: u128) -> String {
    let micro = (nano + 500) / 1000;
    format!("{}.{:06}", micro / 1_000_000, micro % 1_000_000)
}

/// Humanize a byte count as `1.4 KiB` / `980 B` / `3.2 MiB` using binary
/// units (1024-based), one decimal place for KiB and above and plain bytes
/// below. Used by `hh gc` and `hh stats`. Computed with integer math (no
/// `f64` cast) so it never loses precision and never trips
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

/// Humanize a `started_at` (unix-ms) relative to `now`. Recent
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

/// A session's command line for display, or for a session recorded by
/// `hh watch` (no command, effects-only capture) a plain label saying so,
/// so `hh list` and `hh inspect` never show a blank where the command goes.
pub(crate) fn command_line(row: &hh_core::SessionRow) -> String {
    if row.command.is_empty() && row.capture_tier.as_deref() == Some(hh_core::CAPTURE_TIER_EFFECTS)
    {
        return "(watch: file changes only)".to_string();
    }
    row.command.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_durations() {
        assert_eq!(iso8601_duration(0), "PT0S");
        assert_eq!(iso8601_duration(350), "PT0.35S");
        assert_eq!(iso8601_duration(6_580_000), "PT1H49M40S");
        assert_eq!(iso8601_duration(3_600_000), "PT1H");
        assert_eq!(iso8601_duration(61_005), "PT1M1.005S");
        assert_eq!(iso8601_duration(-5), "PT0S");
        assert_eq!(humanize_ms(6_580_000), "1h49m40s");
    }

    #[test]
    fn thousands_grouping() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(-12_345), "-12,345");
    }

    #[test]
    fn usd_formatting() {
        assert_eq!(format_usd(0), "$0.0000");
        assert_eq!(format_usd(4_200_000), "$0.0042");
        assert_eq!(format_usd(11_000_000_000), "$11.00");
        assert_eq!(format_usd(1_234_999_999), "$1.23");
        assert_eq!(usd_decimal(11_000_000_000), "11.000000");
        assert_eq!(usd_decimal(4_200_000), "0.004200");
    }

    #[test]
    fn utc_timestamps() {
        assert_eq!(format_utc(1_700_000_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(rfc3339_utc(1_700_000_000_250), "2023-11-14T22:13:20.250Z");
    }
}
