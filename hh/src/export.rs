//! `hh export` — serialize a session as a JSON bundle, a portable `.hh`
//! archive, or a self-contained HTML page (docs/redaction.md,
//! docs/redaction-design.md enforcement point 2).
//!
//! Exports are **always redacted by default**. The JSON/HTML paths build the
//! whole bundle as one JSON tree and pass it through a single redaction
//! chokepoint ([`hh_core::Detectors::redact_json`]) before a byte is
//! written; the `--bundle` path passes the same `Detectors` into
//! [`hh_core::bundle::export`], which applies the identical chokepoint (plus
//! blob-content redaction) internally. No code path writes export output
//! around a chokepoint. Sessions may be recorded raw locally; nothing leaves
//! the machine unredacted by accident. Opting out requires `--no-redact`
//! *plus* an interactive "yes"; a non-TTY stdin is refused outright.

use crate::cli;
use crate::render;
use hh_core::{SessionRow, Store};
use owo_colors::OwoColorize;
use std::process::ExitCode;

/// The bytes an export produced, in whichever representation the requested
/// format naturally is — JSON/HTML are always valid UTF-8 text, the portable
/// bundle is a binary zstd stream. Keeping both variants (rather than
/// forcing everything through `String`) lets the file/stdout writer below be
/// one binary-safe path for all three formats.
enum ExportOutput {
    /// JSON or HTML — written as UTF-8 text.
    Text(String),
    /// The `--bundle` archive — written as raw bytes.
    Bytes(Vec<u8>),
}

impl ExportOutput {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Text(s) => s.as_bytes(),
            Self::Bytes(b) => b,
        }
    }
}

/// `hh export [session] [--out FILE] [--html | --bundle] [--no-redact]`.
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
    if args.bundle && args.out.is_none() {
        refuse_binary_to_tty()?;
    }

    let output = if args.bundle {
        let detectors = if args.no_redact {
            None
        } else {
            Some(crate::secrets::detectors(&config)?)
        };
        let bytes = hh_core::bundle::export(&store, &id, crate::HH_VERSION, detectors.as_ref())
            .map_err(|e| anyhow::anyhow!("could not build export bundle\n  why: {e}"))?;
        ExportOutput::Bytes(bytes)
    } else if args.html {
        let detectors = if args.no_redact {
            None
        } else {
            Some(crate::secrets::detectors(&config)?)
        };
        ExportOutput::Text(build_redacted_html(&store, &session, detectors.as_ref())?)
    } else {
        let mut bundle = build_bundle(&store, &session)?;
        // THE chokepoint (enforcement point 2): every string in the bundle
        // passes through the detectors before any output is rendered or
        // written.
        if !args.no_redact {
            let detectors = crate::secrets::detectors(&config)?;
            let _ = detectors.redact_json(&mut bundle);
        }
        let mut s = serde_json::to_string_pretty(&bundle)
            .map_err(|e| anyhow::anyhow!("could not serialize the export bundle\n  why: {e}"))?;
        s.push('\n');
        ExportOutput::Text(s)
    };

    let format = if args.bundle {
        "bundle"
    } else if args.html {
        "html"
    } else {
        "json"
    };

    if let Some(path) = &args.out {
        std::fs::write(path, output.as_bytes()).map_err(|e| {
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
            "{check} Exported session {sid} → {path} ({format}, {redacted})",
            sid = session.short_id,
            path = path.display(),
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

/// Refuse `--bundle` with no `-o` on an interactive stdout: the bundle is a
/// binary zstd stream, and spewing it into a terminal is never what the user
/// wants (same spirit as [`confirm_unredacted_export`]'s TTY guard).
fn refuse_binary_to_tty() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        anyhow::bail!(
            "refusing to write a binary bundle to your terminal\n  \
             why: `--bundle` produces a zstd-compressed archive, not text\n  \
             hint: add `-o FILE` (e.g. `hh export last --bundle -o session.hh`), or redirect stdout to a file"
        );
    }
    Ok(())
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

/// Build the self-contained HTML replay page for `session`, optionally
/// redacted through `detectors` (`None` skips redaction — only reachable via
/// `export --html --no-redact`'s interactive confirmation gate; `hh replay
/// --web` always passes `Some`, since it has no `--no-redact` surface).
/// Shared by `export --html` and `hh replay --web` so the two produce
/// byte-identical pages for the same session.
pub(crate) fn build_redacted_html(
    store: &Store,
    session: &SessionRow,
    detectors: Option<&hh_core::Detectors>,
) -> anyhow::Result<String> {
    let mut payload = build_html_payload(store, session)?;
    if let Some(d) = detectors {
        let _ = d.redact_json(&mut payload);
    }
    Ok(render_html(&payload))
}

/// Build the JSON payload embedded in the HTML replay page: session
/// metadata plus every *step-bearing* event (`terminal_output` is elided,
/// matching the replay TUI's default `show_terminal = false` — this is a
/// step report, not a byte-faithful terminal transcript). Each `file_change`
/// event additionally carries precomputed diff hunks
/// ([`crate::inspect::file_change_diff_json`]), so the page's client-side JS
/// never needs to implement a diff algorithm.
fn build_html_payload(store: &Store, session: &SessionRow) -> anyhow::Result<serde_json::Value> {
    let mut events: Vec<serde_json::Value> = Vec::new();
    store
        .for_each_event_detail(&session.id, |detail| {
            if detail.kind == hh_core::EventKind::TerminalOutput {
                return Ok(());
            }
            let mut ev = crate::inspect::event_to_json(&detail, session);
            if let Some(fc) = &detail.file_change {
                if let Some(obj) = ev.as_object_mut() {
                    obj.insert(
                        "diff".into(),
                        crate::inspect::file_change_diff_json(fc, store),
                    );
                }
            }
            events.push(ev);
            Ok(())
        })
        .map_err(|e| anyhow::anyhow!("could not load session events\n  why: {e}"))?;
    Ok(serde_json::json!({
        "schema": crate::inspect::SCHEMA_VERSION,
        "kind": "hh-export-html",
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
// HTML rendering: a self-contained interactive replay page.
//
// Safety model (the whole page's session data is untrusted input — it is
// whatever the recorded agent/tool output happened to contain):
//   1. The payload is embedded as JSON inside a single
//      `<script type="application/json">` tag, run through
//      `safe_json_for_script` first — that neutralizes `<`/`>`/`&` so a
//      `</script>` substring can never appear literally, which is the only
//      way raw text inside a `<script>` element could end it early.
//   2. Client-side JS reads that tag's `textContent` (never `innerHTML`) and
//      `JSON.parse`s it, then builds every other DOM node with
//      `createElement`/`textContent`. Nothing recorded is ever assigned to
//      `innerHTML`, so nothing recorded can ever be interpreted as markup.
//   3. The *only* other server-interpolated text in the page is the escaped
//      `<title>` — everything else is static chrome (CSS + hand-written JS
//      with no data placeholders inside it).
// ---------------------------------------------------------------------------

/// Escape text for HTML element content and attribute values. Used only for
/// the page `<title>` — every other piece of session data reaches the page
/// through [`safe_json_for_script`] instead.
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

/// Serialize `value` to compact JSON, then neutralize every `<`, `>`, and
/// `&` as a `\uXXXX` escape. Those three bytes are the only ones that matter
/// inside a `<script>` element's raw-text content — the HTML parser ends the
/// element the instant it sees a literal `</script` substring, no matter how
/// deep inside a JSON string it sits. `\uXXXX` is valid anywhere inside a
/// JSON string, so this transform is always safe (it can never occur
/// *outside* a string in output `serde_json` produces, since `<`/`>`/`&` are
/// not JSON structural characters) and always sufficient (no `<`, so no
/// `</script`, so no premature close — regardless of what the session
/// recorded).
fn safe_json_for_script(value: &serde_json::Value) -> String {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    json.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

/// Render the payload as one self-contained, interactive HTML page: a
/// clickable step timeline, a detail pane (pretty JSON / diff viewer /
/// plain text depending on event kind), `j`/`k` keyboard navigation
/// mirroring the replay TUI, and a dark-by-default theme with a light
/// toggle. No external assets, no network requests, no build step — the JS
/// below is hand-written vanilla JS, and it is the *only* code that ever
/// touches the recorded session data (see the module-level safety notes).
fn render_html(payload: &serde_json::Value) -> String {
    let short_id = payload["session"]["short_id"].as_str().unwrap_or("session");
    let title = format!("hh session {short_id}");
    let data = safe_json_for_script(payload);

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
{css}
</style>
</head>
<body>
<noscript>This replay page needs JavaScript to render — the session data below is otherwise inert.</noscript>
<script type="application/json" id="hh-data">{data}</script>
<button id="hh-theme-toggle" type="button" aria-label="Toggle light/dark theme">◐</button>
<div id="app">
  <header id="hh-header"></header>
  <div id="hh-main">
    <nav id="hh-timeline" aria-label="Step timeline"></nav>
    <main id="hh-detail" aria-label="Step detail"></main>
  </div>
</div>
<footer>exported by <code>hh export --html</code> — redaction tokens look like <code>{{{{REDACTED:&lt;type&gt;:&lt;hash8&gt;}}}}</code> · j/k to navigate</footer>
<script>
{js}
</script>
</body>
</html>
"#,
        title = esc(&title),
        css = HTML_CSS,
        data = data,
        js = HTML_JS,
    )
}

/// CSS custom properties: dark by default (not tied to
/// `prefers-color-scheme` — the toggle button is the only way to switch),
/// one accent color for chrome (the focused/selected timeline row, the
/// diff-hunk header), and the replay TUI's own six kind-badge hues
/// (`hh/src/replay/kind.rs`: AGENT cyan, USER green, TOOL yellow, MCP
/// magenta, FILE blue, ERR red) so the page reads as the same visual system
/// as `hh replay`.
const HTML_CSS: &str = r#"
:root {
  --bg: #0b0d10; --panel: #12151a; --border: #23262b; --fg: #e6e6e6; --dim: #8a8f98;
  --accent: #22d3c8;
  --agent: #4fd1e0; --user: #52c41a; --tool: #e0b341; --mcp: #d16fe0; --file: #4f8ef0; --err: #ff6b6b;
}
html[data-theme="light"] {
  --bg: #ffffff; --panel: #f5f6f7; --border: #e2e4e8; --fg: #1a1a1a; --dim: #6b7280;
  --accent: #0b7285;
  --agent: #0891b2; --user: #15803d; --tool: #a16207; --mcp: #a21caf; --file: #1d4ed8; --err: #b91c1c;
}
* { box-sizing: border-box; }
html, body { height: 100%; margin: 0; }
body {
  font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace;
  font-size: 14px; line-height: 1.5; background: var(--bg); color: var(--fg);
}
#app { display: flex; flex-direction: column; height: 100vh; }
#hh-header { flex: 0 0 auto; padding: .75rem 1rem; border-bottom: 1px solid var(--border); }
.hh-title { display: flex; gap: .6rem; align-items: baseline; flex-wrap: wrap; font-size: 1rem; }
.hh-id { color: var(--accent); font-weight: 600; }
.hh-status, .hh-agent { color: var(--dim); font-size: .8rem; text-transform: uppercase; }
.hh-command, .hh-meta { margin-top: .25rem; color: var(--dim); font-size: .8rem; overflow-wrap: anywhere; }
#hh-theme-toggle {
  position: fixed; top: .6rem; right: .8rem; z-index: 1;
  background: var(--panel); color: var(--fg); border: 1px solid var(--border);
  border-radius: 4px; padding: .3rem .6rem; cursor: pointer; font: inherit;
}
#hh-main { flex: 1 1 auto; min-height: 0; display: flex; }
#hh-timeline { flex: 0 0 auto; width: 26rem; max-width: 42vw; overflow-y: auto; border-right: 1px solid var(--border); }
.hh-row { display: flex; gap: .5rem; align-items: baseline; padding: .3rem .6rem; border-bottom: 1px solid var(--border); cursor: pointer; }
.hh-row:hover { background: var(--panel); }
.hh-row.selected { background: var(--panel); box-shadow: inset 3px 0 0 var(--accent); }
.hh-step { flex: 0 0 auto; width: 2.4em; text-align: right; color: var(--dim); }
.hh-badge { flex: 0 0 auto; font-size: .7em; text-transform: uppercase; letter-spacing: .02em; }
.hh-badge-AGENT { color: var(--agent); } .hh-badge-USER { color: var(--user); }
.hh-badge-TOOL { color: var(--tool); } .hh-badge-MCP { color: var(--mcp); }
.hh-badge-FILE { color: var(--file); } .hh-badge-ERR { color: var(--err); }
.hh-badge-LIFE { color: var(--dim); }
.hh-summary { flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.hh-time { flex: 0 0 auto; color: var(--dim); font-size: .8em; }
#hh-detail { flex: 1 1 auto; overflow-y: auto; padding: 1rem 1.25rem; }
.hh-detail-head { display: flex; gap: .6rem; align-items: baseline; margin-bottom: .7rem; }
.hh-empty { color: var(--dim); }
.hh-text, .hh-json {
  margin: 0; padding: .7rem .8rem; background: var(--panel); border: 1px solid var(--border);
  border-radius: 4px; white-space: pre-wrap; word-break: break-word;
}
.hh-diff-path { color: var(--dim); margin-bottom: .4rem; }
.hh-diff-note { color: var(--dim); font-style: italic; }
.hh-diff-hunk { color: var(--accent); margin-top: .5rem; }
.hh-diff-line { white-space: pre-wrap; word-break: break-word; }
.hh-diff-insert { color: var(--user); } .hh-diff-delete { color: var(--err); }
footer { flex: 0 0 auto; padding: .5rem 1rem; color: var(--dim); font-size: .75em; border-top: 1px solid var(--border); }
noscript { display: block; padding: 1rem; color: var(--dim); }
"#;

/// Hand-written, dependency-free JS: reads the `#hh-data` script tag's
/// `textContent` (never `innerHTML`), `JSON.parse`s it, and builds the
/// timeline/detail DOM entirely via `createElement`/`textContent`. See the
/// module-level safety notes for why that makes the recorded session data
/// (however hostile) inert as markup.
const HTML_JS: &str = r"
(function () {
  'use strict';
  var raw = document.getElementById('hh-data').textContent;
  var DATA = JSON.parse(raw);
  var events = DATA.events || [];

  var KIND_LABEL = {
    user_message: 'USER', agent_message: 'AGENT', thinking: 'AGENT',
    tool_call: 'TOOL', tool_result: 'TOOL',
    mcp_request: 'MCP', mcp_response: 'MCP', mcp_notification: 'MCP',
    file_change: 'FILE', error: 'ERR', lifecycle: 'LIFE'
  };
  var TEXT_KINDS = ['user_message', 'agent_message', 'thinking', 'lifecycle'];

  function el(tag, className, text) {
    var e = document.createElement(tag);
    if (className) e.className = className;
    if (text !== undefined && text !== null) e.textContent = text;
    return e;
  }

  function badgeClass(kind) {
    return 'hh-badge hh-badge-' + (KIND_LABEL[kind] || 'LIFE');
  }

  function fmtTime(ms) {
    var total = Math.max(0, Math.floor((ms || 0) / 1000));
    var h = Math.floor(total / 3600);
    var m = Math.floor((total % 3600) / 60);
    var s = total % 60;
    function pad(n) { return (n < 10 ? '0' : '') + n; }
    return (h > 0 ? h + ':' : '') + pad(m) + ':' + pad(s);
  }

  function renderHeader() {
    var header = document.getElementById('hh-header');
    header.textContent = '';
    var sess = DATA.session || {};
    var title = el('div', 'hh-title');
    title.appendChild(el('span', 'hh-id', sess.short_id || ''));
    title.appendChild(el('span', 'hh-status', sess.status || ''));
    title.appendChild(el('span', 'hh-agent', sess.agent_kind || ''));
    header.appendChild(title);
    var cmd = Array.isArray(sess.command) ? sess.command.join(' ') : '';
    header.appendChild(el('div', 'hh-command', cmd));
    var metaText = sess.cwd || '';
    if (sess.imported_from) metaText += '  ·  imported from ' + sess.imported_from;
    header.appendChild(el('div', 'hh-meta', metaText));
  }

  var rows = [];
  var selected = -1;

  function renderTimeline() {
    var nav = document.getElementById('hh-timeline');
    nav.textContent = '';
    rows = [];
    events.forEach(function (ev, i) {
      var row = el('div', 'hh-row');
      if (ev.step !== null && ev.step !== undefined) {
        row.appendChild(el('span', 'hh-step', String(ev.step)));
      }
      row.appendChild(el('span', badgeClass(ev.kind), KIND_LABEL[ev.kind] || ev.kind));
      row.appendChild(el('span', 'hh-summary', ev.summary || ''));
      row.appendChild(el('span', 'hh-time', fmtTime(ev.ts_ms)));
      row.addEventListener('click', function () { select(i); });
      nav.appendChild(row);
      rows.push(row);
    });
  }

  function select(i) {
    if (i < 0 || i >= rows.length) return;
    if (selected >= 0 && rows[selected]) rows[selected].classList.remove('selected');
    selected = i;
    rows[selected].classList.add('selected');
    rows[selected].scrollIntoView({ block: 'nearest' });
    renderDetail(events[selected]);
  }

  function jsonBlock(container, value) {
    var pre = el('pre', 'hh-json');
    pre.textContent = JSON.stringify(value, null, 2);
    container.appendChild(pre);
  }

  function diffBlock(container, diff) {
    var wrap = el('div', 'hh-diff');
    wrap.appendChild(el('div', 'hh-diff-path', diff.path + ' (' + diff.change_kind + ')'));
    if (diff.note) wrap.appendChild(el('div', 'hh-diff-note', diff.note));
    (diff.hunks || []).forEach(function (hunk) {
      if (hunk.header) wrap.appendChild(el('div', 'hh-diff-hunk', hunk.header));
      (hunk.lines || []).forEach(function (line) {
        var prefix = line.tag === 'insert' ? '+' : (line.tag === 'delete' ? '-' : ' ');
        wrap.appendChild(el('div', 'hh-diff-line hh-diff-' + line.tag, prefix + line.text));
      });
    });
    container.appendChild(wrap);
  }

  function renderDetail(ev) {
    var main = document.getElementById('hh-detail');
    main.textContent = '';
    if (!ev) return;
    var head = el('div', 'hh-detail-head');
    head.appendChild(el('span', badgeClass(ev.kind), KIND_LABEL[ev.kind] || ev.kind));
    head.appendChild(el('span', null, ev.summary || ''));
    main.appendChild(head);

    if (ev.kind === 'file_change' && ev.diff) {
      diffBlock(main, ev.diff);
    } else if (ev.body !== null && ev.body !== undefined) {
      var isText = TEXT_KINDS.indexOf(ev.kind) !== -1 &&
        typeof ev.body === 'object' && typeof ev.body.text === 'string';
      if (isText) {
        main.appendChild(el('pre', 'hh-text', ev.body.text));
      } else {
        jsonBlock(main, ev.body);
      }
    }
  }

  document.addEventListener('keydown', function (e) {
    var t = e.target;
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA')) return;
    if (e.key === 'j' || e.key === 'ArrowDown') { e.preventDefault(); select(Math.min(selected + 1, rows.length - 1)); }
    else if (e.key === 'k' || e.key === 'ArrowUp') { e.preventDefault(); select(Math.max(selected - 1, 0)); }
  });

  function applyTheme(t) {
    document.documentElement.setAttribute('data-theme', t);
  }
  function initTheme() {
    var saved = null;
    try { saved = window.localStorage.getItem('hh-theme'); } catch (err) { saved = null; }
    applyTheme(saved === 'light' ? 'light' : 'dark');
  }
  var toggle = document.getElementById('hh-theme-toggle');
  toggle.addEventListener('click', function () {
    var next = document.documentElement.getAttribute('data-theme') === 'light' ? 'dark' : 'light';
    applyTheme(next);
    try { window.localStorage.setItem('hh-theme', next); } catch (err) { /* file:// or blocked storage */ }
  });

  initTheme();
  renderHeader();
  renderTimeline();
  if (rows.length > 0) {
    select(0);
  } else {
    document.getElementById('hh-detail').appendChild(el('div', 'hh-empty', 'no steps in this session'));
  }
})();
";

#[cfg(test)]
mod tests {
    use super::*;
    use hh_core::{
        AdapterStatus, AgentKind, ChangeKind, Event, EventKind, FileChange, NewSession,
        SessionStatus, Store,
    };
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A payload carrying `<script>`/event-handler-shaped content in every
    /// text field `render_html` touches (a step's summary, its body text,
    /// and — new in this design — a `file_change`'s diff path and diff line
    /// text) — the full XSS surface this page must neutralize.
    fn payload() -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "kind": "hh-export-html",
            "hh_version": "test",
            "session": {
                "short_id": "a1b2c3",
                "status": "ok",
                "agent_kind": "generic",
                "cwd": "/tmp/work",
                "command": ["agent", "--flag"],
                "imported_from": null,
            },
            "events": [
                {
                    "kind": "user_message",
                    "step": 1,
                    "ts_ms": 12,
                    "summary": "hello <script>alert(1)</script>",
                    "body": { "text": "prompt with </script><img src=x onerror=alert(2)>" },
                },
                {
                    "kind": "file_change",
                    "step": 2,
                    "ts_ms": 20,
                    "summary": "<svg onload=alert(3)>.txt created",
                    "body": { "path": "<svg onload=alert(3)>.txt" },
                    "diff": {
                        "path": "<svg onload=alert(3)>.txt",
                        "change_kind": "created",
                        "is_binary": false,
                        "note": null,
                        "hunks": [
                            {
                                "header": null,
                                "lines": [
                                    { "tag": "insert", "text": "</script><script>alert(4)</script>" }
                                ],
                            }
                        ],
                    },
                },
            ],
        })
    }

    /// The HTML replay page is self-contained, and every XSS surface
    /// (summary/body/diff path/diff line text) is inert: recorded content
    /// only ever appears inside the `#hh-data` JSON blob, safely escaped —
    /// never as live markup in the static chrome or hand-written JS around
    /// it. Being *inside* the JSON blob is not itself dangerous (it is
    /// `<script type="application/json">` text, parsed only as
    /// `JSON.parse`d data, never as HTML/JS) — the property that matters is
    /// that a literal `</script` can never occur there (it would end the
    /// tag early) and that none of the payload leaks *outside* the blob.
    #[test]
    fn html_is_selfcontained_and_neutralizes_every_xss_surface() {
        let html = render_html(&payload());
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("id=\"hh-theme-toggle\""));
        assert!(html.contains("j/k to navigate"));
        assert!(
            !html.contains("http://") && !html.contains("https://"),
            "no external assets"
        );

        // Isolate the `#hh-data` script tag's content — the one place
        // recorded session data ever appears in the document.
        let marker = "id=\"hh-data\">";
        let data_start = html.find(marker).expect("data script tag present") + marker.len();
        let close_offset = html[data_start..]
            .find("</script>")
            .expect("data script tag closes");
        let before = &html[..data_start];
        let data = &html[data_start..data_start + close_offset];
        let after = &html[data_start + close_offset..];

        // Inside the blob: the escaped form is present, and no literal
        // `<`-headed tag or `</script` ever occurs (which is what makes it
        // safe to embed regardless of content).
        assert!(
            data.contains("\\u003cscript\\u003e"),
            "escaped script tag inside the data blob: {data}"
        );
        assert!(
            data.contains("\\u003cimg"),
            "escaped img tag inside the data blob: {data}"
        );
        assert!(
            data.contains("\\u003csvg"),
            "escaped svg tag inside the data blob: {data}"
        );
        assert!(
            !data.contains('<'),
            "no literal `<` inside the data blob: {data}"
        );

        // Outside the blob (static chrome + hand-written JS, which has no
        // data placeholders): none of the payload substrings ever appear —
        // they must exist only as inert JSON text, never as markup.
        for needle in [
            "<script>alert",
            "onerror=alert",
            "onload=alert",
            "<img",
            "<svg",
        ] {
            assert!(
                !before.contains(needle) && !after.contains(needle),
                "payload leaked outside the data blob as live markup: {needle}"
            );
        }
        assert_eq!(
            html.matches("</script>").count(),
            2,
            "only the two page-owned script tags may close with </script>: {html}"
        );

        insta::assert_snapshot!(html);
    }

    /// `build_html_payload` (the server-side half) elides `terminal_output`
    /// from the timeline and attaches precomputed diff hunks to
    /// `file_change` events, so the client never implements a diff
    /// algorithm.
    #[test]
    fn build_html_payload_elides_terminal_output_and_carries_diffs() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let created = store
            .create_session(&NewSession {
                id: hh_core::Uuid::parse_str("018e2c5a-4a00-7000-8000-000000a1b2c3").unwrap(),
                started_at: 0,
                agent_kind: AgentKind::Generic,
                adapter_status: AdapterStatus::None,
                command: vec!["agent".into()],
                cwd: PathBuf::from("/tmp/work"),
                hostname: None,
                hh_version: "test".into(),
                model: None,
                git_branch: None,
                git_sha: None,
                git_dirty: None,
            })
            .unwrap();
        let sid = created.id.clone();
        let writer = store.event_writer().unwrap();
        writer
            .append_event(Event {
                session_id: sid.clone(),
                ts_ms: 0,
                kind: EventKind::TerminalOutput,
                step: None,
                summary: "terminal output 12 bytes".into(),
                body_json: Some(serde_json::json!({"text": "$ ls\n"})),
                blob_hash: None,
                blob_size: None,
                correlates: None,
            })
            .unwrap();
        let after = store.blobs().put(b"line one\nline two\n").unwrap();
        writer
            .append_file_change(
                Event {
                    session_id: sid.clone(),
                    ts_ms: 10,
                    kind: EventKind::FileChange,
                    step: None,
                    summary: "notes.txt created".into(),
                    body_json: Some(serde_json::json!({"path": "notes.txt"})),
                    blob_hash: Some(after.hash.clone()),
                    blob_size: Some(after.size),
                    correlates: None,
                },
                FileChange {
                    event_id: 0,
                    path: "notes.txt".into(),
                    change_kind: ChangeKind::Created,
                    before_hash: None,
                    after_hash: Some(after.hash.clone()),
                    is_binary: false,
                },
            )
            .unwrap();
        writer.finish().unwrap();
        store.assign_steps(&sid).unwrap();
        store
            .finalize_session(&sid, 100, Some(0), SessionStatus::Ok)
            .unwrap();
        let session = store.get_session(&sid).unwrap();

        let payload = build_html_payload(&store, &session).unwrap();
        let events = payload["events"].as_array().unwrap();
        assert!(
            events.iter().all(|e| e["kind"] != "terminal_output"),
            "terminal_output must be elided from the HTML timeline: {events:?}"
        );
        let fc = events
            .iter()
            .find(|e| e["kind"] == "file_change")
            .expect("file_change event present");
        let hunks = fc["diff"]["hunks"].as_array().unwrap();
        assert!(!hunks.is_empty());
        assert!(hunks[0]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["text"] == "line one"));
    }
}
