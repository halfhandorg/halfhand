# Halfhand

> A local-first CLI **flight recorder** for AI agents. Record a session on your
> own machine, then replay and inspect it later — no servers, no uploads,
> zero network calls from `hh` itself.



![](docs/assets/halfhand-cli.png)

Halfhand wraps any agent command in a PTY, captures terminal output and file
changes as they happen, and — for agents it recognizes — also records the
agent's internal turns (prompts, tool calls, tool results) as structured
events. Everything lands in a local SQLite database you can replay, inspect,
list, and delete. Recordings never leave your machine.

## 30-second quickstart

```bash
cargo install halfhand@0.1.0-beta.1     # installs the `hh` binary
hh run -- claude                        # record a Claude Code session (or any command)
hh replay last                  # faithfully play it back in an interactive TUI
hh inspect last                 # non-interactive summary + step table
```

That's the whole loop. `hh list` shows every recording; `hh delete last --yes`
removes one.

## What it looks like

An annotated recording + replay session:

```
$ hh run -- claude
  ● Recording in a PTY (adapter: claude-code, auto-detected)
  ● Working directory: ~/proj
  ▶ claude
    …agent runs normally, you interact as usual…
  ✓ Recorded session a1b2c3 · exit 0 · 14 steps · 3 files · hh replay a1b2c3

$ hh list
  ID       STATUS  AGENT        STARTED          DURATION  STEPS  FILES  COMMAND
  a1b2c3   ✓ ok    claude-code  3 minutes ago    42s       14     3      claude

$ hh replay last
  ┌─ Steps ──────────────────┐ ┌─ Detail ──────────────────────────────────┐
  │ ● 1  user_message        │ │ list the files                            │
  │ ● 2  tool_call  Bash     │ │ $ ls                                      │
  │ ● 3  tool_result         │ │ file.txt  README.md  src/                 │
  │   4  user_message        │ │ …                                         │
  └──────────────────────────┘ └───────────────────────────────────────────┘
   ↑/↓ scroll · Enter jump · d diff · q quit
```

Replay is a **faithful playback** of the captured timeline — the agent's
text, tool calls, and results rendered in order. It is *not* a deterministic
re-execution: the agent is not re-invoked, no API calls are made, and no
network is touched during replay. A session whose recording was cut short
(for example `hh` was killed mid-run) replays the partial timeline and is
marked `interrupted`.

## Commands

| Command | What it does |
|---|---|
| `hh run -- <command>` | Record an agent session inside a PTY (FR-1). `--record-input` also captures keystrokes. `--adapter claude-code` forces an adapter. |
| `hh mcp-proxy -- <server>` | Wrap an MCP server in a recording stdio proxy (FR-2). |
| `hh replay <id\|last>` | Faithfully play back a session in an interactive TUI (FR-3). |
| `hh inspect <id\|last>` | Print non-interactive detail: summary, `--step N`, `--json`, `--diff` (FR-4). |
| `hh list` | List recorded sessions, newest first; `--json`, `--limit` (FR-5). |
| `hh delete <id\|last> --yes` | Delete a session and garbage-collect its blobs (FR-6.1). |
| `hh scan <id\|last\|--all>` | Report recorded secrets (type, step, location, hash8 — never the secret); `--json`; exits 4 on findings. |
| `hh redact <id\|last>` | Irreversibly remove detected secrets from a session in place. |
| `hh export <id\|last>` | Export a session as a JSON bundle or `--html` page — **redacted by default**. |
| `hh doctor` | Diagnose the recording stack and print pass/fail per check; `--json`. |

Every subcommand takes `--help` with a usage example. Run `hh --help` for the
overview.

If a session finalized `ok` but recorded `0 steps` or `0 files changed`, run
`hh doctor` — it checks data-dir writability, DB integrity, config resolution,
Claude Code transcript discoverability for the cwd, and a filesystem-watcher
smoke test, and exits nonzero if any check fails. See
[`docs/doctor.md`](docs/doctor.md).

## Structured capture with adapters

Every `hh run` captures terminal output and file changes regardless of what
you run. On top of that, Halfhand can detect certain agents and record their
internal turns as structured events.

- **Claude Code** is auto-detected when the wrapped command's basename is
  `claude` (`hh run -- claude`). Halfhand tails Claude's own transcript
  (`~/.claude/projects/<project>/*.jsonl`) as it's written and turns each
  record into an event, correlating every `tool_result` back to the
  `tool_call` that produced it.
- Force a specific adapter: `hh run --adapter claude-code -- claude`.
- If the adapter can't locate a transcript (no `~/.claude/projects` dir), it
  **degrades gracefully**: you still get the full terminal and file-change
  recording, just without the structured breakdown. Nothing about the session
  fails because of this.
- A session's `agent_kind` and `adapter_status` (`active` / `degraded` /
  `none`) appear in `hh list --json` and `hh inspect`, so you can tell at a
  glance whether structured events were captured.

Other agents record as `generic` (PTY + file changes, no structured events).
More adapters are planned for v0.2.

## Recording MCP traffic with `hh mcp-proxy`

`hh mcp-proxy` sits between an MCP client and an MCP server on stdio,
forwarding every JSON-RPC message verbatim in both directions while quietly
recording it. It's a drop-in wrapper:

```jsonc
{
  "mcpServers": {
    "example": {
      "command": "hh",
      "args": ["mcp-proxy", "--", "python3", "/path/to/server.py"]
    }
  }
}
```

Requests and responses are correlated by JSON-RPC `id` with measured
round-trip latency; notifications are recorded on their own; payloads ≥256 KiB
spill to the local blob store instead of bloating the events table. Run it
standalone for an `mcp-only` session, or as a child of `hh run` (e.g. when
your agent spawns the proxy itself) and it auto-attaches to that parent
session via `HH_SESSION_ID`, so MCP traffic interleaves with the rest of the
timeline. See [`docs/mcp-proxy.md`](docs/mcp-proxy.md).

## Where data lives

Halfhand resolves its data directory in this order:

1. `HH_DATA_DIR` env var
2. `[storage] data_dir` in config
3. The platform default data directory

```bash
HH_DATA_DIR=/tmp/halfhand hh list      # point it anywhere
```

The database (`hh.db`) and blob store live there. DB and blob directories are
created `0700`; blob files are written `0600`.

## Secrets & privacy

- **Halfhand makes zero network calls** (NFR-2). Recorded agent data — prompts,
  tool I/O, MCP traffic, file contents, terminal output — stays on the machine
  it was captured on. There is no HTTP client in `hh`'s dependency graph to
  exfiltrate it; a build-time test fails CI if one is ever added.
- **Recordings can contain secrets** present in prompts, tool I/O, and file
  contents (NFR-4). `--record-input` is **off by default** because keystrokes
  may include passwords typed into the agent. Use `hh delete <id> --yes` to
  remove a recording and garbage-collect its blobs.
- **Redaction is built in**: `hh scan` finds recorded secrets (AWS keys,
  GitHub/GitLab/Slack tokens, private keys, JWTs, your own patterns),
  `hh redact` removes them irreversibly, `hh export` output is redacted by
  default, and `[redaction] at_record = true` scrubs matches before they
  ever hit disk. See [`docs/redaction.md`](docs/redaction.md) and
  [`SECURITY.md`](SECURITY.md) for the threat model and disclosure policy.
- You can keep recordings out of any default location with `HH_DATA_DIR`.
- Exit codes are a contract: `0` ok · `1` error · `2` usage · `3` session
  not found · `4` redaction/policy block (`hh scan` with findings).

## JSON and on-disk schema (pre-1.0)

Two public, documented interfaces ship with this beta:

- **JSON** — `hh list --json` and `hh inspect --json` emit stable, documented
  JSON. Every object carries a `schema` integer (currently `1`); consumers
  should gate on it. See [`docs/json.md`](docs/json.md).
- **SQLite** — `hh.db` is a public, documented interface (DR-2): a content-
  addressed zstd-compressed blob store keyed by BLAKE3, with refcounted
  garbage collection. You can query it directly.

Both schemas are **public but pre-1.0**: within the `0.1.x-beta` series they
may change additively *or* breaking-ly. Additive changes bump the `schema`
integer; breaking changes ship as a new `schema` value with both shapes
documented. **Pin a beta version in CI** rather than tracking `latest` until
1.0.

## Installation

```bash
cargo install halfhand@0.1.0-beta.1
```

Or grab a prebuilt binary from the
[GitHub releases](https://github.com/halfhandorg/halfhand/releases) (x86_64
and aarch64, macOS and Linux) and verify it against the published SHA-256
checksum. `hh --version` prints the version plus the git sha it was built
from: `hh 0.1.0-beta.1 (<sha>)`.

### Build from source

```bash
git clone https://github.com/halfhandorg/halfhand
cd halfhand
cargo build --release -p halfhand    # binary at target/release/hh
```

Requires Rust 1.75+ (MSRV).

## Project layout

Halfhand is a Cargo workspace: `hh-core` (storage, blob store, event model,
config), `hh-record` (PTY runner, FS watcher, MCP proxy), and `hh` (the
binary). See [`ARCHITECTURE.md`](ARCHITECTURE.md) and the
[ADRs](docs/adr/0001-threads-vs-tokio.md).

## Status & limitations

This is the **v0.1.0-beta.1** release. `hh run`, `hh mcp-proxy`, `hh replay`,
`hh inspect`, `hh list`, and `hh delete` are all implemented and tested on
Linux and macOS.

Known limitations:

- **Windows PTY is best-effort.** CI builds on Windows but does not run the
  test suite there; PTY recording on Windows is not validated.
- **Single structured adapter.** Only Claude Code is auto-detected; other
  agents record as `generic`. More adapters are planned for v0.2.
- **Replay is faithful playback, not deterministic re-execution** (SRS §1.4).
  The agent is not re-invoked during replay.
- **Pre-1.0 schemas** (see above).

## License

Apache-2.0 ([`LICENSE`](LICENSE)).
