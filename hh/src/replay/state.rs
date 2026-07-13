//! Pure application state and reducer for the replay TUI (FR-3).
//!
//! `AppState` holds everything the UI needs to render and nothing that needs
//! a terminal: no ratatui `Frame`, no crossterm event types. [`update`] is a
//! plain `fn(&mut AppState, AppEvent)` reducer, so the keymap, filter, jump,
//! and terminal-toggle logic are unit-testable without spawning a real
//! terminal (CLAUDE.md testing standard).

use hh_core::{EventKind, SessionRow, TimelineRow};

/// Terminal-independent input the reducer understands. The event loop
/// (`super::run`) translates crossterm `KeyEvent`s into these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyInput {
    /// A printable character.
    Char(char),
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Tab,
}

/// Top-level events the reducer consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEvent {
    /// A key press.
    Key(KeyInput),
    /// The terminal was resized to (columns, rows). Currently only recorded
    /// for the render layer to react to; the reducer does not need to act on
    /// it beyond keeping the viewport height used for paging in sync.
    Resize(u16, u16),
}

/// Which pane has keyboard focus (`Tab` switches). Timeline focus moves the
/// selection; Detail focus scrolls the detail pane's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Timeline,
    Detail,
}

/// The current input mode. Only one modal prompt is active at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    /// Editing the `/` filter query (live-updating).
    Filter,
    /// Editing the `J` jump-to-timestamp prompt.
    Jump,
    /// The `?` full-keymap help overlay is open.
    Help,
}

/// How many rows a `PageUp`/`PageDown` keystroke moves the selection by, when
/// the real viewport height is not yet known (e.g. in tests). The render
/// layer overrides this via [`AppState::set_viewport_rows`] once it knows the
/// timeline pane's actual height, so paging matches what's on screen.
const DEFAULT_PAGE_SIZE: usize = 10;

/// The full, terminal-independent state of the replay TUI.
pub struct AppState {
    /// The session being replayed (header data).
    pub session: SessionRow,
    /// The full per-event index, eagerly loaded once (FR-3.5): the source
    /// [`build_timeline`](hh_core::build_timeline) rebuilds `base_rows` from
    /// whenever [`Mode`]-independent view options change (currently just the
    /// terminal-segment toggle).
    pub index: Vec<hh_core::EventIndexRow>,
    /// Timeline rows for the current `show_terminal` setting, pre-filter.
    base_rows: Vec<TimelineRow>,
    /// Indices into `base_rows` that pass the current filter (all of them
    /// when no filter is active).
    visible: Vec<usize>,
    /// Position within `visible` of the selected row.
    selected: usize,
    /// Scroll offset (in rows) of the timeline viewport, kept in sync with
    /// `selected` so the selection is always on screen.
    pub timeline_scroll: usize,
    /// Scroll offset (in lines) within the detail pane's rendered content.
    pub detail_scroll: usize,
    /// Which pane has focus.
    pub focus: Focus,
    /// The current input mode.
    pub mode: Mode,
    /// Live filter query text (edited in [`Mode::Filter`]).
    pub filter_input: String,
    /// Live jump-to-timestamp text (edited in [`Mode::Jump`]).
    pub jump_input: String,
    /// Whether `terminal_output` runs are shown as their own rows (`t`).
    pub show_terminal: bool,
    /// A transient one-line status/error message (e.g. an invalid jump time).
    pub status: Option<String>,
    /// The last-known timeline viewport height in rows, for paging.
    viewport_rows: usize,
    /// Set once the user asks to quit (`q`).
    pub should_quit: bool,
}

impl AppState {
    /// Build the initial state from a session header and its full event
    /// index (FR-3.5 eager load). Terminal segments start hidden.
    #[must_use]
    pub fn new(session: SessionRow, index: Vec<hh_core::EventIndexRow>) -> Self {
        let base_rows = hh_core::build_timeline(&index, false);
        let visible = (0..base_rows.len()).collect();
        Self {
            session,
            index,
            base_rows,
            visible,
            selected: 0,
            timeline_scroll: 0,
            detail_scroll: 0,
            focus: Focus::default(),
            mode: Mode::default(),
            filter_input: String::new(),
            jump_input: String::new(),
            show_terminal: false,
            status: None,
            viewport_rows: DEFAULT_PAGE_SIZE,
            should_quit: false,
        }
    }

    /// Tell the reducer the timeline pane's actual height in rows, so
    /// `PageUp`/`PageDown` and scroll clamping match the real viewport. Cheap
    /// to call every frame; a no-op when unchanged.
    pub fn set_viewport_rows(&mut self, rows: usize) {
        self.viewport_rows = rows.max(1);
    }

    /// The currently selected row, if any (empty timeline has none).
    #[must_use]
    pub fn selected_row(&self) -> Option<&TimelineRow> {
        self.visible
            .get(self.selected)
            .and_then(|&idx| self.base_rows.get(idx))
    }

    /// The 0-based position of the selection within the visible rows, and the
    /// total visible row count — used by the render layer for a scrollbar.
    #[must_use]
    pub fn selection_position(&self) -> (usize, usize) {
        (self.selected, self.visible.len())
    }

    /// All currently visible rows, in display order.
    #[must_use]
    pub fn visible_rows(&self) -> Vec<&TimelineRow> {
        self.visible
            .iter()
            .filter_map(|&idx| self.base_rows.get(idx))
            .collect()
    }

    /// Rebuild `base_rows` for the current `show_terminal` setting, keeping
    /// the selection anchored to the same underlying event across the rebuild
    /// (a plain `recompute_visible()` afterward would compare against the
    /// *new* `base_rows` at the *old* `selected` index — wrong row).
    fn toggle_terminal_and_rebuild(&mut self) {
        let anchor = self
            .selected_row()
            .and_then(|r| r.event_ids().first().copied());
        self.base_rows = hh_core::build_timeline(&self.index, self.show_terminal);
        self.apply_visible(anchor);
    }

    /// Recompute `visible` from `base_rows` and the current filter text,
    /// anchoring the selection to the row currently selected (identified by
    /// its first event id) when it still matches.
    fn recompute_visible(&mut self) {
        let anchor = self
            .selected_row()
            .and_then(|r| r.event_ids().first().copied());
        self.apply_visible(anchor);
    }

    /// Recompute `visible` from `base_rows` and the current filter text,
    /// restoring the selection to the row whose first event id is `anchor`
    /// when one is given and it still passes the filter, else clamping.
    fn apply_visible(&mut self, anchor: Option<i64>) {
        let query = self.filter_input.trim().to_ascii_lowercase();
        self.visible = if query.is_empty() {
            (0..self.base_rows.len()).collect()
        } else {
            self.base_rows
                .iter()
                .enumerate()
                .filter(|(_, row)| matches_filter(row, &query))
                .map(|(i, _)| i)
                .collect()
        };
        self.selected = anchor
            .and_then(|id| {
                self.visible
                    .iter()
                    .position(|&idx| self.base_rows[idx].event_ids().first() == Some(&id))
            })
            .unwrap_or(0)
            .min(self.visible.len().saturating_sub(1));
        self.detail_scroll = 0;
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        if self.selected < self.timeline_scroll {
            self.timeline_scroll = self.selected;
        }
        let last_visible_row = self.timeline_scroll + self.viewport_rows.saturating_sub(1);
        if self.selected > last_visible_row {
            self.timeline_scroll = self.selected + 1 - self.viewport_rows;
        }
    }

    fn move_selection(&mut self, delta: i64) {
        if self.visible.is_empty() {
            return;
        }
        let max = i64::try_from(self.visible.len() - 1).unwrap_or(i64::MAX);
        let current = i64::try_from(self.selected).unwrap_or(0);
        let new = (current + delta).clamp(0, max);
        // `new` is clamped to [0, max] above, both non-negative, and `max`
        // came from a `usize` — the round trip back cannot lose information.
        self.selected = usize::try_from(new).unwrap_or(0);
        self.detail_scroll = 0;
        self.clamp_scroll();
    }

    /// Move the selection to the next row whose kind is `FileChange` (the
    /// `d` key: "next diff"), wrapping around to the start if none is found
    /// after the current position.
    fn jump_next_diff(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        let n = self.visible.len();
        for step in 1..=n {
            let candidate = (self.selected + step) % n;
            let idx = self.visible[candidate];
            if self.base_rows[idx].kind() == EventKind::FileChange {
                self.selected = candidate;
                self.detail_scroll = 0;
                self.clamp_scroll();
                return;
            }
        }
    }

    /// Jump the selection to the row nearest `target_ms` (the `J` key, once
    /// its input parses). Selects the last row at or before the target if
    /// one exists, otherwise the first row after it.
    fn jump_to_ts(&mut self, target_ms: i64) {
        if self.visible.is_empty() {
            return;
        }
        let mut best_before: Option<usize> = None;
        let mut first_after: Option<usize> = None;
        for (pos, &idx) in self.visible.iter().enumerate() {
            let ts = self.base_rows[idx].ts_ms();
            if ts <= target_ms {
                best_before = Some(pos);
            } else if first_after.is_none() {
                first_after = Some(pos);
            }
        }
        self.selected = best_before.or(first_after).unwrap_or(0);
        self.detail_scroll = 0;
        self.clamp_scroll();
    }
}

/// Case-insensitive substring match over a row's badge kind name or label
/// text (FR-3.3 `/` filter). `query` must already be lowercased.
fn matches_filter(row: &TimelineRow, query: &str) -> bool {
    let kind_name = super::kind::badge_label(row.kind()).to_ascii_lowercase();
    if kind_name.contains(query) {
        return true;
    }
    row.label().to_ascii_lowercase().contains(query)
}

/// Parse a `J`-prompt time as `mm:ss` or `hh:mm:ss` into milliseconds.
/// Returns `None` for anything else (non-numeric parts, wrong component
/// count, out-of-range minutes/seconds).
#[must_use]
pub fn parse_jump_time(input: &str) -> Option<i64> {
    let parts: Vec<&str> = input.trim().split(':').collect();
    let (h, m, s) = match parts.as_slice() {
        [m, s] => (0i64, m, s),
        [h, m, s] => (h.parse().ok()?, m, s),
        _ => return None,
    };
    let m: i64 = m.parse().ok()?;
    let s: i64 = s.parse().ok()?;
    if !(0..60).contains(&m) || !(0..60).contains(&s) {
        return None;
    }
    Some(((h * 3600 + m * 60 + s) * 1000).max(0))
}

/// Drive the reducer: apply one [`AppEvent`] to `state`, mutating it in
/// place. Pure aside from the mutation — no I/O, no terminal.
pub fn update(state: &mut AppState, event: AppEvent) {
    match event {
        AppEvent::Resize(_cols, rows) => {
            // Matches the render layer's outer layout: 3 header rows + 1
            // status row + 2 timeline-pane border rows = 6 non-content rows.
            state.set_viewport_rows(usize::from(rows).saturating_sub(6));
        }
        AppEvent::Key(key) => match state.mode {
            Mode::Normal => handle_normal(state, key),
            Mode::Filter => handle_filter(state, key),
            Mode::Jump => handle_jump(state, key),
            Mode::Help => handle_help(state, key),
        },
    }
}

fn handle_normal(state: &mut AppState, key: KeyInput) {
    match key {
        KeyInput::Char('q') => state.should_quit = true,
        KeyInput::Char('j') | KeyInput::Down => match state.focus {
            Focus::Timeline => state.move_selection(1),
            Focus::Detail => state.detail_scroll += 1,
        },
        KeyInput::Char('k') | KeyInput::Up => match state.focus {
            Focus::Timeline => state.move_selection(-1),
            Focus::Detail => state.detail_scroll = state.detail_scroll.saturating_sub(1),
        },
        KeyInput::PageDown => {
            let n = state.viewport_rows_i64();
            state.move_selection(n);
        }
        KeyInput::PageUp => {
            let n = state.viewport_rows_i64();
            state.move_selection(-n);
        }
        KeyInput::Char('g') | KeyInput::Home => {
            state.selected = 0;
            state.detail_scroll = 0;
            state.clamp_scroll();
        }
        KeyInput::Char('G') | KeyInput::End => {
            state.selected = state.visible.len().saturating_sub(1);
            state.detail_scroll = 0;
            state.clamp_scroll();
        }
        KeyInput::Char('J') => {
            state.mode = Mode::Jump;
            state.jump_input.clear();
            state.status = None;
        }
        KeyInput::Char('/') => {
            state.mode = Mode::Filter;
            state.status = None;
        }
        KeyInput::Char('t') => {
            state.show_terminal = !state.show_terminal;
            state.toggle_terminal_and_rebuild();
        }
        KeyInput::Char('d') => state.jump_next_diff(),
        KeyInput::Char('?') => state.mode = Mode::Help,
        KeyInput::Tab => {
            state.focus = match state.focus {
                Focus::Timeline => Focus::Detail,
                Focus::Detail => Focus::Timeline,
            };
        }
        KeyInput::Esc if !state.filter_input.is_empty() => {
            state.filter_input.clear();
            state.recompute_visible();
        }
        _ => {}
    }
}

fn handle_filter(state: &mut AppState, key: KeyInput) {
    match key {
        KeyInput::Char(c) => {
            state.filter_input.push(c);
            state.recompute_visible();
        }
        KeyInput::Backspace => {
            state.filter_input.pop();
            state.recompute_visible();
        }
        KeyInput::Enter => state.mode = Mode::Normal,
        KeyInput::Esc => {
            state.filter_input.clear();
            state.recompute_visible();
            state.mode = Mode::Normal;
        }
        _ => {}
    }
}

fn handle_jump(state: &mut AppState, key: KeyInput) {
    match key {
        KeyInput::Char(c) if c.is_ascii_digit() || c == ':' => state.jump_input.push(c),
        KeyInput::Backspace => {
            state.jump_input.pop();
        }
        KeyInput::Enter => {
            match parse_jump_time(&state.jump_input) {
                Some(ms) => {
                    state.jump_to_ts(ms);
                    state.status = None;
                }
                None => {
                    state.status = Some(format!(
                        "invalid time `{}` — expected mm:ss or hh:mm:ss",
                        state.jump_input
                    ));
                }
            }
            state.mode = Mode::Normal;
        }
        KeyInput::Esc => {
            state.jump_input.clear();
            state.mode = Mode::Normal;
        }
        _ => {}
    }
}

fn handle_help(state: &mut AppState, _key: KeyInput) {
    // Any key closes the overlay (Esc/`?`/q/Enter are the documented ones,
    // but there is no reason to special-case them over anything else).
    state.mode = Mode::Normal;
}

impl AppState {
    fn viewport_rows_i64(&self) -> i64 {
        i64::try_from(self.viewport_rows).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hh_core::{AdapterStatus, AgentKind, EventIndexRow, SessionStatus};
    use std::path::PathBuf;

    fn session() -> SessionRow {
        // `SessionRow` is `#[non_exhaustive]`; see the comment in
        // `hh/src/main.rs`'s `row()` fixture for why this uses
        // `SessionRow::default()` + field assignment rather than a struct
        // literal (even with `..` update syntax).
        let mut r = SessionRow::default();
        r.id = "s".into();
        r.short_id = "abcdef".into();
        r.started_at = 0;
        r.ended_at = None;
        r.exit_code = None;
        r.status = SessionStatus::Ok;
        r.agent_kind = AgentKind::Generic;
        r.adapter_status = AdapterStatus::None;
        r.command = vec!["claude".into()];
        r.cwd = PathBuf::from("/tmp");
        r.step_count = 0;
        r.files_changed = 0;
        r
    }

    fn idx(
        id: i64,
        ts_ms: i64,
        kind: EventKind,
        step: Option<i64>,
        summary: &str,
    ) -> EventIndexRow {
        EventIndexRow {
            id,
            ts_ms,
            kind,
            step,
            correlates: None,
            summary: summary.to_string(),
        }
    }

    fn sample_state() -> AppState {
        let index = vec![
            idx(
                1,
                0,
                EventKind::UserMessage,
                Some(1),
                "please list the directory",
            ),
            idx(2, 1000, EventKind::ToolCall, Some(2), "tool_call: Bash"),
            idx(3, 1500, EventKind::ToolResult, Some(2), "tool_result: ok"),
            idx(4, 2000, EventKind::AgentMessage, Some(3), "done listing"),
            idx(
                5,
                2500,
                EventKind::FileChange,
                Some(4),
                "modified Cargo.toml",
            ),
            idx(6, 100, EventKind::TerminalOutput, None, "chunk"),
        ];
        AppState::new(session(), index)
    }

    #[test]
    fn terminal_hidden_by_default_in_state() {
        let s = sample_state();
        assert_eq!(s.visible_rows().len(), 4, "4 steps; terminal_output hidden");
    }

    #[test]
    fn toggle_terminal_shows_segment_and_preserves_selection() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('j'))); // select step 2
        let selected_id_before = s.selected_row().unwrap().event_ids().to_vec();
        update(&mut s, AppEvent::Key(KeyInput::Char('t')));
        assert_eq!(s.visible_rows().len(), 5, "terminal segment now shown");
        assert_eq!(
            s.selected_row().unwrap().event_ids(),
            selected_id_before,
            "selection follows the same underlying row across the toggle"
        );
    }

    #[test]
    fn j_k_move_selection_and_clamp() {
        let mut s = sample_state();
        assert_eq!(s.selection_position().0, 0);
        update(&mut s, AppEvent::Key(KeyInput::Char('k'))); // already at top
        assert_eq!(s.selection_position().0, 0);
        for _ in 0..10 {
            update(&mut s, AppEvent::Key(KeyInput::Char('j')));
        }
        assert_eq!(s.selection_position().0, 3, "clamped to last row (4 rows)");
    }

    #[test]
    fn g_and_shift_g_jump_to_ends() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('G')));
        assert_eq!(s.selection_position().0, 3);
        update(&mut s, AppEvent::Key(KeyInput::Char('g')));
        assert_eq!(s.selection_position().0, 0);
    }

    #[test]
    fn filter_live_updates_and_esc_clears() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('/')));
        assert_eq!(s.mode, Mode::Filter);
        for c in "file".chars() {
            update(&mut s, AppEvent::Key(KeyInput::Char(c)));
        }
        assert_eq!(
            s.visible_rows().len(),
            1,
            "only the FileChange row matches `file`"
        );
        update(&mut s, AppEvent::Key(KeyInput::Esc));
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(s.visible_rows().len(), 4, "Esc clears the filter");
    }

    #[test]
    fn filter_matches_kind_keyword() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('/')));
        for c in "tool".chars() {
            update(&mut s, AppEvent::Key(KeyInput::Char(c)));
        }
        assert_eq!(s.visible_rows().len(), 1, "the TOOL-badged step matches");
    }

    #[test]
    fn filter_matches_summary_substring() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('/')));
        for c in "listing".chars() {
            update(&mut s, AppEvent::Key(KeyInput::Char(c)));
        }
        assert_eq!(s.visible_rows().len(), 1);
        assert_eq!(s.visible_rows()[0].label(), "done listing");
    }

    #[test]
    fn jump_parses_mmss_and_hhmmss() {
        assert_eq!(parse_jump_time("01:05"), Some(65_000));
        assert_eq!(parse_jump_time("01:00:00"), Some(3_600_000));
        assert_eq!(parse_jump_time("0:02"), Some(2000));
        assert_eq!(parse_jump_time("not-a-time"), None);
        assert_eq!(parse_jump_time("1:99"), None, "seconds out of range");
        assert_eq!(parse_jump_time("1:2:3:4"), None, "too many components");
    }

    #[test]
    fn jump_key_selects_nearest_row() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('J')));
        assert_eq!(s.mode, Mode::Jump);
        for c in "00:02".chars() {
            update(&mut s, AppEvent::Key(KeyInput::Char(c)));
        }
        update(&mut s, AppEvent::Key(KeyInput::Enter));
        assert_eq!(s.mode, Mode::Normal);
        // 2000ms lands exactly on the AgentMessage step (index 2 of 4 visible).
        assert_eq!(s.selected_row().unwrap().label(), "done listing");
    }

    #[test]
    fn jump_invalid_time_sets_status_and_returns_to_normal() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('J')));
        for c in "nope".chars() {
            update(&mut s, AppEvent::Key(KeyInput::Char(c)));
        }
        update(&mut s, AppEvent::Key(KeyInput::Enter));
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.status.is_some());
    }

    #[test]
    fn jump_esc_cancels_without_moving_selection() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('G')));
        let before = s.selection_position().0;
        update(&mut s, AppEvent::Key(KeyInput::Char('J')));
        for c in "00:00".chars() {
            update(&mut s, AppEvent::Key(KeyInput::Char(c)));
        }
        update(&mut s, AppEvent::Key(KeyInput::Esc));
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(
            s.selection_position().0,
            before,
            "Esc must not apply the jump"
        );
    }

    #[test]
    fn d_jumps_to_next_diff_and_wraps() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('d')));
        assert_eq!(s.selected_row().unwrap().label(), "modified Cargo.toml");
        update(&mut s, AppEvent::Key(KeyInput::Char('d')));
        assert_eq!(
            s.selected_row().unwrap().label(),
            "modified Cargo.toml",
            "wraps around to the only FileChange row"
        );
    }

    #[test]
    fn tab_switches_focus() {
        let mut s = sample_state();
        assert_eq!(s.focus, Focus::Timeline);
        update(&mut s, AppEvent::Key(KeyInput::Tab));
        assert_eq!(s.focus, Focus::Detail);
        update(&mut s, AppEvent::Key(KeyInput::Tab));
        assert_eq!(s.focus, Focus::Timeline);
    }

    #[test]
    fn detail_focus_scrolls_instead_of_selecting() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Tab));
        update(&mut s, AppEvent::Key(KeyInput::Char('j')));
        update(&mut s, AppEvent::Key(KeyInput::Char('j')));
        assert_eq!(s.detail_scroll, 2);
        assert_eq!(
            s.selection_position().0,
            0,
            "selection unchanged while Detail is focused"
        );
    }

    #[test]
    fn help_overlay_opens_and_closes() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('?')));
        assert_eq!(s.mode, Mode::Help);
        update(&mut s, AppEvent::Key(KeyInput::Esc));
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn q_quits() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('q')));
        assert!(s.should_quit);
    }

    #[test]
    fn q_while_filtering_types_the_letter_not_quit() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('/')));
        update(&mut s, AppEvent::Key(KeyInput::Char('q')));
        assert!(!s.should_quit);
        assert_eq!(s.filter_input, "q");
    }

    #[test]
    fn resize_updates_viewport_for_paging() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Resize(80, 12));
        update(&mut s, AppEvent::Key(KeyInput::PageDown));
        assert_eq!(
            s.selection_position().0,
            3,
            "page size clamps to the 4 available rows"
        );
    }
}
