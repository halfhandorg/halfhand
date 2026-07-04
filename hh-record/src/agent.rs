//! Agent kind detection (FR-1.2: `claude-code` vs `generic`).
//!
//! Detection is intentionally simple and string-based: the SRS only
//! distinguishes `claude-code` from `generic` in v0.1, and the Claude Code
//! adapter (FR-1.5) is out of scope for this skeleton. We match on the
//! basename of `command[0]` so that `npx claude`, `/usr/local/bin/claude`,
//! and `claude` all detect as Claude Code.

use hh_core::AgentKind;
use std::path::Path;

/// Detect the agent kind from the command argv (FR-1.2).
///
/// Rules:
/// - If `command` is empty, this is a caller bug → `Generic`.
/// - If the basename of `command[0]` is `claude` (with optional `.exe` on
///   Windows), → `ClaudeCode`.
/// - Otherwise → `Generic`.
///
/// A forced `--adapter claude-code` flag would bypass this; the CLI wires that
/// by passing `AgentKind::ClaudeCode` directly, so detection only runs for the
/// auto path.
#[must_use]
pub fn detect_agent(command: &[String]) -> AgentKind {
    let Some(first) = command.first() else {
        return AgentKind::Generic;
    };
    let basename = Path::new(first)
        .file_name()
        .map(std::ffi::OsStr::to_string_lossy)
        .unwrap_or_default();
    let stem = basename
        .strip_suffix(".exe")
        .unwrap_or(&basename)
        .to_ascii_lowercase();
    if stem == "claude" {
        AgentKind::ClaudeCode
    } else {
        AgentKind::Generic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_command_is_generic() {
        assert_eq!(detect_agent(&[]), AgentKind::Generic);
    }

    #[test]
    fn claude_basename_detects_claude_code() {
        assert_eq!(detect_agent(&["claude".into()]), AgentKind::ClaudeCode);
        assert_eq!(
            detect_agent(&["/usr/local/bin/claude".into()]),
            AgentKind::ClaudeCode
        );
        // `.exe` suffix stripped before comparison (Windows binaries).
        assert_eq!(detect_agent(&["claude.exe".into()]), AgentKind::ClaudeCode);
    }

    #[test]
    fn other_commands_are_generic() {
        assert_eq!(detect_agent(&["python3".into()]), AgentKind::Generic);
        assert_eq!(
            detect_agent(&["npx".into(), "tsx".into()]),
            AgentKind::Generic
        );
    }
}
