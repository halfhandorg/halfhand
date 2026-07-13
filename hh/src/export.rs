//! `hh export` — serialize a session as a JSON bundle or a self-contained
//! HTML page (docs/redaction.md, docs/redaction-design.md enforcement
//! point 2).
//!
//! Exports are **always redacted by default**: the whole bundle is built as
//! one JSON tree and passed through a single redaction chokepoint
//! ([`hh_core::Detectors::redact_json`]) before a byte is written — both the
//! JSON and the HTML renderer consume the already-redacted tree, so there is
//! no code path that writes export output around the chokepoint. Sessions may
//! be recorded raw locally; nothing leaves the machine unredacted by
//! accident. Opting out requires `--no-redact` *plus* an interactive "yes";
//! a non-TTY stdin is refused outright.

use crate::cli;
use crate::render;
use hh_core::{SessionRow, Store};
use owo_colors::OwoColorize;
use std::fmt::Write as _;
use std::process::ExitCode;

/// `hh export [session] [--out FILE] [--html] [--no-redact]`.
pub(crate) fn export_command(args: &cli::ExportArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, config) = crate::open_store()?;
    let hint = args.session.as_deref().unwrap_or("last");
    let id = crate::resolve_session_arg(&store, hint)?;
    let session = store
        .get_session(&id)
        .map_err(|e| anyhow::anyhow!("could not load session\n  why: {e}"))?;

    if args.no_redact {
        confirm_unredacted_export(&session)?;
    }

    let mut bundle = build_bundle(&store, &session)?;
    // THE chokepoint (enforcement point 2): every string in the bundle passes
    // through the detectors before any output is rendered or written.
    if !args.no_redact {
        let detectors = crate::secrets::detectors(&config)?;
        let _ = detectors.redact_json(&mut bundle);
    }

    let output = if args.html {
        render_html(&bundle)
    } else {
        let mut s = serde_json::to_string_pretty(&bundle)
            .map_err(|e| anyhow::anyhow!("could not serialize the export bundle\n  why: {e}"))?;
        s.push('\n');
        s
    };

    if let Some(path) = &args.out {
        std::fs::write(path, &output).map_err(|e| {
            anyhow::anyhow!(
                "could not write export to {}\n  why: {e}\n  hint: check the directory exists and is writable",
                path.display()
            )
        })?;
        let color = render::use_color();
        let check = if color {
            "✓".green().to_string()
        } else {
            "✓".to_string()
        };
        let redacted = if args.no_redact {
            "UNREDACTED"
        } else {
            "redacted"
        };
        println!(
            "{check} Exported session {sid} → {path} ({fmt}, {redacted})",
            sid = session.short_id,
            path = path.display(),
            fmt = if args.html { "html" } else { "json" },
        );
    } else {
        use std::io::Write;
        // Pipe-safe: a downstream `head`/closed pager is a clean stop.
        if let Err(e) = std::io::stdout().write_all(output.as_bytes()) {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(anyhow::anyhow!("could not write export\n  why: {e}"));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Build the export bundle: schema-versioned session metadata plus every
/// event with its resolved body (blob overflows already inlined by
/// [`Store::for_each_event_detail`]). Materializes the session in memory —
/// an export is a whole-session artifact by definition.
fn build_bundle(store: &Store, session: &SessionRow) -> anyhow::Result<serde_json::Value> {
    let mut events: Vec<serde_json::Value> = Vec::new();
    store
        .for_each_event_detail(&session.id, |detail| {
            events.push(crate::inspect::event_to_json(&detail, session));
            Ok(())
        })
        .map_err(|e| anyhow::anyhow!("could not load session events\n  why: {e}"))?;
    Ok(serde_json::json!({
        "schema": crate::inspect::SCHEMA_VERSION,
        "kind": "hh-export",
        "hh_version": crate::HH_VERSION,
        "session": crate::session_to_json(session),
        "events": events,
    }))
}

/// Interactive gate for `--no-redact`: requires a TTY and a typed `yes` —
/// deliberately more friction than a `[y/N]`, and deliberately without a
/// `--yes`-style bypass, so no script can exfiltrate raw sessions.
fn confirm_unredacted_export(session: &SessionRow) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "refusing --no-redact without interactive confirmation\n  \
             why: stdin is not a TTY (piped or redirected); unredacted exports can leak \
             secrets recorded in prompts, tool output, and files\n  \
             hint: run `hh export {sid} --no-redact` in an interactive terminal, or drop \
             --no-redact — exports are redacted by default",
            sid = session.short_id
        );
    }
    let color = render::use_color_stderr();
    let prefix = if color {
        "●".yellow().to_string()
    } else {
        "●".to_string()
    };
    eprint!(
        "{prefix} Export session {sid} UNREDACTED? Any secrets recorded in prompts, tool \
         output, or files leave this machine as-is. Type 'yes' to continue: ",
        sid = session.short_id
    );
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| anyhow::anyhow!("could not read confirmation\n  why: {e}"))?;
    if line.trim().to_lowercase() != "yes" {
        anyhow::bail!("export cancelled (exports are redacted by default; drop --no-redact)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTML rendering (consumes the already-redacted bundle tree).
// ---------------------------------------------------------------------------

/// Escape text for HTML element content and attribute values.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// A string field of a JSON object, or an empty string.
fn s<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(serde_json::Value::as_str).unwrap_or("")
}

/// Render the bundle as one self-contained HTML page: session header, then
/// one block per step-bearing event (terminal chunks are elided — the page
/// is a shareable step report, not a byte-faithful replay). No external
/// assets, no scripts; restrained styling consistent with the CLI's "one
/// accent color" rule.
fn render_html(bundle: &serde_json::Value) -> String {
    let session = &bundle["session"];
    let events = bundle["events"].as_array().cloned().unwrap_or_default();
    let title = format!("hh session {}", s(session, "short_id"));
    let mut body = String::new();
    let _ = write!(
        body,
        "<h1>{title}</h1>\n<table class=\"meta\">\n",
        title = esc(&title)
    );
    for (label, key) in [
        ("status", "status"),
        ("agent", "agent_kind"),
        ("cwd", "cwd"),
    ] {
        let _ = writeln!(
            body,
            "<tr><th>{label}</th><td>{v}</td></tr>",
            v = esc(s(session, key))
        );
    }
    let command = session["command"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let _ = writeln!(
        body,
        "<tr><th>command</th><td><code>{}</code></td></tr>\n</table>",
        esc(&command)
    );

    for ev in &events {
        let kind = s(ev, "kind");
        if kind == "terminal_output" {
            continue;
        }
        let step = ev
            .get("step")
            .and_then(serde_json::Value::as_i64)
            .map_or_else(String::new, |n| format!("step {n} · "));
        let ts = ev
            .get("ts_ms")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let _ = writeln!(
            body,
            "<section class=\"event\">\n<h2><span class=\"kind\">{kind}</span> {step}{ts} ms</h2>\n<p>{summary}</p>",
            kind = esc(kind),
            step = esc(&step),
            summary = esc(s(ev, "summary")),
        );
        if let Some(b) = ev.get("body").filter(|b| !b.is_null()) {
            let pretty = serde_json::to_string_pretty(b).unwrap_or_default();
            let _ = writeln!(body, "<pre>{}</pre>", esc(&pretty));
        }
        let _ = writeln!(body, "</section>");
    }

    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n<style>\n\
         body {{ font: 15px/1.5 system-ui, sans-serif; max-width: 60rem; margin: 2rem auto; padding: 0 1rem; color: #1a1a1a; background: #fff; }}\n\
         h1 {{ font-size: 1.3rem; }}\n\
         h2 {{ font-size: 0.95rem; font-weight: 600; margin: 0 0 .3rem; }}\n\
         .kind {{ display: inline-block; padding: 0 .4em; border-radius: 3px; background: #0b7285; color: #fff; font-size: .8em; text-transform: uppercase; }}\n\
         .meta th {{ text-align: left; padding-right: 1rem; font-weight: 600; }}\n\
         .event {{ border-top: 1px solid #e0e0e0; padding: .8rem 0; }}\n\
         pre {{ background: #f6f6f6; padding: .6rem; overflow-x: auto; border-radius: 4px; font-size: .85em; }}\n\
         code {{ background: #f6f6f6; padding: 0 .25em; border-radius: 3px; }}\n\
         @media (prefers-color-scheme: dark) {{ body {{ color: #ddd; background: #111; }} pre, code {{ background: #1c1c1c; }} .event {{ border-color: #333; }} }}\n\
         </style>\n</head>\n<body>\n{body}\n<footer><p>exported by <code>hh export</code> — redaction tokens look like <code>{{{{REDACTED:&lt;type&gt;:&lt;hash8&gt;}}}}</code></p></footer>\n</body>\n</html>\n",
        title = esc(&title),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "kind": "hh-export",
            "hh_version": "test",
            "session": {
                "short_id": "a1b2c3",
                "status": "ok",
                "agent_kind": "generic",
                "cwd": "/tmp/work",
                "command": ["agent", "--flag"],
            },
            "events": [
                {
                    "kind": "user_message",
                    "step": 1,
                    "ts_ms": 12,
                    "summary": "hello <script>alert(1)</script>",
                    "body": { "text": "prompt with {{REDACTED:jwt:cafebabe}}" },
                },
                {
                    "kind": "terminal_output",
                    "step": null,
                    "ts_ms": 15,
                    "summary": "terminal output 64 bytes",
                    "body": null,
                },
            ],
        })
    }

    /// The HTML export is self-contained, escapes recorded content, elides
    /// terminal chunks, and carries the redaction-token legend.
    #[test]
    fn html_is_selfcontained_escaped_and_snapshot_locked() {
        let html = render_html(&bundle());
        insta::assert_snapshot!(html);
        assert!(html.contains("&lt;script&gt;"), "content must be escaped");
        assert!(!html.contains("<script>"), "no live script tags");
        assert!(!html.contains("terminal output 64 bytes"), "chunks elided");
        assert!(html.contains("REDACTED"), "legend present");
        assert!(
            !html.contains("http://") && !html.contains("https://"),
            "no external assets"
        );
    }
}
