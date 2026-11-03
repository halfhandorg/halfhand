//! Kind → badge label / color mapping shared by filtering and rendering
//! (FR-3.2): six step categories — AGENT, USER, TOOL, MCP, FILE, ERR — plus
//! two quieter categories for terminal output and lifecycle markers, which
//! are not "steps". Styling lives in [`super::theme::Theme::badge_style`].

use hh_core::EventKind;

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
}
