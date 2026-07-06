//! Kind → badge label / color mapping shared by filtering and rendering
//! (FR-3.2): six accent categories — AGENT, USER, TOOL, MCP, FILE, ERR — plus
//! two quieter, uncolored categories for terminal output and lifecycle
//! markers, which are not "steps".

use hh_core::EventKind;
use ratatui::style::Color;

/// The short badge label shown in the timeline pane for a row's kind.
#[must_use]
pub fn badge_label(kind: EventKind) -> &'static str {
    match kind {
        EventKind::AgentMessage | EventKind::Thinking => "AGENT",
        EventKind::UserMessage => "USER",
        EventKind::ToolCall | EventKind::ToolResult => "TOOL",
        EventKind::McpRequest | EventKind::McpResponse | EventKind::McpNotification => "MCP",
        EventKind::FileChange => "FILE",
        EventKind::Error => "ERR",
        EventKind::TerminalOutput => "TERM",
        EventKind::Lifecycle => "LIFE",
    }
}

/// The accent color for a kind's badge (FR-3.2: "distinct colors"). `None`
/// for the two non-badge categories (terminal/lifecycle), which render
/// dimmed instead — see [`crate::replay::theme::Theme::dim`].
#[must_use]
pub fn badge_color(kind: EventKind) -> Option<Color> {
    match kind {
        EventKind::AgentMessage | EventKind::Thinking => Some(Color::Cyan),
        EventKind::UserMessage => Some(Color::Green),
        EventKind::ToolCall | EventKind::ToolResult => Some(Color::Yellow),
        EventKind::McpRequest | EventKind::McpResponse | EventKind::McpNotification => {
            Some(Color::Magenta)
        }
        EventKind::FileChange => Some(Color::Blue),
        EventKind::Error => Some(Color::Red),
        EventKind::TerminalOutput | EventKind::Lifecycle => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_and_result_share_badge() {
        assert_eq!(badge_label(EventKind::ToolCall), "TOOL");
        assert_eq!(badge_label(EventKind::ToolResult), "TOOL");
    }

    #[test]
    fn mcp_variants_share_badge() {
        assert_eq!(badge_label(EventKind::McpRequest), "MCP");
        assert_eq!(badge_label(EventKind::McpResponse), "MCP");
        assert_eq!(badge_label(EventKind::McpNotification), "MCP");
    }

    #[test]
    fn terminal_and_lifecycle_have_no_accent_color() {
        assert_eq!(badge_color(EventKind::TerminalOutput), None);
        assert_eq!(badge_color(EventKind::Lifecycle), None);
    }
}
