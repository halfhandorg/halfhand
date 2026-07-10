//! Pretty-printed JSON with syntax tinting for the detail pane (FR-3.2):
//! 2-space indent (serde_json's pretty printer), keys and string values
//! tinted, punctuation dimmed. Numbers/booleans/null are left in the base
//! style — a deliberate simplification (see [`tokenize`]) rather than a full
//! JSON lexer, which would be excess machinery for a read-only viewer.

use super::theme::Theme;
use ratatui::text::{Line, Span};

/// A handful of internal bookkeeping fields that are meaningful to the
/// recorder/adapters but not to a human reading the replay (FR-1.5's
/// `correlate_key`, used to resolve `events.correlates` before storage).
/// Stripped before pretty-printing so the detail pane shows only
/// user-relevant payload.
const HIDDEN_KEYS: &[&str] = &["correlate_key"];

/// Pretty-print `value` as tinted [`Line`]s, with [`HIDDEN_KEYS`] removed.
#[must_use]
pub fn pretty_lines(value: &serde_json::Value, theme: Theme) -> Vec<Line<'static>> {
    let cleaned = strip_hidden_keys(value);
    let text = serde_json::to_string_pretty(&cleaned).unwrap_or_else(|_| cleaned.to_string());
    tokenize(&text, theme)
}

fn strip_hidden_keys(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if HIDDEN_KEYS.contains(&k.as_str()) {
                    continue;
                }
                out.insert(k.clone(), strip_hidden_keys(v));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(strip_hidden_keys).collect())
        }
        other => other.clone(),
    }
}

/// Scan `text` char-by-char, classifying quoted strings (as a JSON key if
/// followed by `:`, else a string value) and structural punctuation; every
/// other run of characters (numbers, `true`/`false`/`null`, whitespace) is
/// emitted unstyled.
fn tokenize(text: &str, theme: Theme) -> Vec<Line<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                flush(&mut buf, &mut current);
                lines.push(Line::from(std::mem::take(&mut current)));
                i += 1;
            }
            '"' => {
                flush(&mut buf, &mut current);
                let start = i;
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 2;
                        continue;
                    }
                    if chars[i] == '"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                let s: String = chars[start..i.min(chars.len())].iter().collect();
                let mut j = i;
                while j < chars.len() && chars[j] == ' ' {
                    j += 1;
                }
                let is_key = chars.get(j) == Some(&':');
                if is_key {
                    current.push(Span::styled(s, theme.json_key_style()));
                } else {
                    push_string_value(&s, theme.json_string_style(), &mut lines, &mut current);
                }
            }
            '{' | '}' | '[' | ']' | ':' | ',' => {
                flush(&mut buf, &mut current);
                current.push(Span::styled(c.to_string(), theme.json_punct_style()));
                i += 1;
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush(&mut buf, &mut current);
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

/// Push a raw JSON string token (`raw` includes the surrounding quotes and
/// any escapes, exactly as `serde_json::to_string_pretty` wrote it) onto the
/// line being built. A value whose *unescaped* content contains a real
/// newline — extremely common for tool output (file contents, command
/// stdout) — is split so each embedded line renders as its own display
/// line, instead of a wall of literal `\n` two-character escapes that made
/// large tool outputs unreadable in the detail pane.
fn push_string_value(
    raw: &str,
    style: ratatui::style::Style,
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
) {
    let Ok(unescaped) = serde_json::from_str::<String>(raw) else {
        current.push(Span::styled(raw.to_string(), style));
        return;
    };
    if !unescaped.contains('\n') {
        current.push(Span::styled(raw.to_string(), style));
        return;
    }
    let mut parts = unescaped.split('\n');
    if let Some(first) = parts.next() {
        current.push(Span::styled(format!("\"{first}"), style));
    }
    let rest: Vec<&str> = parts.collect();
    let last = rest.len().saturating_sub(1);
    for (idx, part) in rest.into_iter().enumerate() {
        lines.push(Line::from(std::mem::take(current)));
        if idx == last {
            current.push(Span::styled(format!("{part}\""), style));
        } else {
            current.push(Span::styled(part.to_string(), style));
        }
    }
}

fn flush(buf: &mut String, current: &mut Vec<Span<'static>>) {
    if !buf.is_empty() {
        current.push(Span::raw(std::mem::take(buf)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn round_trips_readable_text() {
        let v = serde_json::json!({"name": "Bash", "input": {"command": "ls -la"}});
        let theme = Theme::resolve(true);
        let lines = pretty_lines(&v, theme);
        let text = plain(&lines);
        assert!(text.contains("\"name\""));
        assert!(text.contains("\"Bash\""));
        assert!(text.contains("\"command\""));
    }

    #[test]
    fn strips_correlate_key() {
        let v = serde_json::json!({"name": "Bash", "correlate_key": "tu_1"});
        let theme = Theme::resolve(true);
        let text = plain(&pretty_lines(&v, theme));
        assert!(!text.contains("correlate_key"));
        assert!(!text.contains("tu_1"));
    }

    #[test]
    fn strips_correlate_key_in_nested_objects() {
        let v = serde_json::json!({"outer": {"correlate_key": "x", "keep": 1}});
        let theme = Theme::resolve(true);
        let text = plain(&pretty_lines(&v, theme));
        assert!(!text.contains("correlate_key"));
        assert!(text.contains("keep"));
    }

    #[test]
    fn multiline_string_values_render_as_real_lines() {
        let v = serde_json::json!({"text": "line one\nline two"});
        let theme = Theme::resolve(true);
        let lines = pretty_lines(&v, theme);
        let text = plain(&lines);
        // The embedded newline must become an actual line break, not a
        // visible literal `\n` (what serde_json's escaped output looks like
        // before this splits it) — otherwise a multi-line tool output (a
        // Bash stdout, a file's contents) renders as one unreadable wall of
        // `\n` escapes in the detail pane.
        assert!(!text.contains("\\n"), "literal backslash-n leaked: {text}");
        assert!(text.contains("\"line one"));
        assert!(text.contains("line two\""));
        let has_split_line = lines.iter().any(|l| {
            let s = l
                .spans
                .iter()
                .map(|sp| sp.content.as_ref())
                .collect::<String>();
            s.trim() == "line two\""
        });
        assert!(has_split_line, "line two should be on its own display line");
    }

    #[test]
    fn multiline_string_value_with_three_lines_splits_each() {
        let v = serde_json::json!({"text": "a\nb\nc"});
        let theme = Theme::resolve(true);
        let text = plain(&pretty_lines(&v, theme));
        assert!(!text.contains("\\n"), "literal backslash-n leaked: {text}");
        assert!(text.contains("\"a"));
        assert!(
            text.contains("\nb\n"),
            "middle line should stand alone: {text}"
        );
        assert!(text.contains("c\""));
    }
}
