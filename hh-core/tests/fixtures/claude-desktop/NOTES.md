# Claude Desktop session fixture

`session.jsonl` is a real Claude Desktop (Code tab) session, sanitized. It is
the Desktop counterpart of `../claude-code/current-format.jsonl` and exists so
the `claude-desktop` adapter has a committed sample of what Desktop actually
writes, rather than a hand-written approximation.

## Tests that consume this fixture

Both live in `hh-core/src/adapter/mod.rs` (`mod tests`):

- `snapshot_claude_desktop_fixture_parse` runs every line through the pure
  parser and locks the resulting 51 events in an insta snapshot. Regenerate
  with `INSTA_UPDATE=always cargo test -p hh-core snapshot_claude_desktop`
  after an intentional change, and review the `.snap` diff.
- `tailer_parses_committed_claude_desktop_fixture` tails the fixture through
  `ClaudeDesktopAdapter` end to end and checks the event mix, tool names, the
  single error result, `AgentKind::ClaudeDesktop`, model and usage.

## Provenance

| Field                 | Value                                              |
|-----------------------|----------------------------------------------------|
| Recorded              | 2026-07-18 (timestamps kept verbatim)              |
| Host                  | Ubuntu Linux, Claude Desktop app, Code tab         |
| Embedded CLI version  | `2.1.209` (the `version` field on every record)    |
| Records / human turns | 79 lines, 3 human prompts                          |
| Tools exercised       | Bash, Read, Write, Edit (one `is_error` result)    |
| Work                  | halfhand.org website change (not client work)      |

## Where Claude Desktop writes session data

### Linux (confirmed on this machine)

Desktop does **not** keep its own transcript format. It embeds a Claude Code
CLI binary and the transcript lands in the same place a terminal session does:

```
~/.claude/projects/<cwd-slug>/<sessionId>.jsonl
```

`<cwd-slug>` is the working directory with every `/` and `.` replaced by `-`
(`/home/me/Desktop/site.org` → `-home-me-Desktop-site-org`). Worktree
sessions use the worktree path, so they land in a sibling directory such as
`-home-me-proj--claude-worktrees-<branch>-<hash>`.

What is Desktop-specific lives under the Electron config dir:

```
~/.config/Claude/
  claude-code-sessions/<orgUuid>/<accountUuid>/
    local_<uuid>.json      # one per Desktop session (the "index" record)
    deleted_<uuid>         # tombstone; body is a single epoch-ms integer
    archived-sessions.idx, backlog, waiting-input, scheduled-tasks.json
  claude-code/<cliVersion>/<hash>/claude   # the bundled CLI binary Desktop runs
  local-agent-mode-sessions/               # per-session MCP/skills plugin state
  git-shadow/, git-worktrees.json          # Desktop's worktree bookkeeping
  logs/main.log, logs/claude.ai-web.log    # app logs, not transcripts
```

The index record is the authoritative link between a Desktop session and its
transcript. Relevant keys observed:

```
sessionId        "local_<uuid>"            Desktop-side id
cliSessionId     "<uuid>"                  == <sessionId> in the transcript file name
cwd, originCwd   working directory          -> selects the ~/.claude/projects slug
title, titleSource, model, effort, permissionMode, completedTurns, isArchived
writtenBranches  ["main"]
toolSurfaceSnapshot.appVersion / .cliVersion   (newer records only)
remoteControlSpawn, bridgeSessionIds           (Remote Control sessions only)
```

While a session is running, the CLI also registers itself in
`~/.claude/sessions/<pid>.json` with `"entrypoint":"claude-desktop"` and
`"hostSessionId":"local_<uuid>"`; CLI-launched processes there have
`"entrypoint":"cli"` and no `hostSessionId`.

Nothing session-shaped was found anywhere else under `~/.config/Claude/`
(`IndexedDB`, `Local Storage`, `Session Storage` are Chromium web storage for
the claude.ai web view, not agent transcripts).

### macOS (unverified)

Expected by analogy with Electron's `app.getPath('userData')`:

```
~/.claude/projects/<cwd-slug>/<sessionId>.jsonl
~/Library/Application Support/Claude/claude-code-sessions/<orgUuid>/<accountUuid>/local_<uuid>.json
```

### Windows (unverified)

```
%USERPROFILE%\.claude\projects\<cwd-slug>\<sessionId>.jsonl
%APPDATA%\Claude\claude-code-sessions\<orgUuid>\<accountUuid>\local_<uuid>.json
```

The slug rule on Windows (drive letter, backslashes) has not been checked.

## How a Desktop transcript differs from a CLI one

Same JSONL envelope, same `user` / `assistant` / `system` / `attachment`
records, same `message.content` block types (`text`, `thinking`, `tool_use`,
`tool_result`). The adapter can and does reuse the Claude Code parser.
Differences observed on this machine (192 Desktop-indexed vs 9 CLI transcripts):

| Signal                                   | Desktop                              | CLI                     |
|------------------------------------------|--------------------------------------|-------------------------|
| `entrypoint` on user/assistant/system/attachment records | `"claude-desktop"` | `"cli"`             |
| `promptSource` on human `user` records   | `"sdk"`                              | absent                  |
| `permissionMode` on human `user` records | `"auto"` (typical)                   | varies                  |
| `queue-operation` records (`enqueue` then `dequeue`) wrapping every human prompt | present | present but rarer |
| Record types seen only in Desktop sessions | `pr-link`, `bridge-session`, `frame-link`, `relocated`, `worktree-state`, `file-history-delta` | — |
| Keys seen only in Desktop sessions       | `hookInfos`, `hookCount`, `hookErrors`, `hookAdditionalContext`, `hasOutput`, `preventedContinuation`, `stopReason`, `toolUseID` (on `system` `stop_hook_summary`), `prNumber`/`prUrl`/`prRepository`, `slug`, `worktreeSession`, `bridgeSessionId` | — |
| Keys seen only in CLI sessions           | —                                    | `commandRun`            |
| Index record in `~/.config/Claude/claude-code-sessions/` | yes                  | no                      |

Caveats:

- `entrypoint` is a strong but not perfect signal. 9 of 192 Desktop-indexed
  transcripts here carry `"entrypoint":"cli"` (a terminal `claude` launched
  from inside Desktop, or a Remote Control attach). The index record's
  `cliSessionId` is the ground truth.
- CLI and Desktop embed different CLI versions at any given time (Desktop was
  on 2.1.209 when this fixture was captured, the terminal CLI on 2.1.304), so
  some key/record-type differences above are version skew, not product
  differences. Treat the table as "what to expect", not a schema.
- `isMeta`, `fileHistorySnapshot`, `mode`, `cost-state`, `atis-latch` appear
  in both and are omitted from this fixture only because this short session
  did not emit them.

### Telling them apart, in order of reliability

1. Look up the transcript's file-name UUID as `cliSessionId` in
   `~/.config/Claude/claude-code-sessions/*/*/local_*.json`. Match → Desktop.
2. Read `entrypoint` from the first `user` or `assistant` record.
3. For a live process, `~/.claude/sessions/<pid>.json` → `entrypoint`.

## Sanitization rules applied

Structure, key order, record order, list lengths and scalar types are
unchanged from the original. Replacements:

| Original                                   | Placeholder                                        |
|--------------------------------------------|----------------------------------------------------|
| Prompt text, assistant text, thinking, titles, `lastPrompt`, queue `content`, attachment payloads, tool input strings, tool output, `structuredPatch` lines, hook command names | lorem-ipsum filler of the **same length and same line layout** |
| `cwd`                                      | `/home/user/proj`                                   |
| File paths (`file_path`, `filePath`, any `/home/...` string) | `/home/user/proj/dirN/fileN.<ext>`, same file → same placeholder |
| `gitBranch`                                | `work`                                              |
| `uuid`, `parentUuid`, `leafUuid`, `sessionId`, `promptId`, `toolUseID`, `sourceToolAssistantUUID` | `00000000-0000-4000-8000-<counter>`, consistent per original id |
| `message.id` / `requestId` / `tool_use` ids | `msg_0…N` / `req_0…N` / `toolu_0…N`, same length, consistent |
| `thinking.signature`                        | `sig000…` same length                               |
| git commit `sha`                            | zero-padded counter, same length                    |

Kept verbatim: `type`, `subtype`, `role`, `operation`, `entrypoint`,
`userType`, `version`, `model`, `permissionMode`, `promptSource`, `level`,
`stop_reason`, `service_tier`, `timestamp`, tool names, `origin.kind`,
`caller.type`, all numbers and booleans (including `usage`).

No emails, tokens or credentials were present in kept fields; every free-text
field was replaced wholesale, so none could survive in replaced ones.
