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
                let style = if is_key {
                    theme.json_key_style()
                } else {
                    theme.json_string_style()
                };
                current.push(Span::styled(s, style));
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
        // Must not panic; the escaped \n inside the JSON string stays inside
        // the quoted span (serde_json escapes it as literal `\n`, two chars).
        let lines = pretty_lines(&v, theme);
        assert!(!lines.is_empty());
    }
}
