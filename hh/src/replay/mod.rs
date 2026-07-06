//! The `hh replay` interactive TUI (SRS FR-3).
//!
//! [`run`] owns the terminal for the session: raw mode + alternate screen on
//! entry, restored on exit *and* on panic (a panic mid-render must not leave
//! the user's shell in raw/alt-screen mode — CLAUDE.md "robust terminal
//! hygiene"). All interactive logic lives in [`state`] as a pure reducer;
//! this module only translates crossterm events into [`AppEvent`]s and
//! drives the draw loop.

mod data;
mod diff;
mod json;
mod kind;
mod state;
mod theme;
mod ui;

pub use data::ReplayData;
pub use state::{AppEvent, AppState, KeyInput};
pub use theme::Theme;

use anyhow::Context;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use hh_core::{EventIndexRow, SessionRow, Store};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::Stdout;
use std::time::Duration;

/// How long to block waiting for the next terminal event before redrawing
/// anyway. Short enough that a resize or programmatic redraw need feels
/// instant; long enough not to busy-loop.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Run the replay TUI for one session to completion (until `q` or Ctrl+C).
///
/// `no_color` should reflect `NO_COLOR`/non-TTY (CLAUDE.md CLI/UX standard);
/// the caller (`hh replay`) resolves that before calling in.
pub fn run(
    store: Store,
    session: SessionRow,
    index: Vec<EventIndexRow>,
    no_color: bool,
) -> anyhow::Result<()> {
    let theme = Theme::resolve(no_color);
    let mut app = AppState::new(session, index);
    let mut data = ReplayData::new(store);

    install_panic_hook();
    let mut terminal = enter_terminal()?;

    // Seed the reducer's paging math with the real terminal size before the
    // first draw; a live resize event keeps it in sync afterward.
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    state::update(&mut app, AppEvent::Resize(cols, rows));

    let result = event_loop(&mut terminal, &mut app, &mut data, theme);

    // Always attempt to restore the terminal, even if the loop errored.
    let restore = leave_terminal(&mut terminal);
    result.and(restore)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    data: &mut ReplayData,
    theme: Theme,
) -> anyhow::Result<()> {
    loop {
        terminal
            .draw(|f| ui::draw(f, app, data, theme))
            .context("rendering the replay TUI")?;

        if app.should_quit {
            return Ok(());
        }

        if event::poll(POLL_INTERVAL).context("polling terminal events")? {
            match event::read().context("reading a terminal event")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(input) = map_key(key.code) {
                        state::update(app, AppEvent::Key(input));
                    }
                }
                Event::Resize(cols, rows) => {
                    state::update(app, AppEvent::Resize(cols, rows));
                }
                _ => {}
            }
        }
    }
}

fn map_key(code: event::KeyCode) -> Option<KeyInput> {
    use event::KeyCode;
    match code {
        KeyCode::Char(c) => Some(KeyInput::Char(c)),
        KeyCode::Enter => Some(KeyInput::Enter),
        KeyCode::Esc => Some(KeyInput::Esc),
        KeyCode::Backspace => Some(KeyInput::Backspace),
        KeyCode::Up => Some(KeyInput::Up),
        KeyCode::Down => Some(KeyInput::Down),
        KeyCode::Left => Some(KeyInput::Left),
        KeyCode::Right => Some(KeyInput::Right),
        KeyCode::PageUp => Some(KeyInput::PageUp),
        KeyCode::PageDown => Some(KeyInput::PageDown),
        KeyCode::Home => Some(KeyInput::Home),
        KeyCode::End => Some(KeyInput::End),
        KeyCode::Tab => Some(KeyInput::Tab),
        _ => None,
    }
}

fn enter_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enabling raw terminal mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("entering the alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("initializing the terminal backend")
}

fn leave_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    disable_raw_mode().context("disabling raw terminal mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("leaving the alternate screen")?;
    terminal.show_cursor().context("restoring the cursor")?;
    Ok(())
}

/// Install a panic hook that restores the terminal (raw mode + alternate
/// screen) before the default hook prints the panic message, so a crash mid-
/// render leaves the user's shell usable instead of stuck in raw/alt-screen
/// mode. Best-effort: the restore calls are `let _ =`'d because a panic
/// handler that itself fails must not mask the original panic.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}
