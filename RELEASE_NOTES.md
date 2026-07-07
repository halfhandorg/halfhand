# Halfhand v0.1.0-beta.1 — Release Notes

**Halfhand** is a local-first CLI flight recorder for AI agents. It wraps any
agent command in a PTY, captures terminal output and file changes, and — for
agents it recognizes — records the agent's internal turns (prompts, tool
calls, tool results) as structured events. Everything stays in a local SQLite
database you can replay, inspect, list, and delete. `hh` itself makes **zero
network calls**.

This is the first public beta. Install it with `cargo install halfhand` or
grab a prebuilt binary from the
[GitHub release](https://github.com/halfhandorg/halfhand/releases/tag/v0.1.0-beta.1)
(verify against the published `SHA256SUMS`).

```bash
cargo install halfhand
hh run -- claude          # record a Claude Code session
hh replay last            # faithfully play it back
hh inspect last           # non-interactive detail
```

## What works

All six command surfaces are implemented and tested end-to-end on **Linux and
macOS** (CI runs the full suite on both; Windows is build-only — see
Limitations):

- **`hh run -- <command>`** (FR-1) — records an agent inside a real PTY.
  Captures ANSI terminal output (UTF-8 + binary chunks), file
  create/modify/delete with BLAKE3 before/after hashes and zstd-compressed
  blob round-trips, skips `target/`/`.git/` and `.gitignore`-matched paths,
  propagates the child exit code, and prints a `✓ Recorded session …`
  epilogue. `--record-input` (off by default) also captures keystrokes.
- **Claude Code adapter** (FR-1.5) — auto-detected when the wrapped command is
  `claude`. Tails `~/.claude/projects/<slug>/*.jsonl` and records structured
  `user_message` / `tool_call` / `tool_result` events, correlating each
  `tool_result` to its `tool_call`, and persists `model` + `usage_json`.
  Degrades gracefully to `adapter_status=degraded` (PTY session still
  recorded) when no projects dir is found.
- **`hh mcp-proxy -- <server>`** (FR-2) — stdio JSON-RPC middleman: forwards
  every message verbatim both ways while recording. Correlates responses to
  requests by JSON-RPC `id` with measured `latency_ms`, records notifications,
  spills payloads ≥256 KiB to the blob store, and attaches to a parent
  `hh run` session via `HH_SESSION_ID` when run as its child.
- **`hh replay <id|last>`** (FR-3) — interactive TUI that faithfully plays
  back the timeline: step list, detail pane, unified diffs, bounded LRU body
  cache. Refuses non-TTY invocation with an actionable error.
- **`hh inspect <id|last>`** (FR-4) — summary table, `--step N` detail,
  `--json` NDJSON / step object (`schema:1`), `--diff` unified diff.
- **`hh list`** (FR-5) — aligned plain table (non-TTY / `NO_COLOR`-safe) and
  `--json` array of session objects (`schema:1`); `--limit` bounds results.
- **`hh delete <id|last> --yes`** (FR-6.1) — removes a session and
  reference-counts its blobs, GCing blobs that reach zero. Refuses without
  `--yes` on non-TTY stdin.
- **`--version`** embeds the git sha (NFR-8): `hh 0.1.0-beta.1 (<sha>)`.
  Release binaries are built with `HH_BUILD_SHA` so the sha is always correct.
- **Crash-safety** (FR-1.7) — SIGKILL of `hh` mid-recording leaves a readable
  session that the next `hh` invocation reconciles to `interrupted`.
- **No-network guarantee** (NFR-2) — enforced by a build-time tripwire test
  that fails CI if any HTTP *client* crate enters the dependency graph.
- **Hardened local storage** (NFR-4) — DB and blob dirs `0700`, blob files
  `0600`, atomic writes with fsync; `--record-input` defaults off.

### Acceptance criteria (SRS §7)

AC-2 through AC-5 are covered by automated tests in `hh/tests/cli.rs` (generic
agent recording, MCP proxy, crash-safety, JSON-schema-via-`jq`). AC-1 (a real
`claude` run) and AC-6 (fresh-machine experience) can't run in CI — they're
documented as checklists in [`docs/manual-qa.md`](docs/manual-qa.md), and
every automatable part of AC-1 (adapter parsing, `hh run -- claude` e2e with a
shim, degraded mode, read-side on a structured session) is automated. This
release fixed a contract bug found while building AC-1's test: `hh list --json`
session objects now emit the documented `schema:1` field (`docs/json.md`).

## Known limitations

- **Windows PTY is best-effort** (SRS §2.2). CI builds the workspace on
  Windows but does not run the test suite there; PTY recording on Windows is
  not validated. Treat Windows support as experimental until v0.2.
- **Single structured adapter.** Only Claude Code is auto-detected. Other
  agents record as `generic` (PTY + file changes, no structured events).
- **Replay is faithful playback, not deterministic re-execution** (SRS §1.4).
  The agent is not re-invoked during replay; no network is touched. A session
  cut short mid-recording replays the partial timeline and is marked
  `interrupted`.
- **Pre-1.0 schemas.** The on-disk SQLite schema (DR-2) and the JSON schema
  (`docs/json.md`, currently `schema:1`) are public and documented but may
  change additively *or* breaking-ly within the `0.1.x-beta` series. **Pin a
  beta version in CI** rather than tracking `latest` until 1.0.
- **`cargo install` builds report `(unknown)` for the git sha** because the
  packaged crate on crates.io has no `.git`. Release binaries built from the
  git tag embed the real sha. (NFR-8 is satisfied for git-checkout builds.)
- **Cross-process step-ordinal race** — an `mcp-proxy` child can lose the
  write race for step ordinals against a concurrently finalizing parent
  `hh run`; affected events keep `step = NULL` until the next `hh` invocation
  self-heals. No data is lost — only the display ordinal is deferred. See
  [`docs/adr/0002-stored-step-ordinals.md`](docs/adr/0002-stored-step-ordinals.md).

## v0.2 pointers (SRS §8)

Cloud/network features remain out of scope (SRS §8 excludes them). Expected
v0.2 focus:

- Additional structured adapters beyond Claude Code.
- Hardened, validated Windows PTY support.
- Cross-process coordination for step ordinals (remove the self-heal race).
- Revisit the `std::thread` vs `tokio` boundary (ADR-0001) **only if** a
  concurrent/streaming feature lands; `hh-core` stays runtime-free.

## Verification

- `cargo fmt --all -- --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace --locked` — 27 integration tests + unit tests pass
  on Linux (macOS run by CI).
- `cargo-deny check advisories licenses bans sources` — all green.
- `cargo package --workspace` — all three crates package and verify.
  `cargo publish --dry-run -p hh-core` passes; `hh-record` and `halfhand` publish after `hh-core` is on crates.io (dependency order).

## Thanks

This beta was shaped by the in-code SRS citations and ADRs in the tree. The
SRS document is not in the repo; the acceptance-criteria mapping in `docs/manual-qa.md` 
is reconstructed from those in-code citations and is flagged as such.