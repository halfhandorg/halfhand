//! Event and session data model (SRS §4.1, §1.3).
//!
//! These types mirror the SQLite schema columns. The store converts between
//! them and the DB via string mapping; they are the public surface used by
//! adapters and the CLI.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use uuid::Uuid;

/// Session status (SRS §4.1 `sessions.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// Session is actively being recorded.
    Recording,
    /// Agent exited cleanly (exit code 0).
    Ok,
    /// Agent exited with a nonzero exit code.
    Error,
    /// `hh` or the agent was killed before finalization (FR-1.7).
    Interrupted,
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Recording => "recording",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Interrupted => "interrupted",
        })
    }
}

impl FromStr for SessionStatus {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "recording" => Ok(Self::Recording),
            "ok" => Ok(Self::Ok),
            "error" => Ok(Self::Error),
            "interrupted" => Ok(Self::Interrupted),
            other => Err(format!("unknown session status `{other}`")),
        }
    }
}

/// Detected agent kind (SRS §4.1 `sessions.agent_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    /// Claude Code (structured adapter active).
    ClaudeCode,
    /// Generic agent, PTY-only capture.
    Generic,
    /// Standalone `hh mcp-proxy` session (FR-2.2).
    McpOnly,
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ClaudeCode => "claude-code",
            Self::Generic => "generic",
            Self::McpOnly => "mcp-only",
        })
    }
}

impl FromStr for AgentKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "claude-code" => Ok(Self::ClaudeCode),
            "generic" => Ok(Self::Generic),
            "mcp-only" => Ok(Self::McpOnly),
            other => Err(format!("unknown agent kind `{other}`")),
        }
    }
}

/// Adapter state (SRS §4.1 `sessions.adapter_status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdapterStatus {
    /// No adapter applicable.
    #[default]
    None,
    /// Adapter is tailing structured events.
    Active,
    /// Adapter failed but PTY/FS recording continues (FR-1.5).
    Degraded,
}

impl fmt::Display for AdapterStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Active => "active",
            Self::Degraded => "degraded",
        })
    }
}

/// Event kind (SRS §4.1 `events.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Session lifecycle marker (start/end/exit).
    Lifecycle,
    /// A user prompt.
    UserMessage,
    /// An assistant message.
    AgentMessage,
    /// Model reasoning/thinking block.
    Thinking,
    /// A tool call.
    ToolCall,
    /// A tool result.
    ToolResult,
    /// An MCP request.
    McpRequest,
    /// An MCP response.
    McpResponse,
    /// An MCP notification.
    McpNotification,
    /// A file change.
    FileChange,
    /// Raw terminal output chunk (not a step; FR-3.4).
    TerminalOutput,
    /// An error.
    Error,
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Lifecycle => "lifecycle",
            Self::UserMessage => "user_message",
            Self::AgentMessage => "agent_message",
            Self::Thinking => "thinking",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::McpRequest => "mcp_request",
            Self::McpResponse => "mcp_response",
            Self::McpNotification => "mcp_notification",
            Self::FileChange => "file_change",
            Self::TerminalOutput => "terminal_output",
            Self::Error => "error",
        })
    }
}

impl FromStr for EventKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "lifecycle" => Ok(Self::Lifecycle),
            "user_message" => Ok(Self::UserMessage),
            "agent_message" => Ok(Self::AgentMessage),
            "thinking" => Ok(Self::Thinking),
            "tool_call" => Ok(Self::ToolCall),
            "tool_result" => Ok(Self::ToolResult),
            "mcp_request" => Ok(Self::McpRequest),
            "mcp_response" => Ok(Self::McpResponse),
            "mcp_notification" => Ok(Self::McpNotification),
            "file_change" => Ok(Self::FileChange),
            "terminal_output" => Ok(Self::TerminalOutput),
            "error" => Ok(Self::Error),
            other => Err(format!("unknown event kind `{other}`")),
        }
    }
}

/// File change kind (SRS §4.1 `file_changes.change_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    /// File was created.
    Created,
    /// File was modified.
    Modified,
    /// File was deleted.
    Deleted,
}

impl fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        })
    }
}

impl FromStr for ChangeKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "created" => Ok(Self::Created),
            "modified" => Ok(Self::Modified),
            "deleted" => Ok(Self::Deleted),
            other => Err(format!("unknown change kind `{other}`")),
        }
    }
}

/// Input for creating a new session (FR-1.2).
#[derive(Debug, Clone)]
pub struct NewSession {
    /// UUIDv7 id (generated by the caller via [`Uuid::now_v7`] or supplied).
    pub id: Uuid,
    /// Unix-ms UTC start timestamp.
    pub started_at: i64,
    /// Detected agent kind.
    pub agent_kind: AgentKind,
    /// Adapter state for this session (FR-1.5): `Active` while a structured
    /// adapter is tailing events, `Degraded` if it failed but PTY/FS recording
    /// continues, `None` for a generic PTY-only session. Written to the
    /// `sessions.adapter_status` column by [`crate::store::Store::create_session`]
    /// and updated at finalize via
    /// [`crate::store::Store::set_session_adapter_meta`].
    pub adapter_status: AdapterStatus,
    /// Full command line as an argv vector (stored as JSON).
    pub command: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Hostname, if known.
    pub hostname: Option<String>,
    /// Halfhand version string.
    pub hh_version: String,
    /// Model name, if an adapter reported one.
    pub model: Option<String>,
    /// Git branch, if cwd is a repo.
    pub git_branch: Option<String>,
    /// Git HEAD sha, if cwd is a repo.
    pub git_sha: Option<String>,
    /// Whether the git tree had uncommitted changes.
    pub git_dirty: Option<bool>,
}

/// Generate a fresh UUIDv7 session id (SRS §1.3). Centralized here so the
/// recorder does not need to depend on `uuid` directly.
#[must_use]
pub fn now_v7() -> Uuid {
    Uuid::now_v7()
}

impl NewSession {
    /// The 6-hex-char short id (SRS §1.3/FR-1.2).
    ///
    /// # SRS deviation (flagged)
    ///
    /// The SRS says "first 6 hex of the UUID", but a UUIDv7's first 12 hex are
    /// the 48-bit unix-ms timestamp, so the first 6 hex are ~constant for ~194
    /// days and would collide immediately under the `short_id UNIQUE`
    /// constraint (SRS §4.1). To keep short ids actually unique and useful as
    /// the SRS intends, we derive the 6 hex from the random tail of the UUIDv7
    /// (`simple[26..32]`, part of `rand_b`). This is a deviation from the
    /// literal wording that should be reconciled with the SRS author; see the
    /// decisions summary and `docs/adr/`.
    pub fn short_id(&self) -> String {
        let simple = self.id.simple().to_string();
        simple[26..32].to_string()
    }
}

/// A session row as read back from the DB.
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// Full UUID string.
    pub id: String,
    /// 6-hex-char short id.
    pub short_id: String,
    /// Unix-ms UTC start timestamp.
    pub started_at: i64,
    /// Unix-ms UTC end timestamp, if finalized.
    pub ended_at: Option<i64>,
    /// Process exit code, if finalized.
    pub exit_code: Option<i32>,
    /// Session status.
    pub status: SessionStatus,
    /// Agent kind.
    pub agent_kind: AgentKind,
    /// Adapter status.
    pub adapter_status: AdapterStatus,
    /// Full command line.
    pub command: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Count of steps (semantic events) in the session.
    pub step_count: i64,
    /// Count of distinct file paths changed.
    pub files_changed: i64,
}

/// An event to append to a session (SRS §4.1 `events`).
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// Owning session id (full UUID string).
    pub session_id: String,
    /// Milliseconds since session start.
    pub ts_ms: i64,
    /// Event kind.
    pub kind: EventKind,
    /// 1-based step ordinal, or `None` for non-step events (FR-3.4).
    pub step: Option<i64>,
    /// One-line summary (≤ 120 chars per SRS §4.1).
    pub summary: String,
    /// Kind-specific structured payload.
    ///
    /// # Cross-event correlation (`correlate_key`)
    ///
    /// Adapters cannot know the DB row id an event will receive, so to express
    /// "this tool_result belongs to that tool_call" they emit a string
    /// `correlate_key` field inside `body_json` (the Claude adapter uses the
    /// `tool_use.id` for a `tool_call` and the `tool_use_id` for a
    /// `tool_result`). The recorder's drain thread resolves that key to the
    /// referenced event's row id and stores it in [`Event::correlates`]. The
    /// FR-3.4 step pass then makes a correlated `tool_result` share its
    /// `tool_call`'s step. See the `adapter` module and FR-1.5.
    pub body_json: Option<serde_json::Value>,
    /// Blob hash for large payloads stored out-of-line.
    pub blob_hash: Option<String>,
    /// Uncompressed size of the blob, if `blob_hash` is set. The writer uses
    /// this to seed the `blobs.size` column on first reference.
    pub blob_size: Option<u64>,
    /// Correlated event id (e.g. tool_result → tool_call).
    pub correlates: Option<i64>,
}

/// An event row read back from the DB (input to the FR-3.4 step pass).
///
/// Unlike [`Event`] (the append form), this carries the DB row `id` so the
/// step pass can express correlation by id and the store can write the
/// assigned `step` back to the right row.
#[derive(Debug, Clone)]
pub struct EventRow {
    /// The event row id (`events.id`).
    pub id: i64,
    /// Owning session id (full UUID string).
    pub session_id: String,
    /// Milliseconds since session start.
    pub ts_ms: i64,
    /// Event kind.
    pub kind: EventKind,
    /// 1-based step ordinal, or `None` for non-step events (FR-3.4).
    pub step: Option<i64>,
    /// Correlated event id (e.g. tool_result → tool_call).
    pub correlates: Option<i64>,
}

/// A lightweight per-event row for the replay/inspect timeline index (FR-3.5):
/// carries the one-line `summary` but not `body_json`/`blob_hash`, so loading
/// the full index for a session (including `terminal_output`) is cheap. Full
/// payloads are fetched on demand per selected row via
/// [`crate::store::Store::get_event_detail`].
#[derive(Debug, Clone)]
pub struct EventIndexRow {
    /// The event row id (`events.id`).
    pub id: i64,
    /// Milliseconds since session start.
    pub ts_ms: i64,
    /// Event kind.
    pub kind: EventKind,
    /// 1-based step ordinal, or `None` for non-step events (`terminal_output`).
    pub step: Option<i64>,
    /// Correlated event id (e.g. `tool_result` → `tool_call`).
    pub correlates: Option<i64>,
    /// One-line summary (≤ 120 chars).
    pub summary: String,
}

/// A fully-loaded event, fetched lazily for the selected row (FR-3.5). Unlike
/// [`EventIndexRow`], `body_json` here is display-ready: a blob-overflowed
/// payload has already been resolved (fetched + decompressed) by
/// [`crate::store::Store::get_event_detail`] so callers never see the raw
/// `{"overflow": true, ...}` envelope.
#[derive(Debug, Clone)]
pub struct EventDetail {
    /// The event row id.
    pub id: i64,
    /// Owning session id.
    pub session_id: String,
    /// Milliseconds since session start.
    pub ts_ms: i64,
    /// Event kind.
    pub kind: EventKind,
    /// 1-based step ordinal, or `None` for non-step events.
    pub step: Option<i64>,
    /// Correlated event id.
    pub correlates: Option<i64>,
    /// One-line summary.
    pub summary: String,
    /// Kind-specific structured payload, resolved from the blob store if it
    /// had overflowed inline storage.
    pub body_json: Option<serde_json::Value>,
    /// The attached `file_changes` row, present only for `kind == FileChange`.
    pub file_change: Option<FileChange>,
}

/// A file change row attached to an event (SRS §4.1 `file_changes`).
#[derive(Debug, Clone)]
pub struct FileChange {
    /// The owning event id.
    pub event_id: i64,
    /// Path relative to the session cwd.
    pub path: String,
    /// Change kind.
    pub change_kind: ChangeKind,
    /// Pre-change blob hash, if known.
    pub before_hash: Option<String>,
    /// Post-change blob hash, if known.
    pub after_hash: Option<String>,
    /// Whether the file was detected as binary.
    pub is_binary: bool,
}

/// Truncate a summary to the SRS §4.1 limit of 120 chars, appending `…` if it
/// was longer. Shared by the recorder (PTY/FS capture) and the adapters so the
/// `events.summary` length constraint is enforced in one place.
#[must_use]
pub fn truncate_summary(s: &str) -> String {
    const LIMIT: usize = 120;
    if s.chars().count() <= LIMIT {
        return s.to_string();
    }
    let truncated: String = s.chars().take(LIMIT - 1).collect();
    format!("{truncated}…")
}
