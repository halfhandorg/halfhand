//! Rendering for the replay TUI (FR-3.2): header, timeline pane, detail
//! pane, status/prompt line, and the `?` help overlay.

use super::data::ReplayData;
use super::json::strip_control_sequences;
use super::kind;
use super::state::{AppState, Focus, Mode};
use super::theme::Theme;
use hh_core::{EventDetail, EventKind, SessionStatus, StepEntry, TerminalSegment, TimelineRow};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::Frame;

/// The full keymap, shown in the `?` help overlay — kept as one table so
/// adding a binding only means adding a row here (FR-3.3: "listing every
/// binding").
const KEYMAP: &[(&str, &str)] = &[
    ("j / Down", "move selection down"),
    ("k / Up", "move selection up"),
    ("g / Home", "jump to first row"),
    ("G / End", "jump to last row"),
    ("PageUp / PageDown", "page the timeline"),
    ("J", "jump to a timestamp (mm:ss or hh:mm:ss)"),
    ("/", "filter by kind or summary text"),
    ("Esc", "clear the filter / cancel a prompt"),
    ("t", "toggle terminal-output segments"),
    ("d", "jump to the next file diff"),
    ("Tab", "switch focus: timeline / detail"),
    ("?", "toggle this help"),
    ("q", "quit"),
];

/// Draw one frame of the replay TUI.
pub fn draw(f: &mut Frame, state: &AppState, data: &mut ReplayData, theme: Theme) {
    let size = f.size();
    let outer = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(size);

    draw_header(f, outer[0], state, theme);

    let body = Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(outer[1]);
    draw_timeline(f, body[0], state, theme);
    draw_detail(f, body[1], state, data, theme);

    draw_status_line(f, outer[2], state, theme);

    if state.mode == Mode::Help {
        draw_help_overlay(f, size, theme);
    }
}

fn draw_header(f: &mut Frame, area: Rect, state: &AppState, theme: Theme) {
    let s = &state.session;
    let duration = match s.ended_at {
        Some(end) => humanize_ms((end - s.started_at).max(0)),
        None => "—".to_string(),
    };
    let status_word = match s.status {
        SessionStatus::Ok => "✓ ok",
        SessionStatus::Error => "✗ error",
        SessionStatus::Interrupted => "● interrupted",
        SessionStatus::Recording => "● recording",
    };
    let line1 = Line::from(vec![
        Span::styled(format!(" {} ", s.short_id), theme.accent_style()),
        Span::raw(format!(
            "{status_word} · {} · {duration} · {} steps · {} files changed",
            s.agent_kind, s.step_count, s.files_changed
        )),
    ]);
    let line2 = Line::from(Span::styled(
        format!(" {}", s.command.join(" ")),
        theme.dim_style(),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style());
    let p = Paragraph::new(vec![line1, line2]).block(block);
    f.render_widget(p, area);
}

fn draw_timeline(f: &mut Frame, area: Rect, state: &AppState, theme: Theme) {
    let rows = state.visible_rows();
    let (selected_pos, total) = state.selection_position();
    let inner_height = area.height.saturating_sub(2) as usize;
    let scroll = state.timeline_scroll.min(rows.len().saturating_sub(1));

    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .skip(scroll)
        .take(inner_height.max(1))
        .map(|(pos, row)| render_row(row, theme, pos == selected_pos))
        .collect();

    let title = if total == 0 {
        " Timeline (empty) ".to_string()
    } else {
        format!(" Timeline ({}/{total}) ", selected_pos + 1)
    };
    let border_style = if state.focus == Focus::Timeline {
        theme.accent_style()
    } else {
        theme.border_style()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style);
    f.render_widget(List::new(items).block(block), area);

    if total > inner_height {
        let mut sb_state = ScrollbarState::new(total)
            .position(selected_pos)
            .viewport_content_length(inner_height.max(1));
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        f.render_stateful_widget(scrollbar, area, &mut sb_state);
    }
}

fn render_row<'a>(row: &TimelineRow, theme: Theme, selected: bool) -> ListItem<'a> {
    let ts = Span::styled(format!("{} ", format_hms(row.ts_ms())), theme.dim_style());
    let badge = Span::styled(
        format!("{:<5}", kind::badge_label(row.kind())),
        theme.badge_style(row.kind()),
    );
    let label = Span::raw(format!(" {}", strip_control_sequences(&row.label())));
    let line = Line::from(vec![ts, badge, label]);
    let item = ListItem::new(line);
    if selected {
        item.style(theme.selected_style())
    } else {
        item
    }
}

fn draw_detail(f: &mut Frame, area: Rect, state: &AppState, data: &mut ReplayData, theme: Theme) {
    let border_style = if state.focus == Focus::Detail {
        theme.accent_style()
    } else {
        theme.border_style()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Detail ")
        .border_style(border_style);

    let lines = match state.selected_row() {
        Some(row) => detail_lines(row, data, theme),
        None => vec![Line::from("(no steps in this session)")],
    };
    let p = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((u16::try_from(state.detail_scroll).unwrap_or(u16::MAX), 0));
    f.render_widget(p, area);
}

fn detail_lines(row: &TimelineRow, data: &mut ReplayData, theme: Theme) -> Vec<Line<'static>> {
    match row {
        TimelineRow::Terminal(seg) => render_terminal_segment(seg, data, theme),
        TimelineRow::Step(step) => render_step_detail(step, data, theme),
    }
}

fn render_terminal_segment(
    seg: &TerminalSegment,
    data: &mut ReplayData,
    theme: Theme,
) -> Vec<Line<'static>> {
    let details = match data.get_many(&seg.event_ids) {
        Ok(d) => d,
        Err(e) => {
            return vec![Line::from(Span::styled(
                format!("error: {e}"),
                theme.error_style(),
            ))]
        }
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("terminal output · {} chunk(s)", details.len()),
        theme.accent_style(),
    ))];
    for d in &details {
        let text = d
            .body_json
            .as_ref()
            .and_then(|v| v.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let text = strip_control_sequences(text);
        for line in text.lines() {
            lines.push(Line::from(line.to_string()));
        }
    }
    lines
}

fn render_step_detail(step: &StepEntry, data: &mut ReplayData, theme: Theme) -> Vec<Line<'static>> {
    let details = match data.get_many(&step.event_ids) {
        Ok(d) => d,
        Err(e) => {
            return vec![Line::from(Span::styled(
                format!("error: {e}"),
                theme.error_style(),
            ))]
        }
    };
    let Some(primary) = details.first() else {
        return vec![Line::from("(no data)")];
    };
    match primary.kind {
        EventKind::UserMessage
        | EventKind::AgentMessage
        | EventKind::Thinking
        | EventKind::Lifecycle => render_text_detail(&details),
        EventKind::Error => {
            let mut lines = vec![Line::from(Span::styled(
                strip_control_sequences(&primary.summary),
                theme.error_style(),
            ))];
            if let Some(body) = &primary.body_json {
                lines.extend(super::json::pretty_lines(body, theme));
            }
            lines
        }
        EventKind::FileChange => render_file_change_detail(primary, data, theme),
        EventKind::ToolCall
        | EventKind::ToolResult
        | EventKind::McpRequest
        | EventKind::McpResponse
        | EventKind::McpNotification => render_pair_detail(&details, primary, data, theme),
        EventKind::TerminalOutput => vec![Line::from("(terminal output)")],
    }
}

fn render_text_detail(details: &[EventDetail]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, d) in details.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        let text = d
            .body_json
            .as_ref()
            .and_then(|v| v.get("text"))
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty());
        match text {
            Some(t) => {
                let t = strip_control_sequences(t);
                lines.extend(t.lines().map(|l| Line::from(l.to_string())));
            }
            None => lines.push(Line::from(strip_control_sequences(&d.summary))),
        }
    }
    lines
}

fn render_file_change_detail(
    primary: &EventDetail,
    data: &ReplayData,
    theme: Theme,
) -> Vec<Line<'static>> {
    match &primary.file_change {
        Some(fc) => super::diff::render_file_change(fc, data.store(), theme),
        None => vec![Line::from(strip_control_sequences(&primary.summary))],
    }
}

fn render_pair_detail(
    details: &[EventDetail],
    primary: &EventDetail,
    data: &mut ReplayData,
    theme: Theme,
) -> Vec<Line<'static>> {
    let mut pair: Vec<EventDetail> = details.to_vec();
    if pair.len() < 2 {
        if let Ok(Some(other)) = data.get_correlated(primary.id, primary.correlates) {
            pair.push(other);
        }
    }
    pair.sort_by_key(|d| d.ts_ms);
    pair.dedup_by_key(|d| d.id);

    let mut lines = Vec::new();
    for (i, d) in pair.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!(
                "{} — {}",
                kind::badge_label(d.kind),
                strip_control_sequences(&d.summary)
            ),
            theme.accent_style(),
        )));
        if let Some(body) = &d.body_json {
            lines.extend(super::json::pretty_lines(body, theme));
        }
    }
    if pair.len() == 2 {
        let latency = (pair[1].ts_ms - pair[0].ts_ms).max(0);
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("latency: {latency}ms"),
            theme.dim_style(),
        )));
    }
    lines
}

fn draw_status_line(f: &mut Frame, area: Rect, state: &AppState, theme: Theme) {
    let line = match state.mode {
        Mode::Filter => Line::from(vec![
            Span::styled("/", theme.accent_style()),
            Span::raw(state.filter_input.clone()),
            Span::styled("  (Enter to keep, Esc to clear)", theme.dim_style()),
        ]),
        Mode::Jump => Line::from(vec![
            Span::styled("jump to ", theme.accent_style()),
            Span::raw(state.jump_input.clone()),
            Span::styled("  (mm:ss or hh:mm:ss, Enter/Esc)", theme.dim_style()),
        ]),
        Mode::Help => Line::from(Span::styled(
            "press any key to close help",
            theme.dim_style(),
        )),
        Mode::Normal => {
            if let Some(status) = &state.status {
                Line::from(Span::styled(status.clone(), theme.error_style()))
            } else {
                let filter_hint = if state.filter_input.is_empty() {
                    String::new()
                } else {
                    format!("  filter: {}", state.filter_input)
                };
                Line::from(Span::styled(
                    format!("? help  ·  q quit{filter_hint}"),
                    theme.dim_style(),
                ))
            }
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_help_overlay(f: &mut Frame, size: Rect, theme: Theme) {
    let width = size.width.saturating_sub(10).clamp(20, 60);
    let height = u16::try_from(KEYMAP.len()).unwrap_or(20) + 2;
    let height = height.min(size.height.saturating_sub(4));
    let x = (size.width.saturating_sub(width)) / 2;
    let y = (size.height.saturating_sub(height)) / 2;
    let area = Rect::new(x, y, width, height);

    f.render_widget(Clear, area);
    let lines: Vec<Line> = KEYMAP
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(format!("{key:<20}"), theme.accent_style()),
                Span::raw(*desc),
            ])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Keymap ")
        .border_style(theme.accent_style());
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Format a millisecond offset as `hh:mm:ss` (FR-3.2 relative timestamps,
/// e.g. `00:00:07`).
fn format_hms(ms: i64) -> String {
    let total_secs = ms.max(0) / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Humanize a millisecond duration as `4m32s` / `12s` / `350ms`, matching
/// `main.rs`'s epilogue formatting (NFR-7 consistency).
fn humanize_ms(ms: i64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{hours}h{minutes}m{secs}s")
    } else if minutes > 0 {
        format!("{minutes}m{secs}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::state::{update, AppEvent, KeyInput};
    use hh_core::{AdapterStatus, AgentKind, Event, NewSession, Store};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A small fixture session with one of each interesting row shape: a user
    /// message, a correlated tool_call/tool_result pair, an agent message, and
    /// a terminal_output chunk (hidden by default). Real `Store`-backed (not
    /// hand-built `EventIndexRow`s) so the detail pane's lazy body fetch has
    /// something genuine to load.
    fn fixture() -> (TempDir, AppState, ReplayData) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let new_session = NewSession {
            id: hh_core::event::now_v7(),
            started_at: 0,
            agent_kind: AgentKind::ClaudeCode,
            adapter_status: AdapterStatus::Active,
            command: vec!["claude".into()],
            cwd: PathBuf::from("/tmp/work"),
            hostname: None,
            hh_version: "0.1.0-beta.1".into(),
            model: Some("glm-5.2".into()),
            git_branch: None,
            git_sha: None,
            git_dirty: None,
        };
        let created = store.create_session(&new_session).unwrap();
        {
            let writer = store.event_writer().unwrap();
            writer
                .append_event(Event {
                    session_id: created.id.clone(),
                    ts_ms: 0,
                    kind: EventKind::UserMessage,
                    step: None,
                    summary: "please list the directory".into(),
                    body_json: Some(serde_json::json!({"text": "please list the directory"})),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            let call_id = writer
                .append_event(Event {
                    session_id: created.id.clone(),
                    ts_ms: 1000,
                    kind: EventKind::ToolCall,
                    step: None,
                    summary: "tool_call: Bash".into(),
                    body_json: Some(
                        serde_json::json!({"name": "Bash", "input": {"command": "ls"}}),
                    ),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            writer
                .append_event(Event {
                    session_id: created.id.clone(),
                    ts_ms: 1500,
                    kind: EventKind::ToolResult,
                    step: None,
                    summary: "tool_result: ok".into(),
                    body_json: Some(serde_json::json!({"content": "ok"})),
                    blob_hash: None,
                    blob_size: None,
                    correlates: Some(call_id),
                })
                .unwrap();
            writer
                .append_event(Event {
                    session_id: created.id.clone(),
                    ts_ms: 2000,
                    kind: EventKind::AgentMessage,
                    step: None,
                    summary: "done listing".into(),
                    body_json: Some(serde_json::json!({"text": "done listing"})),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            writer
                .append_event(Event {
                    session_id: created.id.clone(),
                    ts_ms: 100,
                    kind: EventKind::TerminalOutput,
                    step: None,
                    summary: "terminal chunk".into(),
                    body_json: Some(serde_json::json!({"text": "$ ls\nfile.txt\n"})),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            writer.finish().unwrap();
        }
        store.assign_steps(&created.id).unwrap();
        let session = store.get_session(&created.id).unwrap();
        let index = store.list_event_index(&created.id).unwrap();
        let app = AppState::new(session, index);
        let data = ReplayData::new(store);
        (tmp, app, data)
    }

    /// Render one frame to an 80x24 `TestBackend` and return its plain-text
    /// view (FR-3 testing standard: TestBackend, no real terminal).
    fn render(app: &AppState, data: &mut ReplayData) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::resolve(true);
        terminal.draw(|f| draw(f, app, data, theme)).unwrap();
        format!("{}", terminal.backend())
    }

    #[test]
    fn timeline_renders_badges_and_labels() {
        let (_tmp, app, mut data) = fixture();
        let text = render(&app, &mut data);
        assert!(text.contains("USER"), "missing USER badge:\n{text}");
        assert!(text.contains("TOOL"), "missing TOOL badge:\n{text}");
        assert!(text.contains("AGENT"), "missing AGENT badge:\n{text}");
        assert!(
            text.contains("please list"),
            "missing user summary:\n{text}"
        );
    }

    #[test]
    fn filter_hides_non_matching_rows() {
        let (_tmp, mut app, mut data) = fixture();
        update(&mut app, AppEvent::Key(KeyInput::Char('/')));
        for c in "agent".chars() {
            update(&mut app, AppEvent::Key(KeyInput::Char(c)));
        }
        update(&mut app, AppEvent::Key(KeyInput::Enter));
        let text = render(&app, &mut data);
        assert!(
            text.contains("done listing"),
            "matching row missing:\n{text}"
        );
        assert!(
            !text.contains("please list"),
            "filtered-out user-message row should not render:\n{text}"
        );
    }

    #[test]
    fn jump_selects_and_highlights_nearest_row() {
        let (_tmp, mut app, mut data) = fixture();
        update(&mut app, AppEvent::Key(KeyInput::Char('J')));
        for c in "00:02".chars() {
            update(&mut app, AppEvent::Key(KeyInput::Char(c)));
        }
        update(&mut app, AppEvent::Key(KeyInput::Enter));
        assert_eq!(app.selected_row().unwrap().label(), "done listing");
        let text = render(&app, &mut data);
        assert!(text.contains("done listing"));
    }

    #[test]
    fn terminal_toggle_reveals_segment_row() {
        let (_tmp, mut app, mut data) = fixture();
        let before = render(&app, &mut data);
        assert!(
            !before.contains("TERM"),
            "terminal segment must be hidden by default:\n{before}"
        );
        update(&mut app, AppEvent::Key(KeyInput::Char('t')));
        let after = render(&app, &mut data);
        assert!(
            after.contains("TERM"),
            "terminal segment must appear after toggling:\n{after}"
        );
    }

    #[test]
    fn detail_pane_shows_correlated_pair_with_latency() {
        let (_tmp, mut app, mut data) = fixture();
        // Rows in ts order: UserMessage(0), ToolCall+ToolResult(1000/1500), AgentMessage(2000).
        update(&mut app, AppEvent::Key(KeyInput::Char('j')));
        assert_eq!(app.selected_row().unwrap().kind(), EventKind::ToolCall);
        let text = render(&app, &mut data);
        assert!(
            text.contains("latency"),
            "expected a latency line in the detail pane:\n{text}"
        );
    }

    /// FR-3.6: a degraded/generic session (no structured adapter — just
    /// filesystem watcher + PTY capture) must still be a genuinely useful
    /// replay: file changes show as their own steps and terminal output is
    /// reachable via the `t` toggle, with no ToolCall/AgentMessage/UserMessage
    /// events anywhere in the data.
    #[test]
    fn generic_degraded_session_is_useful_via_file_changes_and_terminal() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let new_session = NewSession {
            id: hh_core::event::now_v7(),
            started_at: 0,
            agent_kind: AgentKind::Generic,
            adapter_status: AdapterStatus::None,
            command: vec!["sh".into(), "fixture_agent.sh".into()],
            cwd: PathBuf::from("/tmp/work"),
            hostname: None,
            hh_version: "0.1.0-beta.1".into(),
            model: None,
            git_branch: None,
            git_sha: None,
            git_dirty: None,
        };
        let created = store.create_session(&new_session).unwrap();
        {
            let writer = store.event_writer().unwrap();
            writer
                .append_event(Event {
                    session_id: created.id.clone(),
                    ts_ms: 0,
                    kind: EventKind::TerminalOutput,
                    step: None,
                    summary: "terminal chunk".into(),
                    body_json: Some(serde_json::json!({"text": "running fixture agent...\n"})),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            writer
                .append_file_change(
                    Event {
                        session_id: created.id.clone(),
                        ts_ms: 500,
                        kind: EventKind::FileChange,
                        step: None,
                        summary: "created fixture_output.txt".into(),
                        body_json: None,
                        blob_hash: None,
                        blob_size: None,
                        correlates: None,
                    },
                    hh_core::FileChange {
                        event_id: 0,
                        path: "fixture_output.txt".into(),
                        change_kind: hh_core::ChangeKind::Created,
                        before_hash: None,
                        after_hash: Some(
                            store.blobs().put(b"hello from the fixture\n").unwrap().hash,
                        ),
                        is_binary: false,
                    },
                )
                .unwrap();
            writer.finish().unwrap();
        }
        store.assign_steps(&created.id).unwrap();
        let session = store.get_session(&created.id).unwrap();
        let index = store.list_event_index(&created.id).unwrap();

        // No structured-adapter kinds anywhere: this is exactly the
        // degraded/generic shape FR-3.6 is about.
        assert!(index.iter().all(|e| !matches!(
            e.kind,
            EventKind::ToolCall
                | EventKind::ToolResult
                | EventKind::AgentMessage
                | EventKind::UserMessage
                | EventKind::Thinking
        )));

        let mut app = AppState::new(session, index);
        let mut data = ReplayData::new(store);

        let text = render(&app, &mut data);
        assert!(
            text.contains("generic"),
            "header should show the generic agent kind:\n{text}"
        );
        assert!(
            text.contains("FILE"),
            "file change should render as a step:\n{text}"
        );
        assert!(
            !text.contains("TERM"),
            "terminal output stays hidden until toggled:\n{text}"
        );
        assert!(
            text.contains("created") || text.contains("new file"),
            "file change detail should be informative even with no structured events:\n{text}"
        );

        update(&mut app, AppEvent::Key(KeyInput::Char('t')));
        let text = render(&app, &mut data);
        assert!(
            text.contains("TERM"),
            "terminal segment reachable via `t`:\n{text}"
        );
    }

    /// Regression test for the claude-code adapter bug: a `Bash` tool result
    /// commonly carries the underlying command's raw ANSI codes (colored
    /// `git diff`, colored test runners, etc). ratatui writes `Span`
    /// content straight to the real terminal via crossterm's `Print`, so a
    /// leaked `ESC[...` sequence is interpreted as a cursor move/color
    /// reset instead of a glyph — corrupting the whole pane, exactly like
    /// the garbled detail view reported against `hh replay`. No `ESC` byte
    /// may reach the rendered frame, from the tool-result body, a terminal
    /// segment, or an event summary.
    #[test]
    fn detail_pane_strips_ansi_from_claude_code_tool_output() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let new_session = NewSession {
            id: hh_core::event::now_v7(),
            started_at: 0,
            agent_kind: AgentKind::ClaudeCode,
            adapter_status: AdapterStatus::Active,
            command: vec!["claude".into()],
            cwd: PathBuf::from("/tmp/work"),
            hostname: None,
            hh_version: "0.1.0-beta.1".into(),
            model: Some("glm-5.2".into()),
            git_branch: None,
            git_sha: None,
            git_dirty: None,
        };
        let created = store.create_session(&new_session).unwrap();
        let ansi_summary = "tool_result: \u{1b}[32m\u{2713} ok\u{1b}[0m";
        {
            let writer = store.event_writer().unwrap();
            let call_id = writer
                .append_event(Event {
                    session_id: created.id.clone(),
                    ts_ms: 0,
                    kind: EventKind::ToolCall,
                    step: None,
                    summary: "tool_call: Bash".into(),
                    body_json: Some(
                        serde_json::json!({"name": "Bash", "input": {"command": "git diff --color"}}),
                    ),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            writer
                .append_event(Event {
                    session_id: created.id.clone(),
                    ts_ms: 500,
                    kind: EventKind::ToolResult,
                    step: None,
                    summary: ansi_summary.into(),
                    body_json: Some(serde_json::json!({
                        "content": "\u{1b}[32m+added line\u{1b}[0m\n\u{1b}[31m-removed line\u{1b}[0m"
                    })),
                    blob_hash: None,
                    blob_size: None,
                    correlates: Some(call_id),
                })
                .unwrap();
            writer
                .append_event(Event {
                    session_id: created.id.clone(),
                    ts_ms: 100,
                    kind: EventKind::TerminalOutput,
                    step: None,
                    summary: "terminal chunk".into(),
                    body_json: Some(serde_json::json!({
                        "text": "\u{1b}[1mrunning\u{1b}[0m\n"
                    })),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                })
                .unwrap();
            writer.finish().unwrap();
        }
        store.assign_steps(&created.id).unwrap();
        let session = store.get_session(&created.id).unwrap();
        let index = store.list_event_index(&created.id).unwrap();
        let mut app = AppState::new(session, index);
        let mut data = ReplayData::new(store);

        let text = render(&app, &mut data);
        assert!(
            !text.contains('\u{1b}'),
            "ESC leaked from ToolCall row selection:\n{text}"
        );

        update(&mut app, AppEvent::Key(KeyInput::Char('j')));
        assert_eq!(app.selected_row().unwrap().kind(), EventKind::ToolCall);
        let text = render(&app, &mut data);
        assert!(
            !text.contains('\u{1b}'),
            "ESC leaked into the tool_call/tool_result detail pane:\n{text}"
        );
        assert!(text.contains("added line"));
        assert!(text.contains("removed line"));

        update(&mut app, AppEvent::Key(KeyInput::Char('t')));
        let text = render(&app, &mut data);
        assert!(
            !text.contains('\u{1b}'),
            "ESC leaked from a terminal_output segment:\n{text}"
        );
    }
}
