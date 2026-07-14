# Halfhand

> A local-first CLI **flight recorder** for AI agents. Record a session on your
> own machine, then replay and inspect it later — no servers, no uploads, zero
> network calls from `hh` itself.

Halfhand wraps any agent command in a PTY, captures terminal output and file
changes as they happen, and — for agents it recognizes — also records the
agent's internal turns (prompts, tool calls, tool results) as structured
events. Everything lands in a local SQLite database you can replay, inspect,
list, and delete. Recordings never leave your machine.

![Halfhand CLI](./assets/halfhand-cli.png)

If you just want to try it, go to the [Quickstart](./quickstart.md). For what
"faithful playback" means (and where it ends), see [Replay & Inspect](./replay-inspect.md).
For the 1.0 stability promise — additive CLI flags, frozen `--json` schema,
forward-only DB migrations — see the [Stability policy](./stability.md).

## Why local-first

- **Zero network calls.** `hh` links no HTTP client; nothing is uploaded. Your
  prompts, tool outputs, and diffs stay on disk.
- **You own the data.** One SQLite file (`hh.db`) plus a blob store, in a data
  dir you control (`HH_DATA_DIR`). Delete it and it's gone.
- **Replay is reproducible.** A recording is a faithful transcript of what the
  agent did, so you can audit, share (redacted), or debug later.
- **Secrets are a first-class concern.** `hh scan` finds them, `hh redact`
  removes them in place; exports are redacted by default. See
  [Redaction](./redaction.md).