//! Rendering for the replay TUI: the session header, the timeline
//! or files-touched list, the detail pane (messages, tool calls, diffs), the
//! status line, and the `?` help overlay.
//!
//! One frame draws only what is on screen: the timeline renders the visible
//! window of rows (see [`AppState::visible_window`]), and the detail pane
//! renders the selected row, cached in [`ReplayData`] so an idle redraw does
//! not re-read blobs or re-diff.

use super::data::ReplayData;
use super::json::strip_control_sequences;
use super::kind;
use super::state::{compact_ms, AppState, Focus, Mode, View, HEADER_ROWS};
use super::theme::Theme;
use crate::render;
use hh_core::{EventDetail, EventKind, SessionStatus, StepEntry, TerminalSegment, TimelineRow};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::Frame;

/// The em dash every surface prints for an unknown value. Never `0`.
const DASH: &str = "—";

/// Every keybinding, grouped, as the help overlay and
/// `docs/replay-inspect.md` list them. Adding a binding means adding a row
/// here; a test keeps the docs table in step.
pub const KEYMAP: &[(&str, &[(&str, &str)])] = &[
    (
        "Move",
        &[
            ("j / ↓", "next step, or scroll the detail pane"),
            ("k / ↑", "previous step, or scroll back"),
            ("g / Home", "first step"),
            ("G / End", "last step"),
            ("PgUp / PgDn", "page the list or the detail pane"),
            ("Tab", "switch focus: list ⇄ detail"),
        ],
    ),
    (
        "Find",
        &[
            ("n / N", "next / previous moment worth seeing"),
            ("d / D", "next / previous file diff"),
            ("/", "filter steps by kind or text"),
            ("J", "jump to a time (mm:ss or hh:mm:ss)"),
        ],
    ),
    (
        "Views",
        &[
            ("Enter", "open the step full width; open a file"),
            ("f", "files touched"),
            ("t", "show / hide terminal output"),
            ("Esc", "back, clear the filter, cancel a prompt"),
            ("?", "this help"),
            ("q", "quit"),
        ],
    ),
];

/// Draw one frame of the replay TUI. Takes the state mutably only to tell
/// it the viewport size and clamp the detail scroll to the content.
pub fn draw(f: &mut Frame, state: &mut AppState, data: &mut ReplayData, theme: Theme) {
    let size = f.size();
    let outer = Layout::vertical([
        Constraint::Length(HEADER_ROWS),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(size);

    draw_header(f, outer[0], state, theme);
    state.set_viewport_rows(usize::from(outer[1].height.saturating_sub(2)));

    if state.zoom {
        draw_detail(f, outer[1], state, data, theme);
    } else {
        let list_width = u16::try_from(u32::from(size.width) * 45 / 100)
            .unwrap_or(u16::MAX)
            .clamp(36, 64)
            .min(size.width.saturating_sub(20).max(1));
        let body = Layout::horizontal([Constraint::Length(list_width), Constraint::Min(10)])
            .split(outer[1]);
        match state.view {
            View::Timeline => draw_timeline(f, body[0], state, theme),
            View::Files => draw_files(f, body[0], state, theme),
        }
        draw_detail(f, body[1], state, data, theme);
    }

    draw_status_line(f, outer[2], state, theme);

    if state.mode == Mode::Help {
        draw_help_overlay(f, size, theme);
    }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn status_text(status: SessionStatus) -> String {
    format!("{} {status}", render::status_glyph(status))
}

/// The session header: identity and status on the first line (with any
/// capture gaps, admitted plainly), then two aligned rows of facts. Every
/// unknown value is an em dash.
fn draw_header(f: &mut Frame, area: Rect, state: &AppState, theme: Theme) {
    let s = &state.session;
    let name = s
        .name
        .as_deref()
        .map_or_else(|| DASH.to_string(), |n| render::truncate(n, 40));
    let status_style = match s.status {
        SessionStatus::Error => theme.error_style(),
        _ => Style::default(),
    };
    let mut identity = vec![
        Span::styled(format!(" {} ", s.short_id), theme.accent_style()),
        Span::raw(format!(" {name}  ")),
        Span::styled(status_text(s.status), status_style),
    ];
    for gap in state.capture_gaps() {
        identity.push(Span::styled("  ⚠ ", theme.error_style()));
        identity.push(Span::styled(gap, theme.error_style()));
    }

    let model = s
        .model_display
        .clone()
        .or_else(|| s.model_id.clone())
        .unwrap_or_else(|| DASH.to_string());
    let or_dash = |v: Option<String>| v.unwrap_or_else(|| DASH.to_string());
    let capture = match (&s.capture_tier, &s.adapter_id) {
        (Some(t), Some(a)) => format!("{t} · {a}"),
        (Some(t), None) => t.clone(),
        (None, Some(a)) => a.clone(),
        (None, None) => DASH.to_string(),
    };
    let rows: [[(&str, String); 4]; 2] = [
        [
            ("model", model),
            ("provider", or_dash(s.provider.clone())),
            (
                "duration",
                or_dash(s.duration_ms().map(render::humanize_ms)),
            ),
            ("steps", render::thousands(s.step_count)),
        ],
        [
            ("tokens", or_dash(s.total_tokens().map(render::thousands))),
            (
                "est. cost",
                or_dash(state.insights.cost_nano_usd.map(render::format_usd)),
            ),
            ("capture", capture),
            ("files", render::thousands(s.files_changed)),
        ],
    ];
    let mut lines = vec![Line::from(identity)];
    lines.extend(fact_grid(&rows, theme));
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(theme.border_style());
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Lay rows of `(label, value)` cells out as aligned columns: each column's
/// labels and values padded to that column's widest.
fn fact_grid<const N: usize>(rows: &[[(&str, String); N]], theme: Theme) -> Vec<Line<'static>> {
    let width = |s: &str| s.chars().count();
    let label_w: Vec<usize> = (0..N)
        .map(|c| rows.iter().map(|r| width(r[c].0)).max().unwrap_or(0))
        .collect();
    let value_w: Vec<usize> = (0..N)
        .map(|c| rows.iter().map(|r| width(&r[c].1)).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|row| {
            let mut spans = Vec::new();
            for (c, (label, value)) in row.iter().enumerate() {
                spans.push(Span::styled(
                    format!(" {label:<w$} ", w = label_w[c]),
                    theme.dim_style(),
                ));
                let pad = if c + 1 == N { 0 } else { value_w[c] + 2 };
                spans.push(Span::raw(format!("{value:<pad$}")));
            }
            Line::from(spans)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Timeline
// ---------------------------------------------------------------------------

/// `mm:ss` for sessions under an hour, `hh:mm:ss` otherwise, so the column
/// is as narrow as the session allows and constant within it.
fn format_ts(ms: i64, long: bool) -> String {
    let hms = render::format_hms(ms);
    if long {
        hms
    } else {
        hms.get(3..).unwrap_or(&hms).to_string()
    }
}

fn list_block(title: String, focused: bool, theme: Theme) -> Block<'static> {
    let border_style = if focused {
        theme.accent_style()
    } else {
        theme.border_style()
    };
    Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(title, theme.accent_style()))
        .border_style(border_style)
}

fn draw_scrollbar(f: &mut Frame, area: Rect, total: usize, position: usize, viewport: usize) {
    if total > viewport {
        let mut sb_state = ScrollbarState::new(total)
            .position(position)
            .viewport_content_length(viewport.max(1));
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        f.render_stateful_widget(scrollbar, area, &mut sb_state);
    }
}

fn draw_timeline(f: &mut Frame, area: Rect, state: &AppState, theme: Theme) {
    let (selected_pos, total) = state.selection_position();
    let inner_height = usize::from(area.height.saturating_sub(2)).max(1);
    let scroll = state.timeline_scroll.min(total.saturating_sub(1));
    let long = state.last_ts_ms() >= 3_600_000;

    let items: Vec<ListItem> = state
        .visible_window(scroll, inner_height)
        .map(|(pos, row, dur)| render_row(state, row, dur, long, theme, pos == selected_pos))
        .collect();

    let position = if total == 0 {
        "empty".to_string()
    } else {
        format!("{}/{total}", selected_pos + 1)
    };
    let title = match state.filtered_file() {
        Some(file) => format!(" {} · {position} ", render::truncate(&file.path, 40)),
        None => format!(" Timeline · {position} "),
    };
    let block = list_block(title, state.focus == Focus::Timeline && !state.zoom, theme);
    if items.is_empty() {
        let has_terminal = state
            .index
            .iter()
            .any(|e| e.kind == EventKind::TerminalOutput);
        let msg = if !state.filter_input.is_empty() {
            "no steps match the filter"
        } else if has_terminal && !state.show_terminal {
            "no steps recorded · t shows terminal output"
        } else {
            "no steps recorded"
        };
        f.render_widget(
            Paragraph::new(Span::styled(format!(" {msg}"), theme.dim_style())).block(block),
            area,
        );
        return;
    }
    f.render_widget(List::new(items).block(block), area);
    draw_scrollbar(f, area, total, selected_pos, inner_height);
}

/// One timeline row: `mm:ss ✗ TOOL   12.4s  label`. The mark column holds
/// `✗` for a failure and `●` for another moment worth seeing; a slow step's
/// duration takes the accent.
fn render_row<'a>(
    state: &AppState,
    row: &TimelineRow,
    duration: Option<i64>,
    long: bool,
    theme: Theme,
    selected: bool,
) -> ListItem<'a> {
    let ts = Span::styled(
        format!("{} ", format_ts(row.ts_ms(), long)),
        theme.dim_style(),
    );
    let mark = if state.row_failed(row) {
        Span::styled("✗ ", theme.error_style())
    } else if state.row_highlighted(row) {
        Span::styled("● ", theme.accent_style())
    } else {
        Span::raw("  ")
    };
    let badge = Span::styled(
        format!("{:<5}", kind::badge_label(row.kind())),
        theme.badge_style(row.kind()),
    );
    let dur_text = duration.map_or_else(|| DASH.to_string(), compact_ms);
    let dur_style = if state.is_slow(duration) {
        theme.accent_style()
    } else {
        theme.dim_style()
    };
    let dur = Span::styled(format!(" {dur_text:>6}  "), dur_style);
    let label = Span::raw(strip_control_sequences(&row.label()));
    let item = ListItem::new(Line::from(vec![ts, mark, badge, dur, label]));
    if selected {
        item.style(theme.selected_style())
    } else {
        item
    }
}

// ---------------------------------------------------------------------------
// Files touched
// ---------------------------------------------------------------------------

fn draw_files(f: &mut Frame, area: Rect, state: &AppState, theme: Theme) {
    let files = &state.insights.files;
    let inner_height = usize::from(area.height.saturating_sub(2)).max(1);
    let title = format!(" Files touched · {} ", files.len());
    let block = list_block(title, true, theme);
    if files.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(" no file changes recorded", theme.dim_style()))
                .block(block),
            area,
        );
        return;
    }
    let count_w = files
        .iter()
        .map(|g| g.event_ids.len().to_string().len())
        .max()
        .unwrap_or(1);
    let scroll = state.files_scroll.min(files.len().saturating_sub(1));
    let items: Vec<ListItem> = files
        .iter()
        .enumerate()
        .skip(scroll)
        .take(inner_height)
        .map(|(i, g)| {
            let n = g.event_ids.len();
            let edits = format!(
                " {n:>count_w$} {:<5}  ",
                if n == 1 { "edit" } else { "edits" }
            );
            let mut spans = vec![
                Span::styled(edits, theme.dim_style()),
                Span::raw(strip_control_sequences(&g.path)),
            ];
            let mut notes = Vec::new();
            if let Some(from) = &g.renamed_from {
                notes.push(format!("renamed from {}", strip_control_sequences(from)));
            }
            if g.created {
                notes.push("created".to_string());
            }
            if g.deleted {
                notes.push("deleted".to_string());
            }
            if g.binary {
                notes.push("binary".to_string());
            }
            if !notes.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", notes.join(" · ")),
                    theme.dim_style(),
                ));
            }
            let item = ListItem::new(Line::from(spans));
            if i == state.files_selected {
                item.style(theme.selected_style())
            } else {
                item
            }
        })
        .collect();
    f.render_widget(List::new(items).block(block), area);
    draw_scrollbar(f, area, files.len(), state.files_selected, inner_height);
}

// ---------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------

fn detail_title(state: &AppState) -> String {
    if state.view == View::Files && !state.zoom {
        return match state.insights.files.get(state.files_selected) {
            Some(g) => format!(" {} ", render::truncate(&g.path, 50)),
            None => " Detail ".to_string(),
        };
    }
    match state.selected_row() {
        Some(TimelineRow::Step(s)) => {
            let mut t = format!(" Step {} · {}", s.step, kind::badge_label(s.kind));
            let dur = state
                .visible_window(state.selection_position().0, 1)
                .next()
                .and_then(|(_, _, d)| d);
            if let Some(d) = dur {
                t.push_str(" · ");
                t.push_str(&compact_ms(d));
            }
            t.push(' ');
            t
        }
        Some(TimelineRow::Terminal(_)) => " Terminal output ".to_string(),
        None => " Detail ".to_string(),
    }
}

fn draw_detail(
    f: &mut Frame,
    area: Rect,
    state: &mut AppState,
    data: &mut ReplayData,
    theme: Theme,
) {
    let focused = state.focus == Focus::Detail || state.zoom;
    let block = list_block(detail_title(state), focused, theme);

    // In the files view, preview the selected file's latest change.
    let ids: Vec<i64> = if state.view == View::Files && !state.zoom {
        match state.insights.files.get(state.files_selected) {
            Some(g) => g.event_ids.last().copied().into_iter().collect(),
            None => Vec::new(),
        }
    } else {
        state
            .selected_row()
            .map(|r| r.event_ids().to_vec())
            .unwrap_or_default()
    };
    if ids.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(" nothing selected", theme.dim_style())).block(block),
            area,
        );
        return;
    }
    if data.rendered_detail(&ids, theme.palette()).is_none() {
        let (lines, wrap) = if state.view == View::Files && !state.zoom {
            (file_preview_lines(ids[0], data, theme), false)
        } else {
            match state.selected_row() {
                Some(row) => detail_lines(row, data, theme),
                None => (Vec::new(), true),
            }
        };
        data.set_rendered_detail(&ids, theme.palette(), lines, wrap);
    }
    let Some(rendered) = data.rendered_detail(&ids, theme.palette()) else {
        return;
    };
    let len = rendered.lines.len();
    state.detail_scroll = state.detail_scroll.min(len.saturating_sub(1));
    let height = usize::from(area.height.saturating_sub(2));
    let window: Vec<Line<'static>> = rendered
        .lines
        .iter()
        .skip(state.detail_scroll)
        .take(height.max(1))
        .cloned()
        .collect();
    let mut p = Paragraph::new(window).block(block);
    if rendered.wrap {
        p = p.wrap(Wrap { trim: false });
    }
    f.render_widget(p, area);
    draw_scrollbar(f, area, len, state.detail_scroll, height);
}

fn file_preview_lines(event_id: i64, data: &mut ReplayData, theme: Theme) -> Vec<Line<'static>> {
    match data.get(event_id) {
        Ok(detail) => render_file_change_detail(&detail, data, theme),
        Err(e) => vec![error_line(&e, theme)],
    }
}

fn error_line(e: &hh_core::Error, theme: Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!("could not load this step: {e} — run `hh doctor` to check the database"),
        theme.error_style(),
    ))
}

/// The detail content for a row, and whether it should wrap.
fn detail_lines(
    row: &TimelineRow,
    data: &mut ReplayData,
    theme: Theme,
) -> (Vec<Line<'static>>, bool) {
    match row {
        TimelineRow::Terminal(seg) => (render_terminal_segment(seg, data, theme), true),
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
        Err(e) => return vec![error_line(&e, theme)],
    };
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "{} chunk{} · {}",
            details.len(),
            if details.len() == 1 { "" } else { "s" },
            compact_ms(seg.end_ts_ms - seg.start_ts_ms)
        ),
        theme.dim_style(),
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

fn render_step_detail(
    step: &StepEntry,
    data: &mut ReplayData,
    theme: Theme,
) -> (Vec<Line<'static>>, bool) {
    let details = match data.get_many(&step.event_ids) {
        Ok(d) => d,
        Err(e) => return (vec![error_line(&e, theme)], true),
    };
    let Some(primary) = details.first() else {
        return (vec![Line::from("(no data)")], true);
    };
    let lines = match primary.kind {
        EventKind::UserMessage
        | EventKind::AgentMessage
        | EventKind::Thinking
        | EventKind::Lifecycle => render_text_detail(&details),
        EventKind::Error => {
            let mut lines = vec![Line::from(vec![
                Span::styled("✗ ", theme.error_style()),
                Span::styled(
                    strip_control_sequences(&primary.summary),
                    theme.error_style(),
                ),
            ])];
            if let Some(body) = &primary.body_json {
                lines.push(Line::from(""));
                lines.extend(super::json::pretty_lines(body, theme));
            }
            lines
        }
        EventKind::FileChange => {
            return (render_file_change_detail(primary, data, theme), false);
        }
        EventKind::ToolCall
        | EventKind::ToolResult
        | EventKind::McpRequest
        | EventKind::McpResponse
        | EventKind::McpNotification => render_pair_detail(&details, primary, data, theme),
        EventKind::TerminalOutput => vec![Line::from("(terminal output)")],
    };
    (lines, true)
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

fn tool_failed(d: &EventDetail) -> bool {
    d.body_json
        .as_ref()
        .and_then(|v| v.get("is_error"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
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
    if pair.len() == 2 {
        let latency = (pair[1].ts_ms - pair[0].ts_ms).max(0);
        let outcome = if pair.iter().any(tool_failed) {
            Span::styled("✗ failed", theme.error_style())
        } else {
            Span::raw("✓ returned")
        };
        lines.push(Line::from(vec![
            outcome,
            Span::styled(
                format!(" · latency {}", compact_ms(latency)),
                theme.dim_style(),
            ),
        ]));
        lines.push(Line::from(""));
    }
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
    lines
}

// ---------------------------------------------------------------------------
// Status line and help
// ---------------------------------------------------------------------------

/// The three keys that matter in the current view.
fn hints(state: &AppState) -> [(&'static str, &'static str); 3] {
    if state.view == View::Files {
        [("j/k", "select"), ("↵", "show diffs"), ("esc", "back")]
    } else if state.zoom {
        [("j/k", "scroll"), ("d", "next diff"), ("esc", "back")]
    } else if state.focus == Focus::Detail {
        [("j/k", "scroll"), ("↵", "full width"), ("tab", "list")]
    } else if state.file_filter.is_some() {
        [("↵", "open"), ("d", "next diff"), ("esc", "all files")]
    } else {
        [("↵", "open"), ("n", "next moment"), ("f", "files")]
    }
}

fn draw_status_line(f: &mut Frame, area: Rect, state: &AppState, theme: Theme) {
    let mut left: Vec<Span> = match state.mode {
        Mode::Filter => vec![
            Span::styled(" / ", theme.accent_style()),
            Span::raw(state.filter_input.clone()),
            Span::styled("▏  ↵ keep · esc clear", theme.dim_style()),
        ],
        Mode::Jump => vec![
            Span::styled(" jump to ", theme.accent_style()),
            Span::raw(state.jump_input.clone()),
            Span::styled(
                "▏  mm:ss or hh:mm:ss · ↵ go · esc cancel",
                theme.dim_style(),
            ),
        ],
        Mode::Help => vec![Span::styled(" any key closes help", theme.dim_style())],
        Mode::Normal => {
            if let Some(status) = &state.status {
                vec![Span::styled(
                    format!(" {}", strip_control_sequences(&status.text)),
                    if status.is_error {
                        theme.error_style()
                    } else {
                        Style::default()
                    },
                )]
            } else {
                let mut spans = Vec::new();
                for (i, (key, what)) in hints(state).iter().enumerate() {
                    spans.push(Span::raw(if i == 0 { " " } else { "  ·  " }));
                    spans.push(Span::styled(*key, theme.accent_style()));
                    spans.push(Span::styled(format!(" {what}"), theme.dim_style()));
                }
                spans
            }
        }
    };
    let mut right = String::new();
    if state.mode == Mode::Normal && !state.filter_input.is_empty() {
        right.push('/');
        right.push_str(&state.filter_input);
        right.push_str("  ");
    }
    right.push_str("? help ");
    let used: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let pad = usize::from(area.width).saturating_sub(used + right.chars().count());
    if pad > 0 {
        left.push(Span::raw(" ".repeat(pad)));
        left.push(Span::styled(right, theme.dim_style()));
    }
    f.render_widget(Paragraph::new(Line::from(left)), area);
}

fn draw_help_overlay(f: &mut Frame, size: Rect, theme: Theme) {
    const KEY_W: usize = 13;
    let mut lines: Vec<Line> = Vec::new();
    for (i, (section, bindings)) in KEYMAP.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!(" {section}"),
            theme.dim_style(),
        )));
        for (key, desc) in *bindings {
            lines.push(Line::from(vec![
                Span::styled(format!("   {key:<KEY_W$}"), theme.accent_style()),
                Span::raw(*desc),
            ]));
        }
    }
    let content_h = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let height = (content_h + 2).min(size.height);
    let width = 60.min(size.width);
    let x = (size.width.saturating_sub(width)) / 2;
    let y = (size.height.saturating_sub(height)) / 2;
    let area = Rect::new(x, y, width, height);

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(" Keys ", theme.accent_style()))
        .border_style(theme.accent_style());
    f.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::insights::Insights;
    use crate::replay::state::{update, AppEvent, KeyInput};
    use crate::replay::theme::Palette;
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
    /// view (testing standard: TestBackend, no real terminal).
    fn render(app: &mut AppState, data: &mut ReplayData) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::new(Palette::Mono);
        terminal.draw(|f| draw(f, app, data, theme)).unwrap();
        format!("{}", terminal.backend())
    }

    #[test]
    fn timeline_renders_badges_and_labels() {
        let (_tmp, mut app, mut data) = fixture();
        let text = render(&mut app, &mut data);
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
        let text = render(&mut app, &mut data);
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
        let text = render(&mut app, &mut data);
        assert!(text.contains("done listing"));
    }

    #[test]
    fn terminal_toggle_reveals_segment_row() {
        let (_tmp, mut app, mut data) = fixture();
        let before = render(&mut app, &mut data);
        assert!(
            !before.contains("TERM"),
            "terminal segment must be hidden by default:\n{before}"
        );
        update(&mut app, AppEvent::Key(KeyInput::Char('t')));
        let after = render(&mut app, &mut data);
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
        let text = render(&mut app, &mut data);
        assert!(
            text.contains("latency"),
            "expected a latency line in the detail pane:\n{text}"
        );
    }

    /// A degraded/generic session (no structured adapter — just
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
        // degraded/generic shape this test is about.
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

        let text = render(&mut app, &mut data);
        assert!(
            text.contains("no transcript"),
            "header must admit there is no transcript:\n{text}"
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
        let text = render(&mut app, &mut data);
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

        let text = render(&mut app, &mut data);
        assert!(
            !text.contains('\u{1b}'),
            "ESC leaked from ToolCall row selection:\n{text}"
        );

        update(&mut app, AppEvent::Key(KeyInput::Char('j')));
        assert_eq!(app.selected_row().unwrap().kind(), EventKind::ToolCall);
        let text = render(&mut app, &mut data);
        assert!(
            !text.contains('\u{1b}'),
            "ESC leaked into the tool_call/tool_result detail pane:\n{text}"
        );
        assert!(text.contains("added line"));
        assert!(text.contains("removed line"));

        update(&mut app, AppEvent::Key(KeyInput::Char('t')));
        let text = render(&mut app, &mut data);
        assert!(
            !text.contains('\u{1b}'),
            "ESC leaked from a terminal_output segment:\n{text}"
        );
    }

    // -----------------------------------------------------------------------
    // Snapshots: every screen, light and dark, on a fixed 100x30 buffer.
    // -----------------------------------------------------------------------

    /// Serialize a buffer for a snapshot: the text grid, then every run of
    /// non-default style as `row:start-end fg bg modifiers`, so a light and a
    /// dark render of the same screen produce different, reviewable output.
    fn buffer_snapshot(buf: &ratatui::buffer::Buffer) -> String {
        use std::fmt::Write as _;
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            let mut line = String::new();
            for x in 0..area.width {
                line.push_str(buf.get(x, y).symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out.push_str("--- styles ---\n");
        for y in 0..area.height {
            let mut x = 0;
            while x < area.width {
                let c = buf.get(x, y);
                let style = (c.fg, c.bg, c.modifier);
                let plain = style
                    == (
                        ratatui::style::Color::Reset,
                        ratatui::style::Color::Reset,
                        ratatui::style::Modifier::empty(),
                    );
                let start = x;
                while x < area.width {
                    let n = buf.get(x, y);
                    if (n.fg, n.bg, n.modifier) != style {
                        break;
                    }
                    x += 1;
                }
                if !plain {
                    let _ = writeln!(
                        out,
                        "{y:>2}:{start}-{} fg={:?} bg={:?} mod={:?}",
                        x - 1,
                        style.0,
                        style.1,
                        style.2
                    );
                }
            }
        }
        out
    }

    fn draw_to(app: &mut AppState, data: &mut ReplayData, palette: Palette) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| draw(f, app, data, Theme::new(palette)))
            .unwrap();
        buffer_snapshot(terminal.backend().buffer())
    }

    fn ev(sid: &str, ts: i64, kind: EventKind, summary: &str, body: serde_json::Value) -> Event {
        Event {
            session_id: sid.into(),
            ts_ms: ts,
            kind,
            step: None,
            summary: summary.into(),
            body_json: Some(body),
            blob_hash: None,
            blob_size: None,
            correlates: None,
        }
    }

    /// A finished Claude Code session with one of everything the TUI
    /// highlights: a failed tool call, a slow step, a modified file with a
    /// git-index before-side, a created file, a renamed file, a modified file
    /// whose before-side was never captured, and an error.
    #[allow(clippy::too_many_lines)] // one fixture, read top to bottom
    fn rich_fixture() -> (TempDir, AppState, ReplayData) {
        use hh_core::{BeforeSource, ChangeKind, FileChange, FileChangeProvenance};
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let sid = store
            .create_session(&NewSession {
                id: hh_core::event::now_v7(),
                started_at: 1_700_000_000_000,
                agent_kind: AgentKind::ClaudeCode,
                adapter_status: AdapterStatus::Active,
                command: vec!["claude".into()],
                cwd: PathBuf::from("/tmp/work"),
                hostname: None,
                hh_version: "test".into(),
                model: None,
                git_branch: None,
                git_sha: None,
                git_dirty: None,
            })
            .unwrap()
            .id;
        store.set_session_name(&sid, Some("auth refactor")).unwrap();
        let blob = |b: &[u8]| store.blobs().put(b).unwrap();
        let before =
            blob(b"fn login(user: &str) -> bool {\n    check(user)\n}\n\nfn logout() {}\n");
        let after = blob(
            b"fn login(user: &str, token: &str) -> bool {\n    check(user) && verify(token)\n}\n\nfn logout() {}\n",
        );
        let readme = blob(b"# auth\n\nToken-based login.\n\n## Usage\n\n    login(user, token)\n");
        let after2 = blob(
            b"fn login(user: &str, token: &str) -> bool {\n    check(user) && verify(token)\n}\n\nfn logout() {}\n\nfn refresh() {}\n",
        );
        let moved = blob(b"pub mod session;\n");
        let uncaptured = blob(b"[package]\nname = \"auth\"\n");
        let file_event = |ts: i64, summary: &str, after: &hh_core::PutOutcome| {
            let mut e = ev(
                &sid,
                ts,
                EventKind::FileChange,
                summary,
                serde_json::json!({}),
            );
            e.body_json = None;
            e.blob_hash = Some(after.hash.clone());
            e.blob_size = Some(after.size);
            e
        };
        let w = store.event_writer().unwrap();
        w.append_event(ev(
            &sid,
            0,
            EventKind::UserMessage,
            "add token auth to login",
            serde_json::json!({"text": "add token auth to login"}),
        ))
        .unwrap();
        let call = w
            .append_event(ev(
                &sid,
                1_000,
                EventKind::ToolCall,
                "tool_call: Bash",
                serde_json::json!({"name": "Bash", "input": {"command": "cargo test"}}),
            ))
            .unwrap();
        let mut result = ev(
            &sid,
            4_200,
            EventKind::ToolResult,
            "tool_result: 2 tests failed",
            serde_json::json!({"is_error": true, "content": "2 tests failed"}),
        );
        result.correlates = Some(call);
        w.append_event(result).unwrap();
        w.append_event(ev(
            &sid,
            5_000,
            EventKind::AgentMessage,
            "I'll add a token parameter.",
            serde_json::json!({
                "text": "I'll add a token parameter.",
                "model": "claude-opus-4-8", "message_id": "msg_1",
                "usage": {"input_tokens": 1200, "output_tokens": 340,
                          "cache_read_input_tokens": 48000, "cache_creation_input_tokens": 2000}
            }),
        ))
        .unwrap();
        w.append_file_change_with_provenance(
            file_event(6_000, "modified src/auth.rs", &after),
            FileChange {
                event_id: 0,
                path: "src/auth.rs".into(),
                change_kind: ChangeKind::Modified,
                before_hash: Some(before.hash.clone()),
                after_hash: Some(after.hash.clone()),
                is_binary: false,
            },
            FileChangeProvenance::new(BeforeSource::Git).with_before_size(Some(before.size)),
        )
        .unwrap();
        w.append_file_change_with_provenance(
            file_event(66_000, "created README.md", &readme),
            FileChange {
                event_id: 0,
                path: "README.md".into(),
                change_kind: ChangeKind::Created,
                before_hash: None,
                after_hash: Some(readme.hash.clone()),
                is_binary: false,
            },
            FileChangeProvenance::new(BeforeSource::None),
        )
        .unwrap();
        w.append_file_change_with_provenance(
            file_event(67_000, "renamed src/mod.rs", &moved),
            FileChange {
                event_id: 0,
                path: "src/lib.rs".into(),
                change_kind: ChangeKind::Modified,
                before_hash: Some(moved.hash.clone()),
                after_hash: Some(moved.hash.clone()),
                is_binary: false,
            },
            FileChangeProvenance::new(BeforeSource::Live)
                .with_renamed_from(Some("src/mod.rs".into())),
        )
        .unwrap();
        w.append_file_change_with_provenance(
            file_event(68_000, "modified Cargo.toml", &uncaptured),
            FileChange {
                event_id: 0,
                path: "Cargo.toml".into(),
                change_kind: ChangeKind::Modified,
                before_hash: None,
                after_hash: Some(uncaptured.hash.clone()),
                is_binary: false,
            },
            FileChangeProvenance::new(BeforeSource::None),
        )
        .unwrap();
        w.append_file_change_with_provenance(
            file_event(69_000, "modified src/auth.rs", &after2),
            FileChange {
                event_id: 0,
                path: "src/auth.rs".into(),
                change_kind: ChangeKind::Modified,
                before_hash: Some(after.hash.clone()),
                after_hash: Some(after2.hash.clone()),
                is_binary: false,
            },
            FileChangeProvenance::new(BeforeSource::Live),
        )
        .unwrap();
        w.append_event(ev(
            &sid,
            70_000,
            EventKind::Error,
            "error: rate limited",
            serde_json::json!({"message": "rate limited", "retry_after_s": 30}),
        ))
        .unwrap();
        w.append_event(ev(
            &sid,
            72_000,
            EventKind::AgentMessage,
            "Done: login now takes a token.",
            serde_json::json!({"text": "Done: login now takes a token."}),
        ))
        .unwrap();
        w.finish().unwrap();
        store.assign_steps(&sid).unwrap();
        store
            .finalize_session(&sid, 1_700_000_075_000, Some(0), SessionStatus::Ok)
            .unwrap();
        let mut session = store.get_session(&sid).unwrap();
        session.short_id = "a1b2c3".into();
        let index = store.list_event_index(&sid).unwrap();
        let insights = Insights::load(&store, &session);
        let app = AppState::with_insights(session, index, insights);
        (tmp, app, ReplayData::new(store))
    }

    fn select_label(app: &mut AppState, label: &str) {
        update(app, AppEvent::Key(KeyInput::Char('g')));
        for _ in 0..50 {
            if app.selected_row().unwrap().label() == label {
                return;
            }
            update(app, AppEvent::Key(KeyInput::Char('j')));
        }
        panic!("no row labelled {label}");
    }

    const PALETTES: [(Palette, &str); 2] = [(Palette::Dark, "dark"), (Palette::Light, "light")];

    #[test]
    fn snapshot_timeline() {
        for (palette, name) in PALETTES {
            let (_tmp, mut app, mut data) = rich_fixture();
            insta::assert_snapshot!(
                format!("timeline_{name}"),
                draw_to(&mut app, &mut data, palette)
            );
        }
    }

    #[test]
    fn snapshot_detail_tool_call() {
        for (palette, name) in PALETTES {
            let (_tmp, mut app, mut data) = rich_fixture();
            select_label(&mut app, "tool_call: Bash");
            insta::assert_snapshot!(
                format!("detail_{name}"),
                draw_to(&mut app, &mut data, palette)
            );
        }
    }

    #[test]
    fn snapshot_diff_zoomed() {
        for (palette, name) in PALETTES {
            let (_tmp, mut app, mut data) = rich_fixture();
            update(&mut app, AppEvent::Key(KeyInput::Char('d')));
            assert_eq!(app.selected_row().unwrap().label(), "modified src/auth.rs");
            update(&mut app, AppEvent::Key(KeyInput::Enter));
            insta::assert_snapshot!(
                format!("diff_{name}"),
                draw_to(&mut app, &mut data, palette)
            );
        }
    }

    #[test]
    fn snapshot_diff_missing_before_side_and_rename() {
        let (_tmp, mut app, mut data) = rich_fixture();
        select_label(&mut app, "modified Cargo.toml");
        let missing = draw_to(&mut app, &mut data, Palette::Dark);
        assert!(missing.contains("before content not captured"), "{missing}");
        insta::assert_snapshot!("diff_missing_before_dark", missing);
        select_label(&mut app, "renamed src/mod.rs");
        let renamed = draw_to(&mut app, &mut data, Palette::Dark);
        assert!(renamed.contains("src/mod.rs → src/lib.rs"), "{renamed}");
        assert!(renamed.contains("renamed · content unchanged"), "{renamed}");
    }

    #[test]
    fn snapshot_files_touched() {
        for (palette, name) in PALETTES {
            let (_tmp, mut app, mut data) = rich_fixture();
            update(&mut app, AppEvent::Key(KeyInput::Char('f')));
            insta::assert_snapshot!(
                format!("files_{name}"),
                draw_to(&mut app, &mut data, palette)
            );
        }
    }

    #[test]
    fn files_touched_opens_through_to_that_files_diffs() {
        let (_tmp, mut app, mut data) = rich_fixture();
        update(&mut app, AppEvent::Key(KeyInput::Char('f')));
        update(&mut app, AppEvent::Key(KeyInput::Enter));
        let text = draw_to(&mut app, &mut data, Palette::Dark);
        assert!(text.contains(" src/auth.rs · 1/2 "), "{text}");
        assert!(!text.contains("README.md"), "{text}");
        assert!(text.contains("+2 −2"), "{text}");
    }

    #[test]
    fn snapshot_help_overlay() {
        for (palette, name) in PALETTES {
            let (_tmp, mut app, mut data) = rich_fixture();
            update(&mut app, AppEvent::Key(KeyInput::Char('?')));
            insta::assert_snapshot!(
                format!("help_{name}"),
                draw_to(&mut app, &mut data, palette)
            );
        }
    }

    #[test]
    fn snapshot_timeline_no_color() {
        let (_tmp, mut app, mut data) = rich_fixture();
        insta::assert_snapshot!("timeline_mono", draw_to(&mut app, &mut data, Palette::Mono));
    }

    /// The dash convention: a session with no model, no usage, no capture
    /// tier and no end time renders every unknown header value as `—`,
    /// never `0`, and admits it has no transcript.
    #[test]
    fn snapshot_unknown_values_render_as_dashes() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let sid = store
            .create_session(&NewSession {
                id: hh_core::event::now_v7(),
                started_at: 0,
                agent_kind: AgentKind::Generic,
                adapter_status: AdapterStatus::None,
                command: vec!["sh".into(), "agent.sh".into()],
                cwd: PathBuf::from("/tmp/work"),
                hostname: None,
                hh_version: "test".into(),
                model: None,
                git_branch: None,
                git_sha: None,
                git_dirty: None,
            })
            .unwrap()
            .id;
        let w = store.event_writer().unwrap();
        w.append_event(ev(
            &sid,
            0,
            EventKind::TerminalOutput,
            "terminal chunk",
            serde_json::json!({"text": "working...\n"}),
        ))
        .unwrap();
        w.finish().unwrap();
        let mut session = store.get_session(&sid).unwrap();
        session.short_id = "d4e5f6".into();
        let index = store.list_event_index(&sid).unwrap();
        let insights = Insights::load(&store, &session);
        let mut app = AppState::with_insights(session, index, insights);
        let mut data = ReplayData::new(store);
        for (palette, name) in PALETTES {
            let text = draw_to(&mut app, &mut data, palette);
            let header: String = text.lines().take(3).collect::<Vec<_>>().join("\n");
            let facts: String = text.lines().skip(1).take(2).collect::<Vec<_>>().join("\n");
            for label in [
                "model",
                "provider",
                "duration",
                "tokens",
                "est. cost",
                "capture",
            ] {
                let line = facts
                    .lines()
                    .find(|l| l.contains(label))
                    .unwrap_or_else(|| panic!("no {label} in header:\n{header}"));
                let after = &line[line.find(label).unwrap() + label.len()..];
                assert!(
                    after.trim_start().starts_with('—'),
                    "{label} must be a dash, not a number:\n{header}"
                );
            }
            assert!(header.contains("no transcript"), "{header}");
            insta::assert_snapshot!(format!("unknown_values_{name}"), text);
        }
    }

    /// Every binding in the help overlay is documented in the keybinding
    /// table of `docs/replay-inspect.md`.
    #[test]
    fn docs_list_every_keybinding() {
        let docs = include_str!("../../../docs/replay-inspect.md");
        for (_, bindings) in KEYMAP {
            for (key, _) in *bindings {
                assert!(
                    docs.contains(&format!("| `{key}` |")),
                    "docs/replay-inspect.md is missing the `{key}` binding"
                );
            }
        }
    }

    #[test]
    fn compact_durations_fit_six_columns() {
        for (ms, want) in [
            (0, "0ms"),
            (350, "350ms"),
            (4_250, "4.2s"),
            (42_000, "42s"),
            (245_000, "4m05s"),
            (7_500_000, "2h05m"),
        ] {
            assert_eq!(compact_ms(ms), want);
            assert!(want.chars().count() <= 6);
        }
    }

    /// Rendering a frame touches only the visible window: a 50,000-row
    /// session draws without loading any event body beyond the selected one.
    #[test]
    fn large_session_draws_visible_window_only() {
        let index: Vec<hh_core::EventIndexRow> = (0..50_000)
            .map(|i| hh_core::EventIndexRow {
                id: i + 1,
                ts_ms: i * 10,
                kind: EventKind::AgentMessage,
                step: Some(i + 1),
                correlates: None,
                summary: format!("message {i}"),
            })
            .collect();
        let mut session = hh_core::SessionRow::default();
        session.short_id = "big000".into();
        let mut app = AppState::new(session, index);
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let mut data = ReplayData::new(store);
        update(&mut app, AppEvent::Key(KeyInput::Char('G')));
        let text = draw_to(&mut app, &mut data, Palette::Dark);
        assert!(text.contains("message 49999"), "{text}");
        assert!(text.contains("50000/50000"), "{text}");
    }
}
