//! Pure application state and reducer for the replay TUI (FR-3).
//!
//! `AppState` holds everything the UI needs to render and nothing that needs
//! a terminal: no ratatui `Frame`, no crossterm event types. [`update`] is a
//! plain `fn(&mut AppState, AppEvent)` reducer, so the keymap, filter, jump,
//! moments, files view and zoom logic are unit-testable without spawning a
//! real terminal (CLAUDE.md testing standard).
//!
//! Memory: the per-event *index* (id, timestamp, kind, step, one-line
//! summary) is held for the whole session, as before; event *bodies* and
//! blob content are fetched lazily per selected row (see `super::data`).
//! Everything computed here is a single pass over the index.

use super::insights::{FileGroup, Insights};
use hh_core::{AdapterStatus, EventKind, SessionRow, TimelineRow};
use std::collections::HashSet;

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
    /// The terminal was resized to (columns, rows); keeps the paging
    /// viewport height in sync.
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

/// Which list fills the left pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// The step timeline.
    #[default]
    Timeline,
    /// Every file the session changed (`f`).
    Files,
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

/// Why a moment is worth seeing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MomentKind {
    /// An `error` event.
    Error,
    /// A tool result the agent reported as failed.
    FailedTool,
    /// The step that took longest.
    SlowestStep,
    /// The file change with the most changed lines.
    LargestDiff,
}

impl MomentKind {
    /// Short human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::FailedTool => "failed tool call",
            Self::SlowestStep => "slowest step",
            Self::LargestDiff => "largest diff",
        }
    }
}

/// One interesting moment the `n`/`N` keys cycle through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moment {
    /// An event inside the moment's timeline row.
    pub event_id: i64,
    /// When it happened, ms since session start.
    pub ts_ms: i64,
    /// Why it matters.
    pub kind: MomentKind,
    /// A short detail (`4m12s`, `+120 −30 src/lib.rs`, the error summary).
    pub note: String,
}

/// A transient one-line message in the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The text.
    pub text: String,
    /// Whether it reports a problem (error style) or just information.
    pub is_error: bool,
}

impl Status {
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }
    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// How many rows a `PageUp`/`PageDown` keystroke moves the selection by, when
/// the real viewport height is not yet known (e.g. in tests).
const DEFAULT_PAGE_SIZE: usize = 10;

/// Rows of the header block, including its bottom rule.
pub const HEADER_ROWS: u16 = 4;

/// Rows the layout spends outside the list's content: header, status line,
/// and the list pane's top and bottom borders.
pub const CHROME_ROWS: u16 = HEADER_ROWS + 1 + 2;

/// A step no faster than this is never flagged slow, however it ranks.
const SLOW_FLOOR_MS: i64 = 1000;

/// The full, terminal-independent state of the replay TUI.
#[allow(clippy::struct_excessive_bools)] // independent UI toggles (zoom, terminal rows, structured, quit), not a state machine
pub struct AppState {
    /// The session being replayed (header data).
    pub session: SessionRow,
    /// The full per-event index, eagerly loaded once (FR-3.5).
    pub index: Vec<hh_core::EventIndexRow>,
    /// Session-wide facts computed before the first frame.
    pub insights: Insights,
    /// Whether the session has structured transcript events at all.
    pub structured: bool,
    /// The latest event timestamp, ms since session start.
    last_ts_ms: i64,
    /// Timeline rows for the current `show_terminal` setting, pre-filter.
    base_rows: Vec<TimelineRow>,
    /// Per-row duration, parallel to `base_rows` (`None` when unknown).
    durations: Vec<Option<i64>>,
    /// A step at or above this duration renders as slow.
    slow_threshold_ms: Option<i64>,
    /// Indices into `base_rows` that pass the current filters.
    visible: Vec<usize>,
    /// Position within `visible` of the selected row.
    selected: usize,
    /// Scroll offset (in rows) of the timeline viewport.
    pub timeline_scroll: usize,
    /// Scroll offset (in lines) within the detail pane's rendered content.
    pub detail_scroll: usize,
    /// Which pane has focus.
    pub focus: Focus,
    /// Which list fills the left pane.
    pub view: View,
    /// The detail pane fills the whole body (`Enter`).
    pub zoom: bool,
    /// The current input mode.
    pub mode: Mode,
    /// Live filter query text (edited in [`Mode::Filter`]).
    pub filter_input: String,
    /// Live jump-to-timestamp text (edited in [`Mode::Jump`]).
    pub jump_input: String,
    /// Whether `terminal_output` runs are shown as their own rows (`t`).
    pub show_terminal: bool,
    /// A transient one-line message.
    pub status: Option<Status>,
    /// The moments `n`/`N` cycle through, in time order.
    pub moments: Vec<Moment>,
    /// Index into `moments` of the last one jumped to.
    pub moment_cursor: Option<usize>,
    /// Event ids that mark a row as a failure (`✗`).
    failure_ids: HashSet<i64>,
    /// Event ids that mark a row as worth seeing (`●`).
    highlight_ids: HashSet<i64>,
    /// Selected row in the files view.
    pub files_selected: usize,
    /// Scroll offset of the files view.
    pub files_scroll: usize,
    /// The file (index into `insights.files`) the timeline is narrowed to.
    pub file_filter: Option<usize>,
    /// The event ids of `file_filter`'s file, for the visibility check.
    file_filter_ids: HashSet<i64>,
    /// The last-known list viewport height in rows, for paging.
    viewport_rows: usize,
    /// Set once the user asks to quit (`q`).
    pub should_quit: bool,
}

/// Where the last step ends: the session's last agent event if that is
/// after the step began, else the session's end. `None` when neither is
/// known — a running or crashed session's last step has no known length.
fn last_step_end(session: &SessionRow) -> impl Fn(i64) -> Option<i64> + '_ {
    move |start| {
        session
            .last_agent_ts_ms
            .filter(|&t| t > start)
            .or_else(|| session.duration_ms().filter(|&t| t >= start))
    }
}

/// Duration of each row: a step runs until the next step starts (terminal
/// rows do not end a step), the last step per [`last_step_end`]; a terminal
/// segment spans its own chunks.
fn row_durations(rows: &[TimelineRow], session: &SessionRow) -> Vec<Option<i64>> {
    let end_of_last = last_step_end(session);
    let mut out = vec![None; rows.len()];
    let mut next_step_ts: Option<i64> = None;
    for (i, row) in rows.iter().enumerate().rev() {
        match row {
            TimelineRow::Step(s) => {
                let end = next_step_ts.or_else(|| end_of_last(s.ts_ms));
                out[i] = end.map(|e| (e - s.ts_ms).max(0));
                next_step_ts = Some(s.ts_ms);
            }
            TimelineRow::Terminal(seg) => {
                out[i] = Some((seg.end_ts_ms - seg.start_ts_ms).max(0));
            }
        }
    }
    out
}

/// The duration at or above which a step renders as slow: the 90th
/// percentile of known step durations, but never under [`SLOW_FLOOR_MS`].
fn slow_threshold(rows: &[TimelineRow], durations: &[Option<i64>]) -> Option<i64> {
    let mut steps: Vec<i64> = rows
        .iter()
        .zip(durations)
        .filter(|(r, _)| matches!(r, TimelineRow::Step(_)))
        .filter_map(|(_, d)| *d)
        .collect();
    if steps.is_empty() {
        return None;
    }
    steps.sort_unstable();
    let idx = (steps.len() * 9) / 10;
    let p90 = steps[idx.min(steps.len() - 1)];
    Some(p90.max(SLOW_FLOOR_MS))
}

#[must_use]
/// A step duration in at most six columns: `350ms`, `4.2s`, `42s`,
/// `4m05s`, `2h05m`.
pub fn compact_ms(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        return format!("{ms}ms");
    }
    if ms < 10_000 {
        return format!("{}.{}s", ms / 1000, (ms % 1000) / 100);
    }
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m{:02}s", secs / 60, secs % 60);
    }
    format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
}

/// Whether the index holds any event only a structured source produces.
fn has_structured_events(index: &[hh_core::EventIndexRow]) -> bool {
    index.iter().any(|e| {
        matches!(
            e.kind,
            EventKind::UserMessage
                | EventKind::AgentMessage
                | EventKind::Thinking
                | EventKind::ToolCall
                | EventKind::ToolResult
                | EventKind::McpRequest
                | EventKind::McpResponse
                | EventKind::McpNotification
        )
    })
}

impl AppState {
    /// Build the initial state with no precomputed insights (tests).
    #[cfg(test)]
    #[must_use]
    pub fn new(session: SessionRow, index: Vec<hh_core::EventIndexRow>) -> Self {
        Self::with_insights(session, index, Insights::default())
    }

    /// Build the initial state from a session header, its full event index
    /// (FR-3.5 eager load), and the insights loaded before the first frame.
    /// Terminal segments start hidden.
    #[must_use]
    pub fn with_insights(
        session: SessionRow,
        index: Vec<hh_core::EventIndexRow>,
        insights: Insights,
    ) -> Self {
        let base_rows = hh_core::build_timeline(&index, false);
        let durations = row_durations(&base_rows, &session);
        let slow_threshold_ms = slow_threshold(&base_rows, &durations);
        let visible = (0..base_rows.len()).collect();
        let structured = match session.capture_tier.as_deref() {
            Some("transcript" | "mcp") => true,
            Some(_) => false,
            None => has_structured_events(&index),
        };
        let status = insights.load_error.clone().map(Status::error);
        let last_ts_ms = index.iter().map(|e| e.ts_ms).max().unwrap_or(0);
        let mut state = Self {
            session,
            index,
            insights,
            structured,
            last_ts_ms,
            base_rows,
            durations,
            slow_threshold_ms,
            visible,
            selected: 0,
            timeline_scroll: 0,
            detail_scroll: 0,
            focus: Focus::default(),
            view: View::default(),
            zoom: false,
            mode: Mode::default(),
            filter_input: String::new(),
            jump_input: String::new(),
            show_terminal: false,
            status,
            moments: Vec::new(),
            moment_cursor: None,
            failure_ids: HashSet::new(),
            highlight_ids: HashSet::new(),
            files_selected: 0,
            files_scroll: 0,
            file_filter: None,
            file_filter_ids: HashSet::new(),
            viewport_rows: DEFAULT_PAGE_SIZE,
            should_quit: false,
        };
        state.compute_moments();
        state
    }

    /// Collect the moments worth seeing: every error event, every failed
    /// tool call, the slowest step and the largest diff, in time order.
    fn compute_moments(&mut self) {
        let mut moments = Vec::new();
        for e in &self.index {
            if e.kind == EventKind::Error {
                moments.push(Moment {
                    event_id: e.id,
                    ts_ms: e.ts_ms,
                    kind: MomentKind::Error,
                    note: e.summary.clone(),
                });
                self.failure_ids.insert(e.id);
            } else if self.insights.failed_tool_results.contains(&e.id) {
                moments.push(Moment {
                    event_id: e.id,
                    ts_ms: e.ts_ms,
                    kind: MomentKind::FailedTool,
                    note: e.summary.clone(),
                });
                self.failure_ids.insert(e.id);
            }
        }
        let slowest = self
            .base_rows
            .iter()
            .zip(&self.durations)
            .filter_map(|(r, d)| match r {
                TimelineRow::Step(s) => d.map(|d| (s, d)),
                TimelineRow::Terminal(_) => None,
            })
            .filter(|(_, d)| *d >= SLOW_FLOOR_MS)
            // Longest wins; the earliest breaks a tie.
            .max_by_key(|(s, d)| (*d, std::cmp::Reverse(s.ts_ms)));
        if let Some((s, d)) = slowest {
            if let Some(&id) = s.event_ids.first() {
                moments.push(Moment {
                    event_id: id,
                    ts_ms: s.ts_ms,
                    kind: MomentKind::SlowestStep,
                    note: format!("{} · {}", compact_ms(d), s.summary),
                });
                self.highlight_ids.insert(id);
            }
        }
        if let Some(big) = &self.insights.largest_diff {
            let ts = self
                .index
                .iter()
                .find(|e| e.id == big.event_id)
                .map_or(0, |e| e.ts_ms);
            moments.push(Moment {
                event_id: big.event_id,
                ts_ms: ts,
                kind: MomentKind::LargestDiff,
                note: format!("+{} −{} {}", big.added, big.removed, big.path),
            });
            self.highlight_ids.insert(big.event_id);
        }
        moments.sort_by_key(|m| (m.ts_ms, m.event_id));
        moments.dedup_by_key(|m| (m.event_id, m.kind));
        self.moments = moments;
    }

    /// Tell the reducer the list pane's actual height in rows.
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

    /// All currently visible rows, in display order. Allocates one pointer
    /// per row; the render path uses [`Self::visible_window`] instead.
    #[cfg(test)]
    #[must_use]
    pub fn visible_rows(&self) -> Vec<&TimelineRow> {
        self.visible
            .iter()
            .filter_map(|&idx| self.base_rows.get(idx))
            .collect()
    }

    /// Up to `len` visible rows starting at position `start`, with each row's
    /// duration: what one frame of the timeline draws, without touching the
    /// rest of a long session.
    pub fn visible_window(
        &self,
        start: usize,
        len: usize,
    ) -> impl Iterator<Item = (usize, &TimelineRow, Option<i64>)> + '_ {
        self.visible
            .iter()
            .enumerate()
            .skip(start)
            .take(len)
            .filter_map(|(pos, &idx)| {
                self.base_rows
                    .get(idx)
                    .map(|r| (pos, r, self.durations.get(idx).copied().flatten()))
            })
    }

    /// Whether `duration` counts as slow for this session.
    #[must_use]
    pub fn is_slow(&self, duration: Option<i64>) -> bool {
        matches!((duration, self.slow_threshold_ms), (Some(d), Some(t)) if d >= t)
    }

    /// Whether a row contains a failure (an error or a failed tool call).
    #[must_use]
    pub fn row_failed(&self, row: &TimelineRow) -> bool {
        row.event_ids()
            .iter()
            .any(|id| self.failure_ids.contains(id))
    }

    /// Whether a row is one of the non-failure moments (slowest step,
    /// largest diff).
    #[must_use]
    pub fn row_highlighted(&self, row: &TimelineRow) -> bool {
        row.event_ids()
            .iter()
            .any(|id| self.highlight_ids.contains(id))
    }

    /// The file the timeline is narrowed to, if any.
    #[must_use]
    pub fn filtered_file(&self) -> Option<&FileGroup> {
        self.file_filter.and_then(|i| self.insights.files.get(i))
    }

    /// The largest step timestamp, for choosing the timestamp format.
    #[must_use]
    pub fn last_ts_ms(&self) -> i64 {
        self.last_ts_ms
    }

    /// The short phrases the header uses to admit what this recording lacks:
    /// no structured transcript, a degraded adapter, dropped events.
    #[must_use]
    pub fn capture_gaps(&self) -> Vec<String> {
        let mut gaps = Vec::new();
        if !self.structured {
            gaps.push("no transcript — terminal and file capture only".to_string());
        }
        if self.session.adapter_status == AdapterStatus::Degraded {
            gaps.push(match &self.session.adapter_degrade_reason {
                Some(r) => format!("transcript adapter degraded ({r})"),
                None => "transcript adapter degraded".to_string(),
            });
        }
        if self.session.dropped_events > 0 {
            let n = self.session.dropped_events;
            gaps.push(format!(
                "{n} event{} not recorded",
                if n == 1 { "" } else { "s" }
            ));
        }
        gaps
    }

    /// Rebuild `base_rows` for the current `show_terminal` setting, keeping
    /// the selection anchored to the same underlying event.
    fn toggle_terminal_and_rebuild(&mut self) {
        let anchor = self.anchor();
        self.base_rows = hh_core::build_timeline(&self.index, self.show_terminal);
        self.durations = row_durations(&self.base_rows, &self.session);
        self.apply_visible(anchor);
    }

    fn anchor(&self) -> Option<i64> {
        self.selected_row()
            .and_then(|r| r.event_ids().first().copied())
    }

    /// Recompute `visible`, anchoring the selection to the current row.
    fn recompute_visible(&mut self) {
        let anchor = self.anchor();
        self.apply_visible(anchor);
    }

    /// Recompute `visible` from `base_rows`, the text filter and the file
    /// filter, restoring the selection to the row containing `anchor` when it
    /// still passes, else clamping.
    fn apply_visible(&mut self, anchor: Option<i64>) {
        let query = self.filter_input.trim().to_ascii_lowercase();
        let files = &self.file_filter_ids;
        let narrowed = self.file_filter.is_some();
        self.visible = self
            .base_rows
            .iter()
            .enumerate()
            .filter(|(_, row)| query.is_empty() || matches_filter(row, &query))
            .filter(|(_, row)| !narrowed || row.event_ids().iter().any(|id| files.contains(id)))
            .map(|(i, _)| i)
            .collect();
        self.selected = anchor
            .and_then(|id| self.position_of(id))
            .unwrap_or(0)
            .min(self.visible.len().saturating_sub(1));
        self.detail_scroll = 0;
        self.clamp_scroll();
    }

    /// Position in `visible` of the row containing event `id`.
    fn position_of(&self, id: i64) -> Option<usize> {
        self.visible
            .iter()
            .position(|&idx| self.base_rows[idx].event_ids().contains(&id))
    }

    fn select(&mut self, pos: usize) {
        self.selected = pos.min(self.visible.len().saturating_sub(1));
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
        self.select(usize::try_from(new).unwrap_or(0));
    }

    /// Move to the next (`forward`) or previous `FileChange` row, wrapping.
    fn jump_diff(&mut self, forward: bool) {
        let n = self.visible.len();
        for step in 1..=n {
            let candidate = if forward {
                (self.selected + step) % n
            } else {
                (self.selected + n - step) % n
            };
            if self.base_rows[self.visible[candidate]].kind() == EventKind::FileChange {
                self.select(candidate);
                return;
            }
        }
        self.status = Some(Status::info("no file changes in this view"));
    }

    /// Cycle to the next (`forward`) or previous moment worth seeing. A
    /// moment hidden by a filter clears the filters so it can be shown.
    fn jump_moment(&mut self, forward: bool) {
        let n = self.moments.len();
        if n == 0 {
            self.status = Some(Status::info(
                "nothing stands out: no errors, failed tool calls, slow steps or diffs",
            ));
            return;
        }
        let next = match (self.moment_cursor, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(c), true) => (c + 1) % n,
            (Some(c), false) => (c + n - 1) % n,
        };
        let moment = self.moments[next].clone();
        if self.position_of(moment.event_id).is_none() {
            self.filter_input.clear();
            self.file_filter = None;
            self.file_filter_ids.clear();
            self.apply_visible(None);
        }
        if let Some(pos) = self.position_of(moment.event_id) {
            self.select(pos);
        }
        self.moment_cursor = Some(next);
        self.view = View::Timeline;
        self.status = Some(Status::info(format!(
            "moment {}/{n} · {} · {}",
            next + 1,
            moment.kind.label(),
            moment.note
        )));
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
        self.select(best_before.or(first_after).unwrap_or(0));
    }

    /// Narrow the timeline to one file's changes and select the first.
    fn open_file(&mut self, file: usize) {
        let Some(group) = self.insights.files.get(file) else {
            return;
        };
        self.file_filter_ids = group.event_ids.iter().copied().collect();
        self.file_filter = Some(file);
        let first = group.event_ids.first().copied();
        self.view = View::Timeline;
        self.focus = Focus::Timeline;
        self.apply_visible(first);
    }

    fn clear_file_filter(&mut self) {
        self.file_filter = None;
        self.file_filter_ids.clear();
        self.recompute_visible();
    }

    fn move_file_selection(&mut self, delta: i64) {
        let len = self.insights.files.len();
        if len == 0 {
            return;
        }
        let max = i64::try_from(len - 1).unwrap_or(i64::MAX);
        let cur = i64::try_from(self.files_selected).unwrap_or(0);
        self.files_selected = usize::try_from((cur + delta).clamp(0, max)).unwrap_or(0);
        if self.files_selected < self.files_scroll {
            self.files_scroll = self.files_selected;
        }
        if self.files_selected >= self.files_scroll + self.viewport_rows {
            self.files_scroll = self.files_selected + 1 - self.viewport_rows;
        }
    }

    fn viewport_rows_i64(&self) -> i64 {
        i64::try_from(self.viewport_rows).unwrap_or(i64::MAX)
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
            state.set_viewport_rows(usize::from(rows.saturating_sub(CHROME_ROWS)));
        }
        AppEvent::Key(key) => match state.mode {
            Mode::Normal => match state.view {
                View::Timeline => handle_normal(state, key),
                View::Files => handle_files(state, key),
            },
            Mode::Filter => handle_filter(state, key),
            Mode::Jump => handle_jump(state, key),
            Mode::Help => handle_help(state, key),
        },
    }
}

/// Keys every list view shares: quit, help, the files view toggle.
fn handle_global(state: &mut AppState, key: KeyInput) -> bool {
    match key {
        KeyInput::Char('q') => state.should_quit = true,
        KeyInput::Char('?') => state.mode = Mode::Help,
        _ => return false,
    }
    true
}

#[allow(clippy::too_many_lines)] // one arm per documented binding; the table reads best whole
fn handle_normal(state: &mut AppState, key: KeyInput) {
    if handle_global(state, key) {
        return;
    }
    // Any key replaces a previous informational message.
    if state.status.as_ref().is_some_and(|s| !s.is_error) {
        state.status = None;
    }
    let scrolling = state.focus == Focus::Detail || state.zoom;
    match key {
        KeyInput::Char('j') | KeyInput::Down => {
            if scrolling {
                state.detail_scroll += 1;
            } else {
                state.move_selection(1);
            }
        }
        KeyInput::Char('k') | KeyInput::Up => {
            if scrolling {
                state.detail_scroll = state.detail_scroll.saturating_sub(1);
            } else {
                state.move_selection(-1);
            }
        }
        KeyInput::PageDown => {
            let n = state.viewport_rows_i64();
            if scrolling {
                state.detail_scroll += state.viewport_rows;
            } else {
                state.move_selection(n);
            }
        }
        KeyInput::PageUp => {
            let n = state.viewport_rows_i64();
            if scrolling {
                state.detail_scroll = state.detail_scroll.saturating_sub(state.viewport_rows);
            } else {
                state.move_selection(-n);
            }
        }
        KeyInput::Char('g') | KeyInput::Home => {
            if scrolling {
                state.detail_scroll = 0;
            } else {
                state.select(0);
            }
        }
        KeyInput::Char('G') | KeyInput::End => {
            if !scrolling {
                state.select(state.visible.len().saturating_sub(1));
            }
        }
        KeyInput::Char('n') => state.jump_moment(true),
        KeyInput::Char('N') => state.jump_moment(false),
        KeyInput::Char('d') => state.jump_diff(true),
        KeyInput::Char('D') => state.jump_diff(false),
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
        KeyInput::Char('f') => {
            state.zoom = false;
            state.view = View::Files;
            state.focus = Focus::Timeline;
        }
        KeyInput::Enter => {
            if state.zoom {
                state.zoom = false;
                state.focus = Focus::Timeline;
            } else if state.selected_row().is_some() {
                state.zoom = true;
                state.focus = Focus::Detail;
            }
        }
        KeyInput::Tab => {
            state.zoom = false;
            state.focus = match state.focus {
                Focus::Timeline => Focus::Detail,
                Focus::Detail => Focus::Timeline,
            };
        }
        KeyInput::Esc => {
            if state.zoom {
                state.zoom = false;
                state.focus = Focus::Timeline;
            } else if state.file_filter.is_some() {
                state.clear_file_filter();
                state.view = View::Files;
            } else if !state.filter_input.is_empty() {
                state.filter_input.clear();
                state.recompute_visible();
            } else {
                state.status = None;
            }
        }
        _ => {}
    }
}

fn handle_files(state: &mut AppState, key: KeyInput) {
    if handle_global(state, key) {
        return;
    }
    match key {
        KeyInput::Char('j') | KeyInput::Down => state.move_file_selection(1),
        KeyInput::Char('k') | KeyInput::Up => state.move_file_selection(-1),
        KeyInput::PageDown => {
            let n = state.viewport_rows_i64();
            state.move_file_selection(n);
        }
        KeyInput::PageUp => {
            let n = state.viewport_rows_i64();
            state.move_file_selection(-n);
        }
        KeyInput::Char('g') | KeyInput::Home => state.move_file_selection(i64::MIN / 2),
        KeyInput::Char('G') | KeyInput::End => state.move_file_selection(i64::MAX / 2),
        KeyInput::Enter => state.open_file(state.files_selected),
        KeyInput::Esc | KeyInput::Char('f') => state.view = View::Timeline,
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
                    state.status = Some(Status::error(format!(
                        "invalid time `{}` — expected mm:ss or hh:mm:ss",
                        state.jump_input
                    )));
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
    // Any key closes the overlay.
    state.mode = Mode::Normal;
}

#[cfg(test)]
mod tests {
    use super::*;
    use hh_core::{AgentKind, EventIndexRow, SessionStatus};
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

    #[test]
    fn durations_run_to_next_step_and_ignore_terminal_rows() {
        let mut s = sample_state();
        // Steps at 0, 1000, 2000, 2500; the last has no known end.
        let durs: Vec<Option<i64>> = s.visible_window(0, 10).map(|(_, _, d)| d).collect();
        assert_eq!(durs, vec![Some(1000), Some(1000), Some(500), None]);
        update(&mut s, AppEvent::Key(KeyInput::Char('t')));
        let durs: Vec<Option<i64>> = s.visible_window(0, 10).map(|(_, _, d)| d).collect();
        assert_eq!(
            durs,
            vec![Some(1000), Some(0), Some(1000), Some(500), None],
            "a terminal row between steps does not end the first step"
        );
    }

    #[test]
    fn unknown_duration_is_none_not_zero() {
        let s = sample_state();
        let last = s.visible_window(3, 1).next().unwrap();
        assert_eq!(last.2, None);
        assert!(!s.is_slow(None));
    }

    fn moment_state() -> AppState {
        let index = vec![
            idx(1, 0, EventKind::UserMessage, Some(1), "start"),
            idx(2, 1000, EventKind::ToolCall, Some(2), "tool_call: Bash"),
            idx(
                3,
                1200,
                EventKind::ToolResult,
                Some(2),
                "tool_result: exit 1",
            ),
            idx(4, 1500, EventKind::AgentMessage, Some(3), "thinking hard"),
            idx(5, 61_500, EventKind::Error, Some(4), "rate limited"),
            idx(6, 62_000, EventKind::FileChange, Some(5), "modified a.rs"),
        ];
        let mut r = session();
        r.last_agent_ts_ms = Some(63_000);
        let insights = Insights {
            failed_tool_results: [3].into_iter().collect(),
            largest_diff: Some(crate::replay::insights::LargestDiff {
                event_id: 6,
                path: "a.rs".into(),
                added: 12,
                removed: 3,
            }),
            ..Insights::default()
        };
        AppState::with_insights(r, index, insights)
    }

    #[test]
    fn moments_cover_failures_slowest_step_and_largest_diff_in_time_order() {
        let s = moment_state();
        let kinds: Vec<MomentKind> = s.moments.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            vec![
                MomentKind::FailedTool,
                MomentKind::SlowestStep,
                MomentKind::Error,
                MomentKind::LargestDiff
            ]
        );
        assert_eq!(s.moments[1].note, "1m00s · thinking hard");
    }

    #[test]
    fn n_cycles_moments_forward_and_wraps_and_capital_n_goes_back() {
        let mut s = moment_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('n')));
        assert_eq!(s.selected_row().unwrap().label(), "tool_call: Bash");
        assert!(s.status.as_ref().unwrap().text.starts_with("moment 1/4"));
        update(&mut s, AppEvent::Key(KeyInput::Char('n')));
        assert_eq!(s.selected_row().unwrap().label(), "thinking hard");
        update(&mut s, AppEvent::Key(KeyInput::Char('n')));
        update(&mut s, AppEvent::Key(KeyInput::Char('n')));
        assert_eq!(s.selected_row().unwrap().label(), "modified a.rs");
        update(&mut s, AppEvent::Key(KeyInput::Char('n')));
        assert_eq!(s.moment_cursor, Some(0), "wraps to the first moment");
        update(&mut s, AppEvent::Key(KeyInput::Char('N')));
        assert_eq!(s.moment_cursor, Some(3));
    }

    #[test]
    fn moment_hidden_by_filter_clears_the_filter() {
        let mut s = moment_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('/')));
        for c in "start".chars() {
            update(&mut s, AppEvent::Key(KeyInput::Char(c)));
        }
        update(&mut s, AppEvent::Key(KeyInput::Enter));
        assert_eq!(s.visible_rows().len(), 1);
        update(&mut s, AppEvent::Key(KeyInput::Char('n')));
        assert_eq!(s.filter_input, "");
        assert_eq!(s.selected_row().unwrap().label(), "tool_call: Bash");
    }

    #[test]
    fn rows_are_marked_failed_or_highlighted() {
        let s = moment_state();
        let rows = s.visible_rows();
        assert!(s.row_failed(rows[1]), "failed tool result marks its step");
        assert!(s.row_failed(rows[3]), "error event");
        assert!(s.row_highlighted(rows[2]), "slowest step");
        assert!(s.row_highlighted(rows[4]), "largest diff");
        assert!(!s.row_failed(rows[0]) && !s.row_highlighted(rows[0]));
        assert!(s.is_slow(Some(60_000)));
    }

    #[test]
    fn no_moments_says_so() {
        let mut s = sample_state();
        let empty = AppState::new(session(), Vec::new());
        assert_eq!(empty.moments, Vec::<Moment>::new());
        s.moments.clear();
        update(&mut s, AppEvent::Key(KeyInput::Char('n')));
        assert!(s
            .status
            .as_ref()
            .unwrap()
            .text
            .contains("nothing stands out"));
    }

    fn files_state() -> AppState {
        let index = vec![
            idx(1, 0, EventKind::FileChange, Some(1), "created a.rs"),
            idx(2, 100, EventKind::AgentMessage, Some(2), "ok"),
            idx(3, 200, EventKind::FileChange, Some(3), "modified b.rs"),
            idx(4, 300, EventKind::FileChange, Some(4), "modified a.rs"),
        ];
        let group = |path: &str, ids: Vec<i64>| crate::replay::insights::FileGroup {
            path: path.into(),
            event_ids: ids,
            renamed_from: None,
            created: false,
            deleted: false,
            binary: false,
        };
        let insights = Insights {
            files: vec![group("a.rs", vec![1, 4]), group("b.rs", vec![3])],
            ..Insights::default()
        };
        AppState::with_insights(session(), index, insights)
    }

    #[test]
    fn files_view_opens_a_files_diffs_and_esc_returns() {
        let mut s = files_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('f')));
        assert_eq!(s.view, View::Files);
        update(&mut s, AppEvent::Key(KeyInput::Enter));
        assert_eq!(s.view, View::Timeline);
        assert_eq!(s.filtered_file().unwrap().path, "a.rs");
        let labels: Vec<String> = s.visible_rows().iter().map(|r| r.label()).collect();
        assert_eq!(labels, vec!["created a.rs", "modified a.rs"]);
        update(&mut s, AppEvent::Key(KeyInput::Char('d')));
        assert_eq!(s.selected_row().unwrap().label(), "modified a.rs");
        update(&mut s, AppEvent::Key(KeyInput::Esc));
        assert_eq!(s.view, View::Files, "Esc goes back to the file list");
        assert!(s.file_filter.is_none());
        assert_eq!(s.visible_rows().len(), 4);
        update(&mut s, AppEvent::Key(KeyInput::Char('j')));
        update(&mut s, AppEvent::Key(KeyInput::Char('j')));
        assert_eq!(s.files_selected, 1, "clamped to the last file");
        update(&mut s, AppEvent::Key(KeyInput::Esc));
        assert_eq!(s.view, View::Timeline);
    }

    #[test]
    fn capital_d_goes_to_previous_diff() {
        let mut s = files_state();
        update(&mut s, AppEvent::Key(KeyInput::Char('D')));
        assert_eq!(s.selected_row().unwrap().label(), "modified a.rs");
        update(&mut s, AppEvent::Key(KeyInput::Char('D')));
        assert_eq!(s.selected_row().unwrap().label(), "modified b.rs");
    }

    #[test]
    fn enter_zooms_detail_and_esc_unzooms() {
        let mut s = sample_state();
        update(&mut s, AppEvent::Key(KeyInput::Enter));
        assert!(s.zoom);
        update(&mut s, AppEvent::Key(KeyInput::Char('j')));
        assert_eq!(s.detail_scroll, 1, "j scrolls while zoomed");
        assert_eq!(s.selection_position().0, 0);
        update(&mut s, AppEvent::Key(KeyInput::Esc));
        assert!(!s.zoom);
        assert_eq!(s.focus, Focus::Timeline);
    }

    #[test]
    fn capture_gaps_admit_missing_transcript_and_drops() {
        let mut r = session();
        r.capture_tier = Some("terminal".into());
        r.adapter_status = AdapterStatus::Degraded;
        r.adapter_degrade_reason = Some("transcript_not_found".into());
        r.dropped_events = 2;
        let s = AppState::new(r, Vec::new());
        assert_eq!(
            s.capture_gaps(),
            vec![
                "no transcript — terminal and file capture only",
                "transcript adapter degraded (transcript_not_found)",
                "2 events not recorded"
            ]
        );
        let mut r = session();
        r.capture_tier = Some("transcript".into());
        assert_eq!(
            AppState::new(r, Vec::new()).capture_gaps(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn capture_tier_unknown_falls_back_to_event_kinds() {
        assert!(sample_state().structured, "user/tool/agent events present");
        let index = vec![idx(1, 0, EventKind::FileChange, Some(1), "created a")];
        assert!(!AppState::new(session(), index).structured);
    }
}
