//! The file-change view: a unified diff with old/new line numbers, the file
//! path, and a one-line change summary (FR-3.2).
//!
//! The view never guesses. A modification whose before-side was not captured
//! says so ("before content not captured") and shows the after-side as plain,
//! unmarked text — rendering it as all-added would claim the agent wrote
//! every line. A rename is titled `old → new`. A missing after-side, a binary
//! file, and an unchanged file each get their own plain sentence.

use super::json::strip_control_sequences;
use super::theme::Theme;
use hh_core::{BeforeSource, ChangeKind, FileChange, Store};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

/// Lines of unchanged context around each hunk.
const CONTEXT_LINES: usize = 3;

/// Width of each line-number column in the gutter.
const NUM_WIDTH: usize = 5;

/// Count added and removed lines between two texts.
#[must_use]
pub fn line_stats(before: &str, after: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(before, after);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

/// Read a blob as text, or `None` if it is absent or cannot be read.
fn read_text(store: &Store, hash: Option<&str>) -> Option<String> {
    let bytes = store.blobs().get(hash?).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Render one file change as the diff view's lines.
#[must_use]
pub fn render_file_change(fc: &FileChange, store: &Store, theme: Theme) -> Vec<Line<'static>> {
    let provenance = store.file_change_provenance(fc.event_id).ok().flatten();
    let renamed_from = provenance.as_ref().and_then(|p| p.renamed_from.clone());
    let before_source = provenance.as_ref().and_then(|p| p.before_source);

    let title = match &renamed_from {
        Some(from) => format!(
            "{} → {}",
            strip_control_sequences(from),
            strip_control_sequences(&fc.path)
        ),
        None => strip_control_sequences(&fc.path),
    };
    let mut lines = vec![Line::from(Span::styled(title, theme.accent_style()))];
    let verb = if renamed_from.is_some() {
        "renamed"
    } else {
        match fc.change_kind {
            ChangeKind::Created => "created",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
        }
    };

    if fc.is_binary {
        lines.push(summary_line(theme, verb, &["binary file — diff not shown"]));
        return lines;
    }

    let before = read_text(store, fc.before_hash.as_deref());
    let after = read_text(store, fc.after_hash.as_deref());
    let source_note = before_source_note(before_source, before.is_some());

    match (fc.change_kind, before.as_deref(), after.as_deref()) {
        // Both sides present: a real diff.
        (_, Some(b), Some(a)) => {
            let (added, removed) = line_stats(b, a);
            let mut parts = Vec::new();
            if added == 0 && removed == 0 {
                parts.push("content unchanged".to_string());
            } else {
                parts.push(format!("+{added} −{removed}"));
            }
            parts.extend(source_note);
            lines.push(summary_line_owned(theme, verb, parts));
            if added + removed > 0 {
                lines.push(Line::from(""));
                push_hunks(&mut lines, b, a, theme);
            }
        }
        // A created file genuinely has no before-side: every line is added.
        (ChangeKind::Created, None, Some(a)) => {
            let (added, _) = line_stats("", a);
            lines.push(summary_line_owned(theme, verb, vec![format!("+{added}")]));
            lines.push(Line::from(""));
            push_hunks(&mut lines, "", a, theme);
        }
        // A deleted file with its before-side: every line is removed.
        (ChangeKind::Deleted, Some(b), None) => {
            let (_, removed) = line_stats(b, "");
            let mut parts = vec![format!("−{removed}")];
            parts.extend(source_note);
            lines.push(summary_line_owned(theme, verb, parts));
            lines.push(Line::from(""));
            push_hunks(&mut lines, b, "", theme);
        }
        // Before-side missing on anything but a create: say so, then show
        // what exists without pretending it was all added.
        (ChangeKind::Modified | ChangeKind::Deleted, None, after) => {
            lines.push(summary_line(theme, verb, &[]));
            lines.push(Line::from(Span::styled(
                "before content not captured",
                theme.error_style(),
            )));
            match after {
                Some(a) => {
                    lines.push(Line::from(Span::styled(
                        "shown as it was after this change",
                        theme.dim_style(),
                    )));
                    lines.push(Line::from(""));
                    push_plain(&mut lines, a, theme);
                }
                None if fc.change_kind == ChangeKind::Modified => {
                    lines.push(Line::from(Span::styled(
                        "after content not captured",
                        theme.error_style(),
                    )));
                }
                None => {}
            }
        }
        // After-side missing where one should exist.
        (ChangeKind::Created | ChangeKind::Modified, _, None) => {
            lines.push(summary_line(theme, verb, &[]));
            lines.push(Line::from(Span::styled(
                "after content not captured",
                theme.error_style(),
            )));
        }
    }
    lines
}

/// The provenance note for a before-side that was found, e.g. "before from
/// git index". `None` when the source is unknown (pre-0006 rows) or there is
/// no before-side.
fn before_source_note(source: Option<BeforeSource>, have_before: bool) -> Option<String> {
    if !have_before {
        return None;
    }
    let text = match source? {
        BeforeSource::Live => "before from earlier in this session",
        BeforeSource::Git => "before from the git index",
        BeforeSource::Baseline => "before from the session-start snapshot",
        _ => return None,
    };
    Some(text.to_string())
}

fn summary_line(theme: Theme, verb: &str, parts: &[&str]) -> Line<'static> {
    summary_line_owned(
        theme,
        verb,
        parts.iter().map(|p| (*p).to_string()).collect(),
    )
}

/// `modified · +3 −1 · before from the git index`, verb in the base style
/// and the rest dimmed, with the counts tinted.
fn summary_line_owned(theme: Theme, verb: &str, parts: Vec<String>) -> Line<'static> {
    let mut spans = vec![Span::raw(verb.to_string())];
    for part in parts {
        spans.push(Span::styled(" · ", theme.dim_style()));
        if let Some((plus, minus)) = part.split_once(' ').filter(|(p, _)| p.starts_with('+')) {
            spans.push(Span::styled(plus.to_string(), theme.diff_insert_style()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(minus.to_string(), theme.diff_delete_style()));
        } else if part.starts_with('+') {
            spans.push(Span::styled(part, theme.diff_insert_style()));
        } else if part.starts_with('−') {
            spans.push(Span::styled(part, theme.diff_delete_style()));
        } else {
            spans.push(Span::styled(part, theme.dim_style()));
        }
    }
    Line::from(spans)
}

/// Make one line of file content safe and stable to render: control bytes
/// stripped (a raw `ESC` would drive the real terminal) and tabs expanded.
fn clean(content: &str) -> String {
    let content = content.strip_suffix('\n').unwrap_or(content);
    let content = content.strip_suffix('\r').unwrap_or(content);
    strip_control_sequences(&content.replace('\t', "    "))
}

fn num(n: Option<usize>) -> String {
    n.map_or_else(|| " ".repeat(NUM_WIDTH), |n| format!("{n:>NUM_WIDTH$}"))
}

/// One gutter-prefixed diff line: `  old   new │+content`.
fn diff_line(
    old: Option<usize>,
    new: Option<usize>,
    sign: char,
    content: &str,
    style: Style,
    theme: Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{} {} ", num(old), num(new)), theme.dim_style()),
        Span::styled("│", theme.dim_style()),
        Span::styled(format!("{sign}{}", clean(content)), style),
    ])
}

fn push_hunks(lines: &mut Vec<Line<'static>>, before: &str, after: &str, theme: Theme) {
    let diff = TextDiff::from_lines(before, after);
    let mut unified = diff.unified_diff();
    unified.context_radius(CONTEXT_LINES);
    for hunk in unified.iter_hunks() {
        lines.push(Line::from(Span::styled(
            format!("{}", hunk.header()),
            theme.diff_hunk_style(),
        )));
        for change in hunk.iter_changes() {
            let old = change.old_index().map(|i| i + 1);
            let new = change.new_index().map(|i| i + 1);
            let text = change.to_string_lossy();
            let line = match change.tag() {
                ChangeTag::Delete => {
                    diff_line(old, None, '-', &text, theme.diff_delete_style(), theme)
                }
                ChangeTag::Insert => {
                    diff_line(None, new, '+', &text, theme.diff_insert_style(), theme)
                }
                ChangeTag::Equal => diff_line(old, new, ' ', &text, Style::default(), theme),
            };
            lines.push(line);
        }
    }
}

/// Content shown without diff marks (before-side unknown): new-side line
/// numbers only, no sign, base style.
fn push_plain(lines: &mut Vec<Line<'static>>, text: &str, theme: Theme) {
    for (i, l) in text.lines().enumerate() {
        lines.push(diff_line(
            None,
            Some(i + 1),
            ' ',
            l,
            Style::default(),
            theme,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::theme::Palette;
    use hh_core::{Event, EventKind, FileChangeProvenance};
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

    fn theme() -> Theme {
        Theme::new(Palette::Dark)
    }

    fn fc(
        kind: ChangeKind,
        before: Option<String>,
        after: Option<String>,
        path: &str,
    ) -> FileChange {
        FileChange {
            event_id: 1,
            path: path.into(),
            change_kind: kind,
            before_hash: before,
            after_hash: after,
            is_binary: false,
        }
    }

    #[test]
    fn modified_file_shows_numbered_hunk_and_summary() {
        let (_tmp, store) = store();
        let before = store
            .blobs()
            .put(b"line one\nline two\nline three\n")
            .unwrap();
        let after = store
            .blobs()
            .put(b"line one\nline TWO changed\nline three\n")
            .unwrap();
        let change = fc(
            ChangeKind::Modified,
            Some(before.hash),
            Some(after.hash),
            "src/lib.rs",
        );
        let text = plain(&render_file_change(&change, &store, theme()));
        assert!(text.starts_with("src/lib.rs\nmodified · +1 −1"), "{text}");
        assert!(text.contains("@@ -1,3 +1,3 @@"), "{text}");
        assert!(text.contains("    2       │-line two"), "{text}");
        assert!(text.contains("          2 │+line TWO changed"), "{text}");
        assert!(text.contains("    1     1 │ line one"), "{text}");
    }

    #[test]
    fn binary_file_shows_placeholder() {
        let (_tmp, store) = store();
        let mut change = fc(ChangeKind::Modified, None, None, "logo.png");
        change.is_binary = true;
        let text = plain(&render_file_change(&change, &store, theme()));
        assert!(text.contains("binary file — diff not shown"));
    }

    #[test]
    fn created_file_is_all_added() {
        let (_tmp, store) = store();
        let after = store.blobs().put(b"brand new content\n").unwrap();
        let change = fc(ChangeKind::Created, None, Some(after.hash), "NEW.md");
        let text = plain(&render_file_change(&change, &store, theme()));
        assert!(text.contains("created · +1"), "{text}");
        assert!(text.contains("│+brand new content"), "{text}");
    }

    #[test]
    fn missing_before_side_is_admitted_not_rendered_as_added() {
        let (_tmp, store) = store();
        let after = store.blobs().put(b"kept line\n").unwrap();
        let change = fc(ChangeKind::Modified, None, Some(after.hash), "main.rs");
        let text = plain(&render_file_change(&change, &store, theme()));
        assert!(text.contains("before content not captured"), "{text}");
        assert!(text.contains("│ kept line"), "{text}");
        assert!(
            !text.contains("│+"),
            "no line may be marked added without a before-side:\n{text}"
        );
    }

    #[test]
    fn missing_blobs_say_not_captured() {
        let (_tmp, store) = store();
        let change = fc(
            ChangeKind::Modified,
            Some("a".repeat(64)),
            Some("b".repeat(64)),
            "gone.txt",
        );
        let text = plain(&render_file_change(&change, &store, theme()));
        assert!(text.contains("before content not captured"), "{text}");
        assert!(text.contains("after content not captured"), "{text}");
    }

    #[test]
    fn rename_is_titled_as_a_rename() {
        let (_tmp, store) = store();
        let sid = {
            let created = store
                .create_session(&hh_core::NewSession {
                    id: hh_core::event::now_v7(),
                    started_at: 0,
                    agent_kind: hh_core::AgentKind::Generic,
                    adapter_status: hh_core::AdapterStatus::None,
                    command: vec!["sh".into()],
                    cwd: "/tmp".into(),
                    hostname: None,
                    hh_version: "t".into(),
                    model: None,
                    git_branch: None,
                    git_sha: None,
                    git_dirty: None,
                })
                .unwrap();
            created.id
        };
        let blob = store.blobs().put(b"same\n").unwrap();
        let writer = store.event_writer().unwrap();
        let id = writer
            .append_file_change_with_provenance(
                Event {
                    session_id: sid,
                    ts_ms: 0,
                    kind: EventKind::FileChange,
                    step: None,
                    summary: "renamed".into(),
                    body_json: None,
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                },
                fc(
                    ChangeKind::Modified,
                    Some(blob.hash.clone()),
                    Some(blob.hash.clone()),
                    "new.rs",
                ),
                FileChangeProvenance::new(BeforeSource::Live)
                    .with_renamed_from(Some("old.rs".into())),
            )
            .unwrap();
        writer.finish().unwrap();
        let mut change = fc(
            ChangeKind::Modified,
            Some(blob.hash.clone()),
            Some(blob.hash),
            "new.rs",
        );
        change.event_id = id;
        let text = plain(&render_file_change(&change, &store, theme()));
        assert!(
            text.starts_with("old.rs → new.rs\nrenamed · content unchanged"),
            "{text}"
        );
    }

    #[test]
    fn content_control_bytes_are_stripped() {
        let (_tmp, store) = store();
        let after = store.blobs().put(b"\x1b[31mred\x1b[0m\tx\n").unwrap();
        let change = fc(ChangeKind::Created, None, Some(after.hash), "a.txt");
        let text = plain(&render_file_change(&change, &store, theme()));
        assert!(!text.contains('\u{1b}'));
        assert!(text.contains("red    x"), "{text}");
    }

    #[test]
    fn line_stats_counts_both_sides() {
        assert_eq!(line_stats("a\nb\n", "a\nc\nd\n"), (2, 1));
        assert_eq!(line_stats("", ""), (0, 0));
    }
}
