# Changelog

All notable changes to Halfhand are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Pre-1.0 (`0.1.x-beta`) releases may make additive or breaking changes to the
documented JSON schema and the on-disk SQLite schema; both carry a version
field consumers can gate on (see `docs/json.md` and DR-2). Pin a beta version
in CI rather than tracking `latest` until 1.0.

## [Unreleased]

### Added
- Windows is a supported platform (amends SRS §2.2's build-only status): the
  full test suite runs on `windows-latest` in CI, exercising portable-pty's
  ConPTY backend end-to-end with Python and PowerShell fixture-agent
  variants. Platform behavior, permission semantics (Windows ACL note), and
  honest limitations are documented in `docs/platforms.md`; genuinely missing
  pieces are tracked in issues #11 (resize forwarding) and #12 (interactive
  stdin CI coverage).
- CI matrix is now `{ubuntu, macos, windows} × {stable, MSRV}`; the MSRV leg
  builds `--locked` on exactly the `rust-version` toolchain (1.75) declared
  in `Cargo.toml`, with a guard that the pin cannot drift from the manifest.

### Changed
- Recorded relative paths (`file_changes.path`, event summaries) are now
  always `/`-separated on every platform (previously platform-native, which
  would have stored `sub\file.txt` on Windows). Recordings made on one OS
  now query and render identically on another (DR-2).
- The Claude Code adapter's project-dir slug also maps `:` to `-`, so a
  Windows cwd like `C:\Users\me\proj` resolves to `C--Users-me-proj` before
  falling back to the cwd scan.
- `hh` enables Windows virtual terminal processing via crossterm at startup;
  when the console cannot support ANSI, colored output degrades to plain
  text instead of escape garbage.
- `Cargo.lock` is kept MSRV(1.75)-compatible; `lru` is held on the 0.16 line
  (0.17+ needs edition-2024 tooling) and several transitive deps resolved to
  MSRV-compatible releases. `time` is held at 0.3.41 (the fix for
  RUSTSEC-2026-0009 requires rustc 1.88); the advisory is ignored in
  `deny.toml` with a written exposure justification and a revisit note tied
  to the next MSRV bump.

## [0.1.0-beta.1] — 2026-07-07

First public beta. Local-first CLI flight recorder for AI agents: records an
agent session (terminal output, file changes, structured adapter events, MCP
traffic) into a local SQLite database and replays/inspects it later. Halfhand
itself makes zero network calls (NFR-2); only the agent under record may use
the network.

### Added
- `hh run -- <command>`: records an agent inside a PTY. Captures terminal
  output (UTF-8 + binary chunks), file create/modify/delete with before/after
  BLAKE3 hashes and zstd-compressed blob round-trips, propagates the child
  exit code, and prints a `✓ Recorded session …` epilogue (FR-1).
- `--record-input` (off by default; keystrokes may contain secrets — NFR-4)
  captures user keystrokes in addition to terminal output.
- Claude Code adapter (FR-1.5): auto-detected when the wrapped command's
  basename is `claude`. Tails `~/.claude/projects/<slug>/*.jsonl` and records
  structured `user_message` / `tool_call` / `tool_result` events, correlating
  each `tool_result` back to its `tool_call`. Persists `model` + `usage_json`
  on the session. Degrades gracefully (`adapter_status=degraded`) when no
  projects dir exists — the PTY session is still recorded.
- `hh mcp-proxy -- <server>` (FR-2): stdio JSON-RPC middleman that forwards
  every message verbatim in both directions while recording it. Correlates
  responses to requests by JSON-RPC `id` with measured `latency_ms`, records
  notifications on their own, spills payloads ≥256 KiB to the blob store, and
  attaches to a parent `hh run` session via `HH_SESSION_ID` when run as its
  child.
- `hh replay <id|last>` (FR-3): interactive TUI that faithfully plays back a
  recorded session's timeline — assistant text, tool calls, tool results, file
  changes — with a step list, detail pane, unified diffs, and a bounded LRU
  body cache. Refuses non-TTY invocation with an actionable error.
- `hh inspect <id|last>` (FR-4): non-interactive session detail. Summary view
  (header + step table), `--step N` detail, `--json` NDJSON / step object
  (`schema:1`, see `docs/json.md`), `--diff` unified diff. `--json` and
  `--diff` are mutually exclusive with an actionable error.
- `hh list` (FR-5): aligned plain table (non-TTY/`NO_COLOR`-safe) and
  `--json` array of session objects; `--limit` bounds the result set.
- `hh delete <id|last> --yes` (FR-6.1): removes a session and
  reference-counts its blobs, garbage-collecting blobs that reach zero.
  Refuses without `--yes` when stdin is not a TTY.
- `--version` embeds the git sha (NFR-8): `hh 0.1.0-beta.1 (<sha>)`.
- Build-time no-network tripwire (NFR-2): `hh-core/tests/no_network.rs` fails
  the suite if any HTTP *client* crate appears in the resolved workspace graph.
- `cargo-deny` config (NFR-5): advisory, license, bans, and source checks.
- Release CI (`.github/workflows/release.yml`): tag-triggered cross-build for
  x86_64/aarch64 on macOS and Linux (musl where feasible), SHA-256 checksums,
  and GitHub Release upload.
- Docs: `docs/json.md` (public JSON schema), `docs/mcp-proxy.md`,
  `docs/manual-qa.md` (§7 acceptance-criteria matrix + manual checklists),
  `ARCHITECTURE.md`, and ADRs 0001–0002.

### Changed
- `hh-core` storage: SQLite (WAL, `foreign_keys=ON`), embedded idempotent
  migration `0001` (DR-1), content-addressed zstd blob store keyed by BLAKE3
  with refcounted GC. DB and blob dirs `0700`, blob files `0600` (NFR-4).
- `resolve_session` (FR-3.1): `last` → newest session; short-id prefix →
  unique match; ambiguity → error listing candidates.
- Config/path resolution (SRS §4.2): `HH_DATA_DIR` > `[storage] data_dir` >
  platform default. Unknown keys warn, never fail.

### Removed
- Unused `built` build-dependency from the `halfhand` bin crate. It pulled
  `git2` → `libgit2-sys` → `libz-sys` + `cc`/`pkg-config` into the build graph
  for no functionality (the build script embeds the git sha via raw `git`),
  and blocked musl cross-compilation. The git-sha `--version` (NFR-8) is
  unchanged.

### Known limitations
- **Windows PTY is best-effort** (SRS §2.2): CI builds on Windows but does
  not run the test suite there; PTY recording on Windows is not validated.
- **Single structured adapter**: only Claude Code is auto-detected. Other
  agents record as `generic` (PTY + file changes, no structured events).
- **Schema is pre-1.0**: the on-disk SQLite schema (DR-2) and the JSON
  schema (`docs/json.md`, currently `schema:1`) are public but may change
  additively or breaking-ly within the `0.1.x-beta` series. Gate on the
  `schema` field; pin a beta version in CI rather than tracking `latest`.
- **Replay is faithful playback, not deterministic re-execution** (SRS §1.4):
  the agent is not re-invoked during replay; no network calls are made.
  Replay renders the captured timeline. A session whose recording was
  incomplete (e.g. `hh` was SIGKILLed) replays the partial timeline and is
  marked `interrupted`.
- **Cross-process step-ordinal race**: an `mcp-proxy` child may lose the
  write race for step ordinals against a concurrently finalizing parent
  `hh run`; affected events keep `step = NULL` until the next `hh`
  invocation's self-heal. No data is lost — only the display ordinal is
  deferred. See `docs/adr/0002-stored-step-ordinals.md`.

### v0.2 pointers (SRS §8)
Cloud/network features remain out of scope (SRS §8 excludes them); v0.2 work
is expected to focus on: additional structured adapters beyond Claude Code,
hardened Windows PTY support, cross-process step-ordinal coordination, and
revisiting the `std::thread` vs `tokio` boundary (ADR-0001) only if a
concurrent/streaming feature lands.

[Unreleased]: https://github.com/halfhandorg/halfhand/compare/v0.1.0-beta.1...HEAD
[0.1.0-beta.1]: https://github.com/halfhandorg/halfhand/releases/tag/v0.1.0-beta.1