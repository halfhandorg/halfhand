//! `hh doctor --capture`: how faithfully the most recent session's file
//! effects were captured.
//!
//! The report is built from facts the recorder already persists (the
//! session's cwd, its `file_changes` rows with their before-side provenance,
//! and its dropped-event tally) plus one live, per-platform probe of capture
//! health. Facts the recorder does not persist are reported as unknown — a
//! dash in the human output, `null` in JSON — never as a made-up number:
//!
//! - whether each change was observed live or recovered by the shutdown
//!   reconciliation backstop;
//! - whether a watch was refused (permission denied) during the session;
//! - on Windows, whether the backend signalled an event-buffer overflow.

use std::fmt::Write as _;
use std::path::Path;

use hh_core::SessionCapture;
use owo_colors::OwoColorize;

/// Before-side coverage at or above this percentage reads as healthy.
pub const COVERAGE_HEALTHY_PCT: f64 = 95.0;

/// Below this many free inotify watches the Linux probe warns.
const INOTIFY_LOW_HEADROOM: u64 = 1024;

/// The rendered value for an unknown fact.
const DASH: &str = "—";

/// The whole capture-health report.
pub struct CaptureReport {
    /// The most recent session's capture facts, if any session exists.
    pub session: Option<SessionCapture>,
    /// The live per-platform probe.
    pub platform: PlatformHealth,
}

/// Outcome of a platform probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Nothing wrong found.
    Ok,
    /// A condition that loses or risks losing events.
    Warn,
    /// The fact is not available on this platform or not persisted.
    Unknown,
}

impl Health {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Unknown => "unknown",
        }
    }
}

/// Per-platform capture health.
pub struct PlatformHealth {
    /// `linux`, `macos`, `windows`, or another `std::env::consts::OS`.
    pub os: &'static str,
    /// Human name of what was probed.
    pub check: &'static str,
    /// The outcome.
    pub status: Health,
    /// Short value (e.g. free watch count), `None` when unknown.
    pub value: Option<String>,
    /// One-line explanation with a suggested fix where relevant.
    pub detail: String,
}

/// Build the report: read the most recent session and probe this platform.
pub fn build(store: &hh_core::Store) -> anyhow::Result<CaptureReport> {
    use anyhow::Context as _;
    let session = store
        .latest_session_capture()
        .context("could not read the most recent session's file changes")?;
    let root = session.as_ref().map(|s| Path::new(&s.cwd).to_path_buf());
    Ok(CaptureReport {
        platform: probe_platform(root.as_deref()),
        session,
    })
}

/// Probe this platform's capture health. `root` is the watched root of the
/// most recent session, if any.
fn probe_platform(root: Option<&Path>) -> PlatformHealth {
    #[cfg(target_os = "linux")]
    {
        let _ = root;
        inotify_health(Path::new("/proc"))
    }
    #[cfg(target_os = "macos")]
    {
        macos_privacy_health(root)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = root;
        PlatformHealth {
            os: "windows",
            check: "event-buffer overflow",
            status: Health::Unknown,
            value: None,
            detail: "overflow signals are not persisted; the shutdown rescan recovers \
                     changes an overflow drops"
                .into(),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = root;
        PlatformHealth {
            os: std::env::consts::OS,
            check: "platform capture health",
            status: Health::Unknown,
            value: None,
            detail: "no platform probe for this OS".into(),
        }
    }
}

/// Linux: free inotify watches for this user, from `proc` (normally
/// `/proc`; a parameter so tests can use a fixture tree).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn inotify_health(proc: &Path) -> PlatformHealth {
    const CHECK: &str = "inotify watch headroom";
    let Some(max) = read_u64(&proc.join("sys/fs/inotify/max_user_watches")) else {
        return PlatformHealth {
            os: "linux",
            check: CHECK,
            status: Health::Unknown,
            value: None,
            detail: format!(
                "could not read {}",
                proc.join("sys/fs/inotify/max_user_watches").display()
            ),
        };
    };
    let used = inotify_watches_in_use(proc);
    let free = max.saturating_sub(used);
    let status = if free < INOTIFY_LOW_HEADROOM {
        Health::Warn
    } else {
        Health::Ok
    };
    let mut detail = format!("{used} of {max} watches in use by this user");
    if status == Health::Warn {
        detail.push_str(
            " — new directories may go unwatched; raise fs.inotify.max_user_watches \
             (sysctl) or close other watchers",
        );
    }
    PlatformHealth {
        os: "linux",
        check: CHECK,
        status,
        value: Some(format!("{free} free")),
        detail,
    }
}

/// Count `inotify wd:` lines in every readable `/proc/<pid>/fdinfo/*` whose
/// fd is an inotify instance. Processes of other users are unreadable and
/// do not count against this user's limit anyway.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn inotify_watches_in_use(proc: &Path) -> u64 {
    let Ok(pids) = std::fs::read_dir(proc) else {
        return 0;
    };
    let mut total = 0u64;
    for pid in pids.flatten() {
        let name = pid.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(pid.path().join("fdinfo")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(info) = std::fs::read_to_string(fd.path()) else {
                continue;
            };
            let n = info
                .lines()
                .filter(|l| l.starts_with("inotify wd:"))
                .count();
            total += u64::try_from(n).unwrap_or(0);
        }
    }
    total
}

/// macOS: is the watched root behind a privacy (TCC) permission? A protected
/// folder the terminal has no access to fails `read_dir` with
/// `PermissionDenied`, and FSEvents then reports nothing for it.
#[cfg(target_os = "macos")]
fn macos_privacy_health(root: Option<&Path>) -> PlatformHealth {
    const CHECK: &str = "privacy-protected root";
    let Some(root) = root else {
        return PlatformHealth {
            os: "macos",
            check: CHECK,
            status: Health::Unknown,
            value: None,
            detail: "no recorded session to check".into(),
        };
    };
    match std::fs::read_dir(root) {
        Ok(_) => PlatformHealth {
            os: "macos",
            check: CHECK,
            status: Health::Ok,
            value: Some("watchable".into()),
            detail: format!("{} is readable", root.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => PlatformHealth {
            os: "macos",
            check: CHECK,
            status: Health::Warn,
            value: Some("blocked".into()),
            detail: format!(
                "{} cannot be read ({e}) — grant your terminal access in System \
                 Settings → Privacy & Security → Files and Folders (or Full Disk Access)",
                root.display()
            ),
        },
        Err(e) => PlatformHealth {
            os: "macos",
            check: CHECK,
            status: Health::Unknown,
            value: None,
            detail: format!("could not check {}: {e}", root.display()),
        },
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Format a coverage percentage: whole numbers without decimals.
fn format_pct(p: f64) -> String {
    if (p - p.round()).abs() < 0.05 {
        format!("{p:.0}%")
    } else {
        format!("{p:.1}%")
    }
}

/// One rendered row: glyph, label, value, note.
struct Row {
    health: Health,
    label: &'static str,
    value: String,
    note: String,
}

fn rows(report: &CaptureReport) -> Vec<Row> {
    let mut out = Vec::new();
    if let Some(s) = &report.session {
        out.push(Row {
            health: Health::Unknown,
            label: "watched roots",
            value: s.cwd.clone(),
            note: "the session cwd; whether the watch succeeded is not persisted".into(),
        });
        out.push(Row {
            health: Health::Unknown,
            label: "observed live",
            value: DASH.into(),
            note: "not persisted per event".into(),
        });
        out.push(Row {
            health: Health::Unknown,
            label: "recovered by rescan",
            value: DASH.into(),
            note: "not persisted per event".into(),
        });
        let mut parts = format!(
            "{} created, {} modified, {} deleted",
            s.created, s.modified, s.deleted
        );
        if s.renamed > 0 {
            let _ = write!(parts, "; {} renamed", s.renamed);
        }
        out.push(Row {
            health: Health::Unknown,
            label: "file changes",
            value: s.file_changes.to_string(),
            note: parts,
        });
        let (dh, dnote) = if s.dropped.any() {
            (
                Health::Warn,
                format!(
                    "first cause: {} — check free disk space and data-dir permissions",
                    s.dropped.first_error.as_deref().unwrap_or("unknown")
                ),
            )
        } else {
            (Health::Ok, String::new())
        };
        out.push(Row {
            health: dh,
            label: "dropped events",
            value: s.dropped.count.to_string(),
            note: dnote,
        });
        out.push(Row {
            health: Health::Unknown,
            label: "permission denied",
            value: DASH.into(),
            note: "not persisted; `hh run` prints a warning when a watch is refused".into(),
        });
        let (ch, cval, cnote) = match s.before_side_coverage() {
            Some(p) => (
                if p >= COVERAGE_HEALTHY_PCT {
                    Health::Ok
                } else {
                    Health::Warn
                },
                format_pct(p),
                format!(
                    "{} of {} modified/deleted changes have a real before-side",
                    s.with_before, s.needs_before
                ),
            ),
            None => (
                Health::Unknown,
                DASH.into(),
                "no modified or deleted changes with recorded provenance".into(),
            ),
        };
        let mut cnote = cnote;
        if s.unknown_provenance > 0 {
            let _ = write!(
                cnote,
                "; {} recorded before provenance existed, excluded",
                s.unknown_provenance
            );
        }
        out.push(Row {
            health: ch,
            label: "before-side coverage",
            value: cval,
            note: cnote,
        });
    }
    let p = &report.platform;
    out.push(Row {
        health: p.status,
        label: p.check,
        value: p.value.clone().unwrap_or_else(|| DASH.into()),
        note: p.detail.clone(),
    });
    out
}

/// Render the report as aligned plain lines (one accent color per glyph).
pub fn render_plain(report: &CaptureReport, color: bool) -> String {
    let mut out = String::new();
    match &report.session {
        Some(s) => {
            let exit = s
                .exit_code
                .map_or_else(|| DASH.to_string(), |c| c.to_string());
            let _ = writeln!(
                out,
                "capture health — session {} ({}, exit {exit})",
                s.short_id, s.status
            );
        }
        None => {
            let _ = writeln!(
                out,
                "capture health — no recorded sessions yet (run `hh run -- <agent>` first)"
            );
        }
    }
    let rows = rows(report);
    let label_w = rows
        .iter()
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(0);
    let value_w = rows
        .iter()
        .map(|r| r.value.chars().count())
        .max()
        .unwrap_or(0);
    for r in &rows {
        let glyph = match (r.health, color) {
            (Health::Ok, true) => "✓".green().to_string(),
            (Health::Ok, false) => "✓".to_string(),
            (Health::Warn, true) => "✗".red().to_string(),
            (Health::Warn, false) => "✗".to_string(),
            (Health::Unknown, _) => "●".to_string(),
        };
        let line = format!(
            "  {glyph} {label:<label_w$}  {value:<value_w$}  {note}",
            label = r.label,
            value = r.value,
            note = r.note
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The `capture` object added to `hh doctor --json` by `--capture`. Unknown
/// facts are `null`, never 0.
pub fn to_json(report: &CaptureReport) -> serde_json::Value {
    let p = &report.platform;
    let platform = serde_json::json!({
        "os": p.os,
        "check": p.check,
        "status": p.status.as_str(),
        "value": p.value,
        "detail": p.detail,
    });
    let Some(s) = &report.session else {
        return serde_json::json!({
            "session": null,
            "watched_roots": [],
            "events_live": null,
            "events_reconciled": null,
            "file_changes": null,
            "dropped_events": null,
            "dropped_events_reason": null,
            "permission_denied": null,
            "before_side_coverage": null,
            "platform": platform,
        });
    };
    serde_json::json!({
        "session": {
            "id": s.session_id,
            "short_id": s.short_id,
            "status": s.status,
            "exit_code": s.exit_code,
        },
        "watched_roots": [s.cwd],
        "events_live": null,
        "events_reconciled": null,
        "file_changes": {
            "total": s.file_changes,
            "created": s.created,
            "modified": s.modified,
            "deleted": s.deleted,
            "renamed": s.renamed,
        },
        "dropped_events": s.dropped.count,
        "dropped_events_reason": s.dropped.first_error,
        "permission_denied": null,
        "before_side_coverage": {
            "percent": s.before_side_coverage(),
            "covered": s.with_before,
            "eligible": s.needs_before,
            "unknown_provenance": s.unknown_provenance,
        },
        "platform": platform,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hh_core::DroppedEvents;

    fn session() -> SessionCapture {
        SessionCapture {
            session_id: "0190aaaa-0000-7000-8000-000000000000".into(),
            short_id: "0190aa".into(),
            cwd: "/home/me/work".into(),
            status: "error".into(),
            exit_code: Some(3),
            file_changes: 5,
            created: 2,
            modified: 2,
            deleted: 1,
            renamed: 1,
            needs_before: 3,
            with_before: 3,
            unknown_provenance: 0,
            dropped: DroppedEvents::default(),
        }
    }

    fn linux_ok() -> PlatformHealth {
        PlatformHealth {
            os: "linux",
            check: "inotify watch headroom",
            status: Health::Ok,
            value: Some("65000 free".into()),
            detail: "536 of 65536 watches in use by this user".into(),
        }
    }

    #[test]
    fn snapshot_capture_report_plain() {
        let r = CaptureReport {
            session: Some(session()),
            platform: linux_ok(),
        };
        insta::assert_snapshot!(render_plain(&r, false));
    }

    /// Unknown coverage and dropped events: a dash, never 0%, and a ✗ line.
    #[test]
    fn snapshot_capture_report_plain_degraded() {
        let mut s = session();
        s.needs_before = 0;
        s.with_before = 0;
        s.unknown_provenance = 2;
        s.dropped = DroppedEvents {
            count: 4,
            first_error: Some("database or disk is full".into()),
        };
        let r = CaptureReport {
            session: Some(s),
            platform: PlatformHealth {
                os: "windows",
                check: "event-buffer overflow",
                status: Health::Unknown,
                value: None,
                detail: "overflow signals are not persisted".into(),
            },
        };
        let out = render_plain(&r, false);
        assert!(
            !out.contains("0%"),
            "unknown coverage must not render as 0%: {out}"
        );
        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_capture_report_no_session() {
        let r = CaptureReport {
            session: None,
            platform: linux_ok(),
        };
        insta::assert_snapshot!(render_plain(&r, false));
    }

    #[test]
    fn json_unknowns_are_null_not_zero() {
        let mut s = session();
        s.needs_before = 0;
        let j = to_json(&CaptureReport {
            session: Some(s),
            platform: linux_ok(),
        });
        assert!(j["before_side_coverage"]["percent"].is_null());
        assert!(j["events_live"].is_null());
        assert!(j["events_reconciled"].is_null());
        assert!(j["permission_denied"].is_null());
        assert_eq!(j["watched_roots"][0], "/home/me/work");
        assert_eq!(j["platform"]["status"], "ok");

        let none = to_json(&CaptureReport {
            session: None,
            platform: linux_ok(),
        });
        assert!(none["session"].is_null());
        assert!(none["before_side_coverage"].is_null());
    }

    #[test]
    fn pct_formatting() {
        assert_eq!(format_pct(100.0), "100%");
        assert_eq!(format_pct(87.5), "87.5%");
        assert_eq!(format_pct(66.666), "66.7%");
    }

    /// The inotify probe against a fixture `/proc` tree (real files in a
    /// temp dir; no mocks of the probe logic itself).
    #[test]
    fn inotify_probe_counts_watches_in_fixture_proc() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = tmp.path();
        std::fs::create_dir_all(proc.join("sys/fs/inotify")).unwrap();
        std::fs::write(proc.join("sys/fs/inotify/max_user_watches"), "2000\n").unwrap();
        std::fs::create_dir_all(proc.join("42/fdinfo")).unwrap();
        std::fs::write(
            proc.join("42/fdinfo/3"),
            "pos:\t0\nflags:\t00\ninotify wd:1 ino:2 sdev:3\ninotify wd:2 ino:4 sdev:3\n",
        )
        .unwrap();
        std::fs::write(proc.join("42/fdinfo/4"), "pos:\t0\nflags:\t02\n").unwrap();
        std::fs::create_dir_all(proc.join("self-not-a-pid/fdinfo")).unwrap();
        std::fs::write(proc.join("self-not-a-pid/fdinfo/1"), "inotify wd:9\n").unwrap();
        let h = inotify_health(proc);
        assert_eq!(h.status, Health::Ok);
        assert_eq!(h.value.as_deref(), Some("1998 free"));

        std::fs::write(proc.join("sys/fs/inotify/max_user_watches"), "100\n").unwrap();
        assert_eq!(inotify_health(proc).status, Health::Warn);

        let empty = tempfile::tempdir().unwrap();
        let h = inotify_health(empty.path());
        assert_eq!(h.status, Health::Unknown);
        assert_eq!(h.value, None);
    }
}
