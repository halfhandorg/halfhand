//! Agent kind detection (FR-1.2: `claude-code` | `codex-cli` | `gemini-cli` |
//! `generic`).
//!
//! Detection is intentionally simple and string-based. We match on the basename
//! of `command[0]` so that `npx claude`, `/usr/local/bin/claude`, and `claude`
//! all detect as Claude Code; `codex` as Codex CLI; `gemini` as Gemini CLI.
//! A forced `--adapter` flag bypasses this entirely (the CLI passes the
//! adapter's `AgentKind` directly).

use hh_core::AgentKind;
use std::path::Path;

/// Detect the agent kind from the command argv (FR-1.2).
///
/// Rules:
/// - If `command` is empty, this is a caller bug → `Generic`.
/// - If the basename of `command[0]` is `claude` (with optional `.exe` on
///   Windows), → `ClaudeCode`.
/// - If the basename is `codex` → `CodexCli`.
/// - If the basename is `gemini` → `GeminiCli`.
/// - Otherwise → `Generic`.
///
/// A forced `--adapter` flag bypasses this; the CLI wires that by passing the
/// adapter's `AgentKind` directly, so detection only runs for the auto path.
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
    match stem.as_str() {
        "claude" => AgentKind::ClaudeCode,
        "claude-desktop" => AgentKind::ClaudeDesktop,
        "codex" => AgentKind::CodexCli,
        "gemini" => AgentKind::GeminiCli,
        _ => AgentKind::Generic,
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
    fn codex_basename_detects_codex_cli() {
        assert_eq!(detect_agent(&["codex".into()]), AgentKind::CodexCli);
        assert_eq!(
            detect_agent(&["/usr/local/bin/codex".into()]),
            AgentKind::CodexCli
        );
        assert_eq!(detect_agent(&["codex.exe".into()]), AgentKind::CodexCli);
    }

    #[test]
    fn gemini_basename_detects_gemini_cli() {
        assert_eq!(detect_agent(&["gemini".into()]), AgentKind::GeminiCli);
        assert_eq!(
            detect_agent(&["/usr/local/bin/gemini".into()]),
            AgentKind::GeminiCli
        );
        assert_eq!(detect_agent(&["gemini.exe".into()]), AgentKind::GeminiCli);
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
