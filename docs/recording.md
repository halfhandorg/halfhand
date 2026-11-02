# Recording

`hh run` wraps a command in a PTY and records what happens.

```bash
hh run -- claude              # record a Claude Code session (auto-detected)
hh run -- python3 my_agent.py # record any command
hh run -- codex               # any other agent: terminal output + file changes
```

Everything after `--` is the command to record.

## What gets captured

- **Terminal output** — ANSI/UTF-8 bytes (and binary chunks) from the PTY.
- **File changes** — create/modify/delete under the working dir, with BLAKE3
  before/after hashes and zstd-compressed contents.
- **Agent turns** — for Claude Code only, the agent's internal prompts, tool
  calls and tool results, as structured events. Other agents (including Codex
  CLI and Gemini CLI) get terminal output and file changes only. See
  [Adapters](./adapters.md).
- **MCP traffic** — when the agent's MCP servers are wrapped in
  `hh mcp-proxy`.

## Flags

- `--adapter <name>` — force an adapter instead of auto-detecting.
  `claude-code` is the supported value. `codex-cli`, `gemini-cli` and
  `claude-desktop` are **deprecated and scheduled for removal in the next
  minor release**: they still work as before but print a one-line
  deprecation warning, and the Codex and Gemini adapters never captured
  structured events. See [Adapters](./adapters.md#deprecated-flags).
- `--record-input` — also record your keystrokes. **Off by default** because
  keystrokes may contain secrets (SRS NFR-4). Enable only when you need to
  reproduce exact input, and scan/redact before sharing.

## Health check

`hh doctor` is a read-only probe of the recording stack (PTY, watcher, store).
Run it before a session you suspect is broken, or from CI:

```bash
hh doctor
hh doctor --json | jq
```

See [Doctor](./doctor.md). If a recording is interrupted (e.g. you kill `hh`),
the session is marked `interrupted` on the next open and is still inspectable.

## Exit code

`hh run` exits with the **recorded command's own exit code**, so
`hh run -- make` can stand in for `make` in a script. That means `2`, `3` and
`4` from `hh run` are the command's codes, not Halfhand's usage /
session-not-found / policy codes. A command killed by a signal makes `hh run`
exit `1`, and so does a failure in `hh` itself before the command starts. See
[Exit codes](./exit-codes.md).
