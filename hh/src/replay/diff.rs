//! Unified diff rendering for `FileChange` events (FR-3.2), via `similar`.

use super::theme::Theme;
use hh_core::{ChangeKind, FileChange, Store};
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

/// Render a file change as a unified diff: a one-line header, then hunks with
/// `@@ ... @@` headers and `+`/`-`-colored lines. Binary files and creates/
/// deletes with no readable counterpart render a short descriptive line
/// instead of attempting a byte diff.
#[must_use]
pub fn render_file_change(fc: &FileChange, store: &Store, theme: Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("{} ({})", fc.path, fc.change_kind),
        theme.accent_style(),
    ))];

    if fc.is_binary {
        lines.push(Line::from("binary file — diff not shown"));
        return lines;
    }

    let before_text = fc
        .before_hash
        .as_deref()
        .and_then(|h| store.blobs().get(h).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    let after_text = fc
        .after_hash
        .as_deref()
        .and_then(|h| store.blobs().get(h).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned());

    match fc.change_kind {
        ChangeKind::Created => {
            lines.push(Line::from("(new file)"));
        }
        ChangeKind::Deleted => {
            lines.push(Line::from("(deleted)"));
        }
        ChangeKind::Modified => {}
    }

    let (Some(before), Some(after)) = (before_text.as_deref(), after_text.as_deref()) else {
        // A create/delete has only one side, or the referenced blob is
        // missing/was never captured (e.g. exceeded max_file_size) — show
        // whichever side is available in full rather than a diff.
        let only = before_text.as_deref().or(after_text.as_deref());
        if let Some(text) = only {
            lines.extend(text.lines().map(|l| Line::from(l.to_string())));
        } else {
            lines.push(Line::from("(content not captured)"));
        }
        return lines;
    };

    let diff = TextDiff::from_lines(before, after);
    let unified = diff.unified_diff();
    let mut any_hunk = false;
    for hunk in unified.iter_hunks() {
        any_hunk = true;
        lines.push(Line::from(Span::styled(
            format!("{}", hunk.header()),
            theme.diff_hunk_style(),
        )));
        for change in hunk.iter_changes() {
            let (prefix, style) = match change.tag() {
                ChangeTag::Delete => ("-", theme.diff_delete_style()),
                ChangeTag::Insert => ("+", theme.diff_insert_style()),
                ChangeTag::Equal => (" ", ratatui::style::Style::default()),
            };
            let content = change.to_string_lossy();
            let content = content.strip_suffix('\n').unwrap_or(&content);
            lines.push(Line::from(Span::styled(
                format!("{prefix}{content}"),
                style,
            )));
        }
    }
    if !any_hunk {
        lines.push(Line::from("(no changes)"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let s = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        (tmp, s)
    }

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
    fn modified_file_shows_hunk_header_and_signs() {
        let (_tmp, store) = store();
        let before = store
            .blobs()
            .put(b"line one\nline two\nline three\n")
            .unwrap();
        let after = store
            .blobs()
            .put(b"line one\nline TWO changed\nline three\n")
            .unwrap();
        let fc = FileChange {
            event_id: 1,
            path: "src/lib.rs".into(),
            change_kind: ChangeKind::Modified,
            before_hash: Some(before.hash),
            after_hash: Some(after.hash),
            is_binary: false,
        };
        let theme = Theme::resolve(true);
        let text = plain(&render_file_change(&fc, &store, theme));
        assert!(text.contains("@@"));
        assert!(text.contains("-line two"));
        assert!(text.contains("+line TWO changed"));
    }

    #[test]
    fn binary_file_shows_placeholder() {
        let (_tmp, store) = store();
        let fc = FileChange {
            event_id: 1,
            path: "logo.png".into(),
            change_kind: ChangeKind::Modified,
            before_hash: None,
            after_hash: None,
            is_binary: true,
        };
        let theme = Theme::resolve(true);
        let text = plain(&render_file_change(&fc, &store, theme));
        assert!(text.contains("binary file"));
    }

    #[test]
    fn created_file_shows_full_content_no_diff_markers_needed() {
        let (_tmp, store) = store();
        let after = store.blobs().put(b"brand new content\n").unwrap();
        let fc = FileChange {
            event_id: 1,
            path: "NEW.md".into(),
            change_kind: ChangeKind::Created,
            before_hash: None,
            after_hash: Some(after.hash),
            is_binary: false,
        };
        let theme = Theme::resolve(true);
        let text = plain(&render_file_change(&fc, &store, theme));
        assert!(text.contains("new file"));
        assert!(text.contains("brand new content"));
    }

    #[test]
    fn missing_blob_shows_placeholder_not_error() {
        let (_tmp, store) = store();
        let fc = FileChange {
            event_id: 1,
            path: "gone.txt".into(),
            change_kind: ChangeKind::Modified,
            before_hash: Some("a".repeat(64)),
            after_hash: Some("b".repeat(64)),
            is_binary: false,
        };
        let theme = Theme::resolve(true);
        let text = plain(&render_file_change(&fc, &store, theme));
        assert!(text.contains("not captured"));
    }
}
