# Recording

`hh run` wraps a command in a PTY and records what happens.

```bash
hh run -- claude              # record a Claude Code session (auto-detected)
hh run -- python3 my_agent.py # record any command
hh run --adapter codex-cli -- codex   # force an adapter instead of auto-detect
```

Everything after `--` is the command to record.

## What gets captured

- **Terminal output** — ANSI/UTF-8 bytes (and binary chunks) from the PTY.
- **File changes** — create/modify/delete under the working dir, with BLAKE3
  before/after hashes and zstd-compressed contents.
- **Agent turns** — for recognized adapters, the agent's internal prompts,
  tool calls, tool results, and MCP traffic, as structured events.

## Flags

- `--adapter <name>` — force `claude-code`, `claude-desktop`, `codex-cli`, or
  `gemini-cli` instead of auto-detecting. See [Adapters](./adapters.md).
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