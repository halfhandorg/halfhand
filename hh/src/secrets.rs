//! `hh scan` and `hh redact` — the secret-scanning and in-place redaction
//! commands (docs/redaction.md, docs/redaction-design.md).
//!
//! `hh scan` is read-only: it reports what the detectors find (type, step,
//! location, hash8 correlation tag) without ever printing the secret, and
//! exits **4** when findings exist (the documented redaction/policy exit
//! code) so CI can gate on a clean scan.
//!
//! `hh redact` rewrites a session in place and is irreversible: it prompts
//! for confirmation like `hh delete` (`--yes` to skip; refused outright on a
//! non-TTY stdin without `--yes`).

use crate::cli;
use crate::render;
use hh_core::{Detectors, ScanFinding, SessionRow};
use owo_colors::OwoColorize;
use std::fmt::Write as _;
use std::process::ExitCode;

/// Exit code for "findings exist" / redaction policy outcomes (CLAUDE.md
/// exit-code contract: 0 ok, 1 error, 2 usage, 3 session-not-found, 4
/// redaction/policy block).
pub(crate) const EXIT_FINDINGS: u8 = 4;

/// Build the detector set from the loaded config (custom rules + entropy
/// toggle apply to scan/redact/export exactly as they do at record time).
pub(crate) fn detectors(config: &hh_core::Config) -> anyhow::Result<Detectors> {
    Detectors::new(&config.redaction)
        .map_err(|e| anyhow::anyhow!("could not compile redaction detectors\n  why: {e}"))
}

/// `hh scan <session|last|--all> [--json]`.
pub(crate) fn scan_command(args: &cli::ScanArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, config) = crate::open_store()?;
    let detectors = detectors(&config)?;

    let sessions: Vec<SessionRow> = if args.all {
        store
            .list_sessions(u32::MAX)
            .map_err(|e| anyhow::anyhow!("could not list sessions\n  why: {e}"))?
    } else {
        let hint = args.session.as_deref().unwrap_or("last");
        let id = crate::resolve_session_arg(&store, hint)?;
        vec![store
            .get_session(&id)
            .map_err(|e| anyhow::anyhow!("could not load session\n  why: {e}"))?]
    };

    let mut scanned: Vec<(SessionRow, Vec<ScanFinding>)> = Vec::with_capacity(sessions.len());
    for session in sessions {
        let findings = store.scan_session(&session.id, &detectors).map_err(|e| {
            anyhow::anyhow!("could not scan session `{}`\n  why: {e}", session.short_id)
        })?;
        scanned.push((session, findings));
    }
    let total: u64 = scanned
        .iter()
        .flat_map(|(_, f)| f.iter().map(|x| x.count))
        .sum();

    if args.json {
        println!("{}", scan_json(&scanned, total));
    } else {
        print!(
            "{}",
            render_scan_plain(&scanned, total, render::use_color())
        );
    }
    Ok(if total > 0 {
        ExitCode::from(EXIT_FINDINGS)
    } else {
        ExitCode::SUCCESS
    })
}

/// Build the stable `hh scan --json` object (`schema: 1`, docs/json.md).
fn scan_json(scanned: &[(SessionRow, Vec<ScanFinding>)], total: u64) -> serde_json::Value {
    let sessions: Vec<serde_json::Value> = scanned
        .iter()
        .map(|(session, findings)| {
            let items: Vec<serde_json::Value> = findings
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "type": f.secret.to_string(),
                        "hash8": f.hash8,
                        "count": f.count,
                        "event_id": f.event_id,
                        "step": f.step,
                        "event_kind": f.event_kind.map(|k| k.to_string()),
                        "location": f.location.to_string(),
                    })
                })
                .collect();
            serde_json::json!({
                "id": session.id,
                "short_id": session.short_id,
                "findings": items,
            })
        })
        .collect();
    serde_json::json!({
        "schema": crate::inspect::SCHEMA_VERSION,
        "total_findings": total,
        "sessions": sessions,
    })
}

/// Render the human-readable scan report: one block per session with an
/// aligned findings table, or a single clean line. Factored out (color off)
/// for a snapshot test.
fn render_scan_plain(
    scanned: &[(SessionRow, Vec<ScanFinding>)],
    total: u64,
    color: bool,
) -> String {
    let mut out = String::new();
    let glyph = |g: &str, colored: String| if color { colored } else { g.to_string() };
    if total == 0 {
        let check = glyph("✓", "✓".green().to_string());
        let n = scanned.len();
        let _ = writeln!(
            out,
            "{check} no secrets detected in {n} session{s}",
            s = if n == 1 { "" } else { "s" }
        );
        return out;
    }
    for (session, findings) in scanned {
        if findings.is_empty() {
            continue;
        }
        let sid = if color {
            session.short_id.cyan().to_string()
        } else {
            session.short_id.clone()
        };
        let session_total: u64 = findings.iter().map(|f| f.count).sum();
        let _ = writeln!(
            out,
            "{mark} session {sid} — {session_total} secret occurrence(s)",
            mark = glyph("●", "●".yellow().to_string()),
        );
        // Aligned columns: TYPE, STEP, LOCATION, HASH8, COUNT. The hash8 tag
        // lets the user correlate one secret across rows without seeing it.
        let mut rows: Vec<[String; 5]> = Vec::with_capacity(findings.len());
        for f in findings {
            rows.push([
                f.secret.to_string(),
                f.step.map_or_else(|| "—".into(), |s| s.to_string()),
                f.location.to_string(),
                f.hash8.clone(),
                f.count.to_string(),
            ]);
        }
        let headers = ["TYPE", "STEP", "LOCATION", "HASH8", "COUNT"];
        let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        let mut header_line = String::from("  ");
        for (i, h) in headers.iter().enumerate() {
            let _ = write!(header_line, "{h:<w$}  ", w = widths[i]);
        }
        let _ = writeln!(out, "{}", header_line.trim_end());
        for row in &rows {
            let mut line = String::from("  ");
            for (i, cell) in row.iter().enumerate() {
                let _ = write!(line, "{cell:<w$}  ", w = widths[i]);
            }
            let _ = writeln!(out, "{}", line.trim_end());
        }
    }
    let _ = writeln!(
        out,
        "{cross} {total} secret occurrence(s) detected — `hh redact <session>` removes them irreversibly",
        cross = glyph("✗", "✗".red().to_string()),
    );
    out
}

/// `hh redact <session> [--yes]`.
pub(crate) fn redact_command(args: &cli::RedactArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, config) = crate::open_store()?;
    let detectors = detectors(&config)?;
    let id = crate::resolve_session_arg(&store, &args.session)?;
    let session = store
        .get_session(&id)
        .map_err(|e| anyhow::anyhow!("could not load session\n  why: {e}"))?;

    if !args.yes {
        confirm_redact(&session)?;
    }

    let outcome = store.redact_session(&id, &detectors).map_err(|e| {
        anyhow::anyhow!(
            "could not redact session `{}`\n  why: {e}",
            session.short_id
        )
    })?;
    print!(
        "{}",
        render_redact_plain(&session, &outcome, render::use_color())
    );
    Ok(ExitCode::SUCCESS)
}

/// Render the `hh redact` outcome. Factored out (color off) for a snapshot
/// test.
fn render_redact_plain(
    session: &SessionRow,
    outcome: &hh_core::RedactOutcome,
    color: bool,
) -> String {
    let mut out = String::new();
    let check = if color {
        "✓".green().to_string()
    } else {
        "✓".to_string()
    };
    let total: u64 = outcome.secrets.iter().map(|s| s.count).sum();
    let _ = writeln!(
        out,
        "{check} Redacted session {sid} · {total} secret occurrence(s) removed · \
         {events} event(s) rewritten · {blobs} blob(s) rewritten ({shredded} shredded)",
        sid = session.short_id,
        events = outcome.events_rewritten,
        blobs = outcome.blobs_rewritten,
        shredded = outcome.blobs_shredded,
    );
    for s in &outcome.secrets {
        let _ = writeln!(
            out,
            "  {kind:<20} {hash8}  ×{count}",
            kind = s.secret.to_string(),
            hash8 = s.hash8,
            count = s.count,
        );
    }
    if total == 0 {
        let _ = writeln!(out, "  (nothing detected — the session was already clean)");
    }
    out
}

/// Prompt for confirmation before an irreversible redact, mirroring
/// `hh delete`'s contract: a non-TTY stdin is refused with a pointer at
/// `--yes` so a script can never redact by accident.
fn confirm_redact(session: &SessionRow) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "refusing to redact without confirmation\n  \
             why: redaction is irreversible and stdin is not a TTY (piped or redirected), \
             so the confirmation prompt cannot be read\n  \
             hint: re-run with `hh redact {} --yes` to skip the prompt",
            session.short_id
        );
    }
    let color = render::use_color_stderr();
    let prefix = if color {
        "●".yellow().to_string()
    } else {
        "●".to_string()
    };
    eprint!(
        "{prefix} Irreversibly redact secrets from session {sid} ({status}, {steps} steps)? \
         Originals are destroyed. [y/N] ",
        sid = session.short_id,
        status = session.status,
        steps = session.step_count,
    );
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(|e| {
        anyhow::anyhow!(
            "could not read confirmation\n  why: {e}\n  hint: re-run with `hh redact {sid} --yes` to skip the prompt",
            sid = session.short_id
        )
    })?;
    let answer = line.trim().to_lowercase();
    if answer != "y" && answer != "yes" {
        anyhow::bail!("redact cancelled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hh_core::store::FindingLocation;
    use hh_core::{AdapterStatus, AgentKind, EventKind, SecretKind, SessionStatus};
    use std::path::PathBuf;

    fn session(short: &str) -> SessionRow {
        SessionRow {
            id: format!("00000000-0000-7000-8000-{short}000000"),
            short_id: short.to_string(),
            started_at: 1_782_052_800_000,
            ended_at: Some(1_782_053_120_000),
            exit_code: Some(0),
            status: SessionStatus::Ok,
            agent_kind: AgentKind::Generic,
            adapter_status: AdapterStatus::None,
            command: vec!["agent".into()],
            cwd: PathBuf::from("/tmp/work"),
            step_count: 12,
            files_changed: 2,
            imported_from: None,
        }
    }

    /// `hh scan` plain output: the per-session block with an aligned findings
    /// table and the exit-4 trailer, plus the clean-store one-liner.
    /// color=false keeps the snapshot deterministic and pipe-safe.
    #[test]
    fn scan_plain_renders_findings_table_and_clean_line() {
        let findings = vec![
            ScanFinding {
                event_id: Some(7),
                step: Some(3),
                event_kind: Some(EventKind::ToolResult),
                location: FindingLocation::Body,
                secret: SecretKind::AwsAccessKeyId,
                hash8: "a1b2c3d4".into(),
                count: 2,
            },
            ScanFinding {
                event_id: Some(9),
                step: Some(5),
                event_kind: Some(EventKind::FileChange),
                location: FindingLocation::Blob {
                    hash: "ff00".repeat(16),
                    path: Some(".env".into()),
                },
                secret: SecretKind::GithubToken,
                hash8: "deadbeef".into(),
                count: 1,
            },
            ScanFinding {
                event_id: None,
                step: None,
                event_kind: None,
                location: FindingLocation::Command,
                secret: SecretKind::Custom("acme".into()),
                hash8: "12345678".into(),
                count: 1,
            },
        ];
        let scanned = vec![(session("a1b2c3"), findings)];
        let out = render_scan_plain(&scanned, 4, false);
        insta::assert_snapshot!(out);
        assert!(out.contains("aws-access-key-id"));
        assert!(out.contains("file .env"));
        assert!(out.contains("custom:acme"));
        assert!(out.contains("hh redact"));
        assert!(!out.contains('\x1b'));

        let clean = render_scan_plain(&[(session("d4e5f6"), vec![])], 0, false);
        insta::assert_snapshot!(clean);
        assert!(clean.contains("no secrets detected"));
    }

    /// `hh redact` plain output: the summary line plus per-secret tallies.
    #[test]
    fn redact_plain_renders_summary_and_tallies() {
        let outcome = hh_core::RedactOutcome {
            events_rewritten: 5,
            blobs_rewritten: 2,
            blobs_shredded: 2,
            secrets: vec![
                hh_core::SecretSummary {
                    secret: SecretKind::AwsAccessKeyId,
                    hash8: "a1b2c3d4".into(),
                    count: 3,
                },
                hh_core::SecretSummary {
                    secret: SecretKind::PrivateKey,
                    hash8: "deadbeef".into(),
                    count: 1,
                },
            ],
            audit_event_id: 42,
        };
        let out = render_redact_plain(&session("a1b2c3"), &outcome, false);
        insta::assert_snapshot!(out);
        assert!(out.contains("4 secret occurrence(s) removed"));
        assert!(out.contains("2 shredded"));
        assert!(!out.contains('\x1b'));
    }

    /// `hh scan --json` object shape (schema-versioned, docs/json.md).
    #[test]
    fn scan_json_shape() {
        let findings = vec![ScanFinding {
            event_id: Some(7),
            step: Some(3),
            event_kind: Some(EventKind::ToolResult),
            location: FindingLocation::Summary,
            secret: SecretKind::Jwt,
            hash8: "cafebabe".into(),
            count: 1,
        }];
        let v = scan_json(&[(session("a1b2c3"), findings)], 1);
        assert_eq!(v["schema"], 1);
        assert_eq!(v["total_findings"], 1);
        assert_eq!(v["sessions"][0]["short_id"], "a1b2c3");
        let f = &v["sessions"][0]["findings"][0];
        assert_eq!(f["type"], "jwt");
        assert_eq!(f["hash8"], "cafebabe");
        assert_eq!(f["location"], "summary");
        assert_eq!(f["step"], 3);
    }
}
