//! Color theme for the replay TUI (FR-3.2, CLAUDE.md CLI/UX standards:
//! restrained styling, `NO_COLOR` respected).
//!
//! Halfhand does not query the terminal's actual background, so the
//! configured `[replay] theme` (`auto`/`dark`/`light`) does not change the
//! accent hues today — the only axis this module actually switches on is
//! color vs. monochrome (`NO_COLOR` or a non-TTY). That is a deliberate scope
//! limit for v0.1, not an oversight.

use hh_core::EventKind;
use ratatui::style::{Color, Modifier, Style};

/// The resolved rendering theme. `Copy` (one `bool`), so its methods take
/// `self` by value rather than `&self` — cheaper and reads better at call
/// sites (`theme.accent_style()` without a borrow to thread through).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    color: bool,
}

impl Theme {
    /// Resolve the theme: `no_color` is true when `NO_COLOR` is set or
    /// stdout is not a TTY (CLAUDE.md: "Respect NO_COLOR and non-TTY output").
    #[must_use]
    pub fn resolve(no_color: bool) -> Self {
        Self { color: !no_color }
    }

    /// The kind badge's style (FR-3.2: "distinct colors"). Monochrome falls
    /// back to bold for a badge kind and dim for the two non-badge kinds
    /// (terminal/lifecycle) — still visually distinct without hue.
    #[must_use]
    pub fn badge_style(self, kind: EventKind) -> Style {
        if !self.color {
            return match super::kind::badge_color(kind) {
                Some(_) => Style::default().add_modifier(Modifier::BOLD),
                None => Style::default().add_modifier(Modifier::DIM),
            };
        }
        match super::kind::badge_color(kind) {
            Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
            None => Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// Style for the selected timeline row: reverse video works identically
    /// in color and monochrome terminals, so this ignores `self.color` today —
    /// kept as a method (not a bare constant) so a future theme axis (e.g. a
    /// distinct monochrome selection glyph) has a natural home.
    #[must_use]
    #[allow(clippy::unused_self)] // kept as a method, not a free fn, for a future monochrome-selection theme axis
    pub fn selected_style(self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }

    /// Dimmed text (relative timestamps, hints, secondary labels). Same
    /// rationale as [`Self::selected_style`] for taking `self`.
    #[must_use]
    #[allow(clippy::unused_self)] // same rationale as Self::selected_style
    pub fn dim_style(self) -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    /// Pane borders. Focused-pane border uses [`Self::accent_style`] instead.
    #[must_use]
    pub fn border_style(self) -> Style {
        if self.color {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        }
    }

    /// The one accent color (CLAUDE.md: "one accent color"): the focused
    /// pane's border, section headers in the detail pane.
    #[must_use]
    pub fn accent_style(self) -> Style {
        if self.color {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Error/warning text (invalid jump input, load failures).
    #[must_use]
    pub fn error_style(self) -> Style {
        if self.color {
            Style::default().fg(Color::Red)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// A unified diff's added line.
    #[must_use]
    pub fn diff_insert_style(self) -> Style {
        if self.color {
            Style::default().fg(Color::Green)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// A unified diff's removed line.
    #[must_use]
    pub fn diff_delete_style(self) -> Style {
        if self.color {
            Style::default().fg(Color::Red)
        } else {
            Style::default().add_modifier(Modifier::UNDERLINED)
        }
    }

    /// A unified diff's hunk header (`@@ ... @@`).
    #[must_use]
    pub fn diff_hunk_style(self) -> Style {
        if self.color {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// JSON object key.
    #[must_use]
    pub fn json_key_style(self) -> Style {
        if self.color {
            Style::default().fg(Color::Blue)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// JSON string value.
    #[must_use]
    pub fn json_string_style(self) -> Style {
        if self.color {
            Style::default().fg(Color::Green)
        } else {
            Style::default()
        }
    }

    /// JSON punctuation (`{}[]:,`).
    #[must_use]
    pub fn json_punct_style(self) -> Style {
        self.dim_style()
    }
}
