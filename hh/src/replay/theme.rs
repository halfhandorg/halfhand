//! Color theme for the replay TUI (CLAUDE.md CLI/UX standards:
//! restrained styling, one accent colour, `NO_COLOR` respected).
//!
//! Three palettes: [`Palette::Dark`] and [`Palette::Light`] (the configured
//! `[replay] theme`; `auto` reads the `COLORFGBG` hint many terminals export
//! and falls back to dark), and [`Palette::Mono`] for `NO_COLOR`, which uses
//! only bold/dim/reverse/underline so every distinction survives without hue.
//!
//! Colour budget: one accent (cyan on dark, blue on light) for focus, titles
//! and the "worth seeing" markers. The only other hues are semantic — red for
//! errors and removed diff lines, green for added diff lines — because a diff
//! without them is harder to read, not more restrained.

use hh_core::EventKind;
use ratatui::style::{Color, Modifier, Style};

/// Which palette the theme draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Palette {
    /// Light text on a dark background.
    Dark,
    /// Dark text on a light background.
    Light,
    /// No colour at all (`NO_COLOR`): modifiers only.
    Mono,
}

/// The resolved rendering theme. `Copy` (one small enum), so its methods take
/// `self` by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    palette: Palette,
}

impl Theme {
    /// A theme drawing with `palette`.
    #[must_use]
    pub fn new(palette: Palette) -> Self {
        Self { palette }
    }

    /// Resolve the theme from `NO_COLOR` and the configured `[replay] theme`.
    /// `no_color` wins over everything (CLAUDE.md: "Respect `NO_COLOR`").
    /// `auto` uses the `COLORFGBG` hint (`"15;0"` = light text on black) when
    /// the terminal exports one, and dark otherwise.
    #[must_use]
    pub fn resolve(no_color: bool, configured: hh_core::config::Theme) -> Self {
        use hh_core::config::Theme as Cfg;
        let palette = if no_color {
            Palette::Mono
        } else {
            match configured {
                Cfg::Dark => Palette::Dark,
                Cfg::Light => Palette::Light,
                // `Auto`, and any future variant, follows the terminal's hint.
                _ => palette_from_colorfgbg(std::env::var("COLORFGBG").ok().as_deref()),
            }
        };
        Self::new(palette)
    }

    /// The palette in use.
    #[must_use]
    pub fn palette(self) -> Palette {
        self.palette
    }

    fn color(self) -> bool {
        self.palette != Palette::Mono
    }

    fn accent(self) -> Color {
        match self.palette {
            Palette::Light => Color::Blue,
            Palette::Dark | Palette::Mono => Color::Cyan,
        }
    }

    /// The kind badge. Badges stay uncoloured — kinds are told apart by
    /// their label, not a rainbow — except `ERR`, which takes the error hue.
    /// Terminal output and lifecycle markers are not steps and render dim.
    #[must_use]
    pub fn badge_style(self, kind: EventKind) -> Style {
        match kind {
            EventKind::Error => self.error_style().add_modifier(Modifier::BOLD),
            EventKind::TerminalOutput | EventKind::Lifecycle => self.dim_style(),
            _ => Style::default().add_modifier(Modifier::BOLD),
        }
    }

    /// The selected timeline/files row.
    #[must_use]
    pub fn selected_style(self) -> Style {
        match self.palette {
            Palette::Dark => Style::default().bg(Color::DarkGray),
            Palette::Light => Style::default().bg(Color::Gray),
            Palette::Mono => Style::default().add_modifier(Modifier::REVERSED),
        }
    }

    /// Secondary text: labels, timestamps, hints.
    #[must_use]
    pub fn dim_style(self) -> Style {
        match self.palette {
            Palette::Light => Style::default().fg(Color::DarkGray),
            Palette::Dark | Palette::Mono => Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// Unfocused pane borders.
    #[must_use]
    pub fn border_style(self) -> Style {
        match self.palette {
            Palette::Dark => Style::default().fg(Color::DarkGray),
            Palette::Light => Style::default().fg(Color::Gray),
            Palette::Mono => Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// The one accent: focused border, titles, moment markers.
    #[must_use]
    pub fn accent_style(self) -> Style {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        if self.color() {
            bold.fg(self.accent())
        } else {
            bold
        }
    }

    /// Errors and warnings: failed steps, capture gaps, invalid input.
    #[must_use]
    pub fn error_style(self) -> Style {
        if self.color() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// A diff's added line.
    #[must_use]
    pub fn diff_insert_style(self) -> Style {
        if self.color() {
            Style::default().fg(Color::Green)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// A diff's removed line.
    #[must_use]
    pub fn diff_delete_style(self) -> Style {
        if self.color() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().add_modifier(Modifier::CROSSED_OUT)
        }
    }

    /// A diff's hunk header (`@@ … @@`) and gutter rule.
    #[must_use]
    pub fn diff_hunk_style(self) -> Style {
        if self.color() {
            Style::default().fg(self.accent())
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// JSON object key.
    #[must_use]
    pub fn json_key_style(self) -> Style {
        if self.color() {
            Style::default().fg(self.accent())
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// JSON string value: plain text, so the key stays the only accent.
    #[must_use]
    #[allow(clippy::unused_self)] // a method for symmetry with the other json_* styles
    pub fn json_string_style(self) -> Style {
        Style::default()
    }

    /// JSON punctuation (`{}[]:,`).
    #[must_use]
    pub fn json_punct_style(self) -> Style {
        self.dim_style()
    }
}

/// Read a `COLORFGBG` value (`"fg;bg"` or `"fg;default;bg"`, ANSI indices).
/// A background of 7 (white) or 9–15 (bright colours) is a light terminal;
/// anything else, or no hint, is dark.
fn palette_from_colorfgbg(value: Option<&str>) -> Palette {
    let bg = value
        .and_then(|v| v.rsplit(';').next())
        .and_then(|b| b.trim().parse::<u8>().ok());
    match bg {
        Some(7 | 9..=15) => Palette::Light,
        _ => Palette::Dark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hh_core::config::Theme as Cfg;

    #[test]
    fn no_color_always_wins() {
        for cfg in [Cfg::Auto, Cfg::Dark, Cfg::Light] {
            assert_eq!(Theme::resolve(true, cfg).palette(), Palette::Mono);
        }
    }

    #[test]
    fn explicit_config_picks_palette() {
        assert_eq!(Theme::resolve(false, Cfg::Dark).palette(), Palette::Dark);
        assert_eq!(Theme::resolve(false, Cfg::Light).palette(), Palette::Light);
    }

    #[test]
    fn colorfgbg_hint_detects_light_backgrounds() {
        assert_eq!(palette_from_colorfgbg(Some("0;15")), Palette::Light);
        assert_eq!(palette_from_colorfgbg(Some("0;default;7")), Palette::Light);
        assert_eq!(palette_from_colorfgbg(Some("15;0")), Palette::Dark);
        assert_eq!(palette_from_colorfgbg(Some("garbage")), Palette::Dark);
        assert_eq!(palette_from_colorfgbg(None), Palette::Dark);
    }

    #[test]
    fn mono_uses_no_colour() {
        let t = Theme::new(Palette::Mono);
        for s in [
            t.accent_style(),
            t.error_style(),
            t.diff_insert_style(),
            t.diff_delete_style(),
            t.selected_style(),
            t.dim_style(),
        ] {
            assert_eq!(s.fg, None);
            assert_eq!(s.bg, None);
        }
    }
}
