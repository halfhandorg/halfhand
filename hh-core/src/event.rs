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

/// The `schema` integer stamped on every `hh` JSON object — session objects
/// (`hh list --json`), event/step objects (`hh inspect --json`), and the
/// diagnostic/lifecycle command objects (`hh doctor`/`gc`/`stats`/`scan`
/// `--json`, `hh export`). One counter shared across all of them (see
/// `docs/json.md`).
///
/// Frozen at `2` for the 1.0 series (STABILITY.md): additive changes — a new
/// field, a new enum value, a new `body` shape — are documented in
/// `docs/json.md` without bumping this constant further. It only moves again
/// for a breaking change, which ships behind `hh`'s own major-version bump.
/// `2` itself folds in the beta-era additive drift that never got a version
/// bump: three new `agent_kind` values (`claude-desktop`, `codex-cli`,
/// `gemini-cli`) and the `lifecycle` event's `redaction_audit` body shape.
pub const JSON_SCHEMA_VERSION: u64 = 2;

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
///
/// `#[non_exhaustive]`: new agent kinds are added over time (this PR added
/// `ClaudeDesktop`, `CodexCli`, `GeminiCli`). Marking it non-exhaustive keeps
/// that additive under `cargo-semver-checks --release-type minor` instead of
/// registering as a break — a downstream `match` must already carry a wildcard
/// arm, matching CLAUDE.md's v1.0.0 addendum ("additive changes ... not
/// breaking"). New variants are appended at the end so existing discriminant
/// values (used by `as isize` casts) do not shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AgentKind {
    /// Claude Code (structured adapter active).
    ClaudeCode,
    /// Generic agent, PTY-only capture.
    Generic,
    /// Standalone `hh mcp-proxy` session (FR-2.2).
    McpOnly,
    /// Claude Desktop app's Code tab (Claude Code parser; deprecated adapter,
    /// see `docs/adapters.md`).
    ClaudeDesktop,
    /// OpenAI Codex CLI. Terminal output and file changes only: the Codex
    /// adapter never converted a transcript into structured events and is
    /// deprecated (see `docs/adapters.md`).
    CodexCli,
    /// Google Gemini CLI. Terminal output and file changes only: the Gemini
    /// adapter never converted a transcript into structured events and is
    /// deprecated (see `docs/adapters.md`).
    GeminiCli,
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ClaudeCode => "claude-code",
            Self::ClaudeDesktop => "claude-desktop",
            Self::CodexCli => "codex-cli",
            Self::GeminiCli => "gemini-cli",
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
            "claude-desktop" => Ok(Self::ClaudeDesktop),
            "codex-cli" => Ok(Self::CodexCli),
            "gemini-cli" => Ok(Self::GeminiCli),
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
///
/// `#[non_exhaustive]`: this is a growth-prone read model (this PR adds
/// `imported_from`; more fields — `hostname`/`model`/`git_*`, none of which
/// `SessionRow` exposes today — are a plausible future addition). Marking it
/// non-exhaustive now means a future added field stays additive under
/// `cargo-semver-checks --release-type minor` instead of registering as a
/// break, matching the precedent set for `Config`/the error enums. External
/// construction goes through [`Default`] + functional-record-update
/// (`SessionRow { field: value, ..SessionRow::default() }`), which — unlike
/// a struct literal naming every field — stays valid across a
/// `#[non_exhaustive]` boundary; `hh`'s own test fixtures use this pattern.
#[derive(Debug, Clone)]
#[non_exhaustive]
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
    /// The original session id this session was imported from (`hh import`),
    /// or `None` for a locally-recorded session (SRS v1.0.0 addendum: additive
    /// column, migration 0003).
    pub imported_from: Option<String>,
    /// The machine-readable degrade reason code (SRS v1.1.0 BUG-1.1), set
    /// when `adapter_status == Degraded` and the adapter reported a
    /// structured [`AdapterError`](crate::adapter::AdapterError). `None` for
    /// active/none sessions, and for degraded sessions whose tailer panicked
    /// (no structured error — a `hh doctor` diagnostic, not a stale code).
    /// See SRS BUG-1.1 for the code table.
    pub adapter_degrade_reason: Option<String>,
    /// Number of events the recorder failed to write to the database
    /// (`sessions.dropped_events`). Non-zero marks the session degraded: its
    /// timeline has gaps, so it is never shown as a clean `ok`. `0` for a
    /// clean session and for sessions recorded before the column existed.
    pub dropped_events: u64,
    /// The first write failure's cause (what failed and why) when
    /// [`Self::dropped_events`] is non-zero; `None` otherwise.
    pub dropped_events_reason: Option<String>,
}

impl Default for SessionRow {
    /// Neutral placeholder values — the sanctioned way to build a
    /// `#[non_exhaustive]` `SessionRow` from outside this crate is
    /// `SessionRow { field: value, ..SessionRow::default() }`, not a full
    /// struct literal. Real rows always come from [`crate::store::Store`]
    /// queries; this exists for that FRU pattern (test fixtures today, any
    /// future caller tomorrow), not for direct use.
    fn default() -> Self {
        Self {
            id: String::new(),
            short_id: String::new(),
            started_at: 0,
            ended_at: None,
            exit_code: None,
            status: SessionStatus::Recording,
            agent_kind: AgentKind::Generic,
            adapter_status: AdapterStatus::None,
            command: Vec::new(),
            cwd: PathBuf::new(),
            step_count: 0,
            files_changed: 0,
            imported_from: None,
            adapter_degrade_reason: None,
            dropped_events: 0,
            dropped_events_reason: None,
        }
    }
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

/// One event row exactly as stored — no blob-overflow resolution (unlike
/// [`EventDetail`]). Used by `hh-core::bundle` (`hh export --bundle`), which
/// must carry every referenced blob byte-for-byte rather than inlining only
/// the ones that happen to resolve as JSON (see
/// [`crate::store::Store::for_each_event_raw`]).
#[derive(Debug, Clone)]
pub struct RawEventRow {
    /// The event row id.
    pub id: i64,
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
    /// Kind-specific structured payload, exactly as stored (may be an
    /// unresolved `{"overflow": true, ...}` envelope).
    pub body_json: Option<serde_json::Value>,
    /// Blob hash referenced by this event's `events.blob_hash` column, if any.
    pub blob_hash: Option<String>,
    /// Uncompressed size of the referenced blob, if `blob_hash` is set.
    pub blob_size: Option<u64>,
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

/// Where a file change's before-side content came from (migration 0006,
/// `file_changes.before_source`).
///
/// The before-side of the *first* change to a file that existed before the
/// session started is not something the watcher observed live: it is resolved
/// from the session-start baseline. Recording the provenance lets a reader
/// tell a real "no previous content" (a created file) from "previous content
/// was never captured", and tells `hh gc` which before-side blobs are owned by
/// the `file_changes` row (see [`BeforeSource::owns_blob_ref`]).
///
/// `#[non_exhaustive]`: a new provenance (e.g. a future VCS) is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum BeforeSource {
    /// The before-side is the content the watcher itself recorded for this
    /// path earlier in the session (the previous change's after-side).
    Live,
    /// The file was clean against the git index at session start; the
    /// before-side was read from that index blob and stored.
    Git,
    /// The file was dirty, untracked, or outside a git repository at session
    /// start; its content was captured at session start and stored.
    Baseline,
    /// No before-side content exists or it could not be captured: a created
    /// file, a pre-existing file over `max_file_size` or the baseline budget,
    /// or a binary file with `record_binary` off.
    None,
}

impl BeforeSource {
    /// Whether a `file_changes` row with this provenance holds its own
    /// reference (refcount) on the `before_hash` blob. A `Live` before-side
    /// is the after-side blob of an earlier event in the same session, which
    /// that event already references; `Git` and `Baseline` before-sides are
    /// stored only for this row, so the row owns the reference.
    #[must_use]
    pub fn owns_blob_ref(self) -> bool {
        matches!(self, Self::Git | Self::Baseline)
    }
}

impl fmt::Display for BeforeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Live => "live",
            Self::Git => "git",
            Self::Baseline => "baseline",
            Self::None => "none",
        })
    }
}

impl FromStr for BeforeSource {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "live" => Ok(Self::Live),
            "git" => Ok(Self::Git),
            "baseline" => Ok(Self::Baseline),
            "none" => Ok(Self::None),
            other => Err(format!("unknown before source `{other}`")),
        }
    }
}

/// Recorder-side detail of a [`FileChange`] that does not fit the original
/// `file_changes` columns (migration 0006): the before-side provenance and,
/// for a rename, the path the file was renamed from.
///
/// A rename is recorded as ONE row: `path` is the new path, `change_kind` is
/// [`ChangeKind::Modified`] (the before-side is the old path's content, the
/// after-side the new path's), and `renamed_from` is the old path. Adding a
/// `Renamed` variant to the exhaustive [`ChangeKind`] would be a semver break
/// of `hh-core`, so the rename lives here instead.
///
/// `#[non_exhaustive]`: build it with [`FileChangeProvenance::new`] and the
/// `with_*` setters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileChangeProvenance {
    /// Where the before-side came from. `None` for rows written before
    /// migration 0006 or by a caller that did not record it.
    pub before_source: Option<BeforeSource>,
    /// Uncompressed size of the `before_hash` blob. Needed only when the row
    /// owns the blob reference ([`BeforeSource::owns_blob_ref`]), so the
    /// writer can create the `blobs` row. Not persisted.
    pub before_size: Option<u64>,
    /// For a rename: the old path, relative to the session cwd.
    pub renamed_from: Option<String>,
}

impl FileChangeProvenance {
    /// Provenance with the given before-side source and nothing else.
    #[must_use]
    pub fn new(before_source: BeforeSource) -> Self {
        Self {
            before_source: Some(before_source),
            before_size: None,
            renamed_from: None,
        }
    }

    /// Set the uncompressed size of the owned before-side blob.
    #[must_use]
    pub fn with_before_size(mut self, size: Option<u64>) -> Self {
        self.before_size = size;
        self
    }

    /// Mark the change as a rename from `old_path`.
    #[must_use]
    pub fn with_renamed_from(mut self, old_path: Option<String>) -> Self {
        self.renamed_from = old_path;
        self
    }
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
