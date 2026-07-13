//! `hh search` — FTS5 full-text search over recorded sessions.
//!
//! Searches event summaries and message bodies across all sessions using
//! SQLite FTS5. Results show the session, step, event kind, and a highlighted
//! snippet with `<b>` markers around matching terms.

use std::process::ExitCode;

use hh_core::event::{AgentKind, EventKind};
use hh_core::store::{SearchFilters, SearchResult};

use crate::cli;
use crate::render;

/// Run `hh search <query>` (FTS5 full-text search).
pub fn search_command(args: &cli::SearchArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, _config) = crate::open_store()?;

    let agent_kind = args
        .agent
        .as_deref()
        .map(str::parse::<AgentKind>)
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid agent kind: {e}"))?;

    let event_kind = args
        .kind
        .as_deref()
        .map(str::parse::<EventKind>)
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid event kind: {e}"))?;

    let filters = SearchFilters {
        agent_kind,
        event_kind,
        since: args.since,
        path: args.path.clone(),
        limit: args.limit,
    };

    let results = store
        .search(&args.query, &filters)
        .map_err(|e| anyhow::anyhow!("search failed\n  why: {e}"))?;

    if args.json {
        let arr: Vec<serde_json::Value> = results.iter().map(search_result_to_json).collect();
        println!("{}", serde_json::Value::Array(arr));
    } else {
        print_search_table(&results, render::use_color());
    }

    Ok(ExitCode::SUCCESS)
}

/// Build the JSON object for one search result.
fn search_result_to_json(r: &SearchResult) -> serde_json::Value {
    serde_json::json!({
        "event_id": r.event_id,
        "session_id": r.session_id,
        "session_short_id": r.session_short_id,
        "step": r.step,
        "kind": r.kind.to_string(),
        "summary": r.summary,
        "snippet": r.snippet,
        "ts_ms": r.ts_ms,
    })
}

/// Render search results as an aligned table.
fn print_search_table(results: &[SearchResult], color: bool) {
    if results.is_empty() {
        println!("no results found");
        return;
    }
    // Build cell strings.
    let mut lines: Vec<SearchLine> = Vec::with_capacity(results.len());
    for r in results {
        let sid = if color {
            r.session_short_id.as_str().into()
        } else {
            r.session_short_id.clone()
        };
        let step = r.step.map_or_else(|| "—".to_string(), |s| s.to_string());
        let kind = r.kind.to_string();
        // Strip <b> tags for plain output, keep for color
        let snippet = if color {
            r.snippet
                .replace("<b>", "\x1b[1m")
                .replace("</b>", "\x1b[0m")
        } else {
            r.snippet.replace("<b>", "").replace("</b>", "")
        };
        lines.push(SearchLine {
            session: sid,
            step,
            kind,
            snippet,
        });
    }

    let headers = ["SESSION", "STEP", "KIND", "SNIPPET"];
    let widths = compute_search_widths(&headers, &lines);
    let last = headers.len() - 1;

    let mut out = String::new();
    for (i, h) in headers.iter().enumerate() {
        if i == last {
            out.push_str(h);
        } else {
            pad_into(&mut out, h, widths[i]);
        }
    }
    out.push('\n');
    for (i, &w) in widths.iter().enumerate() {
        for _ in 0..w {
            out.push('-');
        }
        if i != last {
            out.push(' ');
        }
    }
    out.push('\n');
    for l in &lines {
        pad_into(&mut out, &l.session, widths[0]);
        pad_into(&mut out, &l.step, widths[1]);
        pad_into(&mut out, &l.kind, widths[2]);
        out.push_str(&l.snippet);
        out.push('\n');
    }
    print!("{out}");
}

/// A row's rendered cells for the search table.
struct SearchLine {
    session: String,
    step: String,
    kind: String,
    snippet: String,
}

/// Compute column widths for the search table.
fn compute_search_widths(headers: &[&str; 4], lines: &[SearchLine]) -> [usize; 4] {
    let cell = |l: &SearchLine, i: usize| -> usize {
        match i {
            0 => render::visible_width(&l.session),
            1 => render::visible_width(&l.step),
            2 => render::visible_width(&l.kind),
            _ => render::visible_width(&l.snippet),
        }
    };
    let mut widths = [0usize; 4];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.chars().count();
        for l in lines {
            widths[i] = widths[i].max(cell(l, i));
        }
    }
    widths
}

/// Append `s` to `out` left-justified in `width` plus one trailing space.
fn pad_into(out: &mut String, s: &str, width: usize) {
    out.push_str(s);
    let pad = width.saturating_sub(render::visible_width(s));
    for _ in 0..pad {
        out.push(' ');
    }
    out.push(' ');
}
