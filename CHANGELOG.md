# Changelog

All notable changes to Halfhand are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Pre-1.0 (`0.1.x-beta`) releases may make additive or breaking changes to the
documented JSON schema and the on-disk SQLite schema; both carry a version
field consumers can gate on (see `docs/json.md` and DR-2). Pin a beta version
in CI rather than tracking `latest` until 1.0.

## [Unreleased]

### Diagnostics
- A degraded Claude Code adapter session now self-documents in the database,
  not just on stderr: the specific degrade reason is persisted as an `error`
  event (`body_json = {"adapter":"claude-code","reason":"…"}`), so
  `SELECT … WHERE kind='error'` explains *why* a session recorded no structured
  steps — previously such a query returned empty and the failure was a silent
  mystery. Reasons are now specific: `no jsonl matched cwd slug <slug>` (with
  the slug dir it looked in + candidate counts), `jsonl found (<file>) but 0
  records parsed (read N line(s); first parse error at line K: <msg>)`,
  `found a transcript at <file> but could not read it`, or `discovery selected
  file <file> but it's a directory`. A found-but-empty transcript now degrades
  with `0 records parsed` instead of finalizing `active` with 0 steps. Set
  `HH_DEBUG=1` during `hh run` to capture a discovery+parse trace (computed
  slug, projects dir, candidate files, selected transcript, records read,
  events produced, first conversion failure) to stderr. Note: a degraded
  session now reports 1 step (the `error` event itself) rather than 0.

### Fixed
- **Silent recording breakage** (the reported "0 steps / 0 files but
  `status=ok`" symptom): the Claude Code adapter's transcript locator used a
  fixed 3 s deadline and matched the *first* `cwd`-bearing record, so on
  recent Claude versions — which write `~/.claude/projects/<slug>/*.jsonl`
  lazily (only on first output) and omit `cwd` from the early `system`/meta
  records — the locator either timed out before the file appeared or rejected
  it on the cwd-less head, finalized `ok` with 0 structured steps, and printed
  no warning. The locator now polls until the session stops, snapshots
  pre-existing transcripts so it only matches one created *during* this
  session, and scans past cwd-less records to the first cwd-bearing one (and
  verifies it belongs to this session's cwd). Every degrade path now carries
  an actionable `degrade_reason` printed after the child exits (FR-1.5), and
  the `hh run` epilogue warns when an adapter-active claude-code session
  records 0 steps over >60 s.
- A `halfhand.toml` (or `hh.toml`) config file is no longer silently ignored.
  When `config.toml` is absent but a legacy file exists in the same directory,
  `hh` now loads it as a fallback (so ignore globs / a custom `data_dir` take
  effect) and emits a single one-line deprecation hint on stderr suggesting the
  rename to `config.toml`. When `config.toml` *is* present, a sibling legacy
  file is still genuinely ignored (the canonical path wins) — `hh` warns on
  stderr at startup and `hh doctor` reports it as a failing check, naming the
  ignored file and where to move its contents.
- The FS watcher no longer aborts `hh run` with "recording failed" when its
  cwd is unwatchable (e.g. `notify` rejects a recursive watch with `EACCES`).
  It degrades instead: a single stderr warning points at `hh doctor`, file
  recording is skipped, and the PTY + adapter session still records
  (`status=ok`). A per-directory fallback also keeps file recording working
  when one unreadable subdir would have blacked out the whole tree.
- The FS watcher now drains in-flight events for a bounded window (150 ms of
  silence, capped at 1 s) after the child exits, before finalizing, and then
  runs a deterministic re-scan of `cwd` as a backstop. On macOS, FSEvents can
  deliver a write event *after* the process that performed it has already
  exited — or never deliver it at all on GitHub's macOS runners. The old
  behavior stopped the watcher the instant the child was gone and dropped that
  event, so a quick-exiting agent could finalize with "0 file changes" and
  `hh inspect --diff` reported "no file changes in this session" despite files
  being written. The grace drain catches *late* events; the re-scan backstop
  catches the *absent*-event case deterministically (it does not depend on
  `notify` delivering anything). At startup the watcher now hashes every
  capturable baseline file (storing only the hash, not a blob), so the backstop
  can also detect a missed *modify* of a pre-existing file (current hash ≠
  baseline hash) and record it as `Modified` — a backstopped modify renders
  with a missing before-side ("all added"), since the original content was
  overwritten before it was observed. Files already captured via the event
  path and unchanged baseline files are skipped, so neither mechanism
  duplicates a normal-path capture. A prompt backend (Linux inotify) pays only
  the short quiet wait; the startup + shutdown baseline hashing is the
  trade-off for missed-modify coverage on large trees.
- `hh list` column headers no longer drift out of alignment with the data
  when color is enabled: padding now measures *visible* width (ANSI escapes
  stripped) instead of byte length, so colored status cells no longer push
  every column to their right out of line. `hh list` also shows a `⚠` marker on
  rows whose adapter ended `degraded`, so a silently-broken session is no
  longer indistinguishable from a clean one.
- `BlobStore::get` / `remove_if_unreferenced` now reject a malformed hash
  (wrong length, non-hex, or non-ASCII bytes) instead of risking a byte-slice
  panic on a multi-byte UTF-8 character at offset 2, or a path-traversal write
  via a crafted hash string.
- The FS watcher, adapter tailer, and MCP proxy threads share one writer
  `Mutex`; a panic in any one of them no longer poisons it for the rest of the
  session (every lock site now recovers from poisoning) or aborts `hh run`'s
  finalize — a panicked adapter tailer now degrades the session
  (`adapter_status=degraded`, warning) exactly like the existing
  "no transcript found" path, instead of leaving it stuck at
  `status=recording` forever.
- `writer_run`'s `PRAGMA foreign_keys = ON` failure is now logged instead of
  silently swallowed (a silent failure here would disable FK enforcement for
  the rest of the session with no visible cause).
- Bumped `crossbeam-epoch` 0.9.19 → 0.9.20 (RUSTSEC-2026-0204), pulled in
  transitively via `ignore`.

### Performance
- **Ingest throughput: `PRAGMA synchronous = NORMAL` (NFR-1 / NFR-3).** The store
  never set `synchronous`, so SQLite defaulted to `FULL`, fsyncing the WAL on
  *every* event commit. That met NFR-3's durability ("fsync on session finalize")
  and then some, but capped sustained ingest near ~300 events/s — far below
  NFR-1's ≥5,000/s. `Store::open` and the writer thread now set
  `synchronous = NORMAL`: in WAL mode this keeps ACID (no corruption on crash)
  but skips the per-commit fsync, fsyncing only at checkpoint. That matches the
  durability design NFR-3 actually names — `finalize_session` runs
  `wal_checkpoint(TRUNCATE)`, which fsyncs, so a session is durable at finalize;
  SQLite's default `wal_autocheckpoint` (≈4 MiB) bounds the mid-session
  power-loss window to the last autocheckpoint. Sustained ingest is now ~1.5k
  events/s (≈5× over the per-event-fsync baseline); reaching NFR-1's 5k/s still
  needs statement caching + a batched writer (tracked as follow-up; the
  regression gate below protects the gain). No CLI/JSON/schema/env change.
- `hh inspect --json` now streams a session as NDJSON through
  `Store::for_each_event_detail` (one `EventDetail` at a time) instead of
  collecting the whole session into RAM — constant memory for any session size
  (Area 2). A 100k-event session streams without materializing 100k rows.
- The replay body cache (`ReplayData`) is verified bounded at its 50-entry
  capacity across unbounded scrolling (Area 2): a regression to an unbounded
  cache would fail `get_evicts_lru_to_stay_bounded_at_capacity`.

### Added
- `hh doctor` (read-only diagnostic): runs five health checks — data dir
  writability, `PRAGMA integrity_check`, config resolution + non-canonical
  config detection, Claude Code transcript discoverability for the cwd (with a
  parse-test of the newest transcript), and a `notify` watcher smoke test —
  printing one `✓`/`✗` line per check and exiting nonzero if any fail. `--json`
  emits a stable `schema:1` object with a per-check array. Docs: see
  `docs/doctor.md`; `--help` carries an example.
- Regression tests for the silent-breakage fix: an end-to-end file-change
  recording test, an adapter test against both Claude JSONL fixture
  generations (including the new-format transcript with `mode` /
  `permissionMode` / `fileHistorySnapshot` and no `cwd` on the early records),
  an integration test asserting the watcher-init-failure warning reaches
  stderr and the session still records `ok` (not "recording failed"), and an
  adapter degraded-warning-at-finalize test. `hh list` / `hh doctor` rendering
  is locked with insta snapshots.
- `cargo fuzz` targets (`fuzz/`, nightly toolchain) for the four
  untrusted/external-input parsers: Claude JSONL transcript lines, MCP
  JSON-RPC framing, `config.toml`, and blob decompression. Seeded from
  `tests/fixtures/`; runs nightly in CI (60s/target,
  `.github/workflows/fuzz-nightly.yml`) and via `just fuzz-all <seconds>`
  locally.
- `proptest` property tests: blob refcount/GC invariants (refcount tracks live
  references exactly; GC removes a blob iff unreferenced) across arbitrary
  create/append/delete sequences, step-assignment invariants (dense ordinals,
  exact call/result pairing, idempotent) across arbitrary event interleavings,
  and migration-reopen idempotency (byte-identical schema across N reopens).
- Panic-injection tests proving a real panic on the FS watcher thread or the
  Claude adapter tailer thread degrades the session instead of aborting the
  recording.
- CI: `cargo-llvm-cov` gate (hh-core ≥ 80% lines, fail-under; workspace-wide
  report-only) and `cargo-semver-checks` against `main` for hh-core's public
  API (adapters implement its `Adapter` trait).
- Criterion benches (`hh-core/benches/`) + a nightly regression gate (Area 1 /
  NFR-1): four groups — `ingest` (`append_event` throughput), `replay_index`
  (10k/100k-event `list_event_index`), `blob_write`/`blob_read` (1 KiB/1 MiB),
  and `adapter_jsonl` (Claude JSONL conversion, gated on the `fuzzing` feature
  so it uses the dev-only parser entry point, off the semver-checked surface).
  `.github/workflows/bench-nightly.yml` runs `cargo bench -p hh-core --features
  fuzzing` nightly and fails on >15% regression vs the previous run's baseline
  (same runner class via an `actions/cache` rolling baseline — a committed
  baseline would false-positive across hardware). `just bench` /
  `just bench-compare` run the same locally. Dev-only: criterion is a
  dev-dependency (default-features off) and pulls no HTTP client, so NFR-2's
  no-network-crate check stays green. See `docs/performance.md`.
- `hh gc` (reclaim space, Area 3): prunes orphaned blob files and stale
  `blobs` rows — the crash-leftover cases `hh delete` does not reach (a blob
  written to disk between `BlobStore::put` and the referencing event's commit,
  or a `blobs` row whose backing file was deleted out of band) — and `VACUUM`s
  `hh.db` to shrink it on disk (`--no-vacuum` skips the rebuild). Safe: it only
  removes blobs no live event references, never referenced data. `--json` emits
  a stable `schema:1` object (`orphan_files_removed` / `orphan_bytes_reclaimed`
  / `orphan_rows_removed` / `vacuumed`). Docs: see `docs/gc.md`; `--help`
  carries an example.
- `hh stats` (read-only store inventory, Area 3): session/event/blob counts,
  on-disk footprint (`hh.db` + WAL/SHM sidecars + compressed blob directory),
  and the largest sessions by event count (`--top N`, default 5). `--json`
  emits a stable `schema:1` object with `blobs` / `disk` / `largest_sessions`
  sub-objects. Docs: see `docs/stats.md`; `--help` carries an example.
- Storage schema migration 0002 (additive): a partial index
  `idx_events_needs_heal` on `events(session_id) WHERE step IS NULL AND
  kind != 'terminal_output'`, making `Store::open`'s step self-heal probe
  O(rows-needing-heal) instead of an O(all-events) scan on every invocation —
  including read-only `hh list`, whose cold start used to scale with total
  recorded history. Additive (a new index on an existing table); no breaking
  schema change, and the `schema_version` row is the only data write.

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