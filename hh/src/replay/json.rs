//! Pretty-printed JSON with syntax tinting for the detail pane (FR-3.2):
//! 2-space indent (serde_json's pretty printer), keys and string values
//! tinted, punctuation dimmed. Numbers/booleans/null are left in the base
//! style — a deliberate simplification (see [`tokenize`]) rather than a full
//! JSON lexer, which would be excess machinery for a read-only viewer.

use super::theme::Theme;
use ratatui::style::Style;
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
                let raw: String = chars[start..i.min(chars.len())].iter().collect();
                let mut j = i;
                while j < chars.len() && chars[j] == ' ' {
                    j += 1;
                }
                let is_key = chars.get(j) == Some(&':');
                let style = if is_key {
                    theme.json_key_style()
                } else {
                    theme.json_string_style()
                };
                push_string_literal(&mut lines, &mut current, &raw, style);
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

fn flush(buf: &mut String, current: &mut Vec<Span<'static>>) {
    if !buf.is_empty() {
        current.push(Span::raw(std::mem::take(buf)));
    }
}

/// Decode `raw` (a JSON string literal, quotes included, as produced by
/// `serde_json::to_string_pretty`) into display text with escapes like `\n`
/// and `\t` turned back into real control characters, re-wrapped in display
/// quotes. A tool result's `content` field is human-readable text, not JSON
/// syntax to be read verbatim — showing `\n` as two glyphs instead of a line
/// break is the bug this exists to avoid. Falls back to the untouched raw
/// text (still escaped) if `raw` isn't a well-formed JSON string, since this
/// runs over the pretty-printer's own output and must never panic.
fn decode_string_literal(raw: &str) -> String {
    match serde_json::from_str::<String>(raw) {
        Ok(decoded) => format!("\"{decoded}\""),
        Err(_) => raw.to_string(),
    }
}

/// Push a (possibly multi-line, once decoded) string literal into the
/// in-progress line buffer, starting a new [`Line`] at each real newline so
/// decoded content renders across multiple terminal rows instead of being
/// squashed into one.
fn push_string_literal(
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    raw: &str,
    style: Style,
) {
    let decoded = decode_string_literal(raw);
    let mut parts = decoded.split('\n');
    if let Some(first) = parts.next() {
        current.push(Span::styled(first.to_string(), style));
    }
    for part in parts {
        lines.push(Line::from(std::mem::take(current)));
        current.push(Span::styled(part.to_string(), style));
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
    fn multiline_string_values_do_not_break_tokenizer() {
        let v = serde_json::json!({"text": "line one\nline two"});
        let theme = Theme::resolve(true);
        // Must not panic, and the escaped `\n` serde_json writes into the
        // pretty-printed JSON must come back as a real line break, not the
        // literal two-character sequence `\` `n`.
        let lines = pretty_lines(&v, theme);
        assert!(!lines.is_empty());
        let text = plain(&lines);
        assert!(!text.contains("\\n"), "escape sequence leaked: {text:?}");
        assert!(text.contains("line one"));
        assert!(text.contains("line two"));
    }

    #[test]
    fn decodes_newline_and_tab_escapes_in_string_values() {
        let v = serde_json::json!({"content": "col1\tcol2\nrow2col1\trow2col2"});
        let theme = Theme::resolve(true);
        let lines = pretty_lines(&v, theme);
        let text = plain(&lines);
        assert!(!text.contains("\\n"), "escaped newline leaked: {text:?}");
        assert!(!text.contains("\\t"), "escaped tab leaked: {text:?}");
        assert!(text.contains("col1\tcol2"));
        assert!(text.contains("row2col1\trow2col2"));
        assert!(
            text.contains("col1\tcol2\nrow2col1"),
            "expected an actual line break splitting the two rows: {text:?}"
        );
    }
}
