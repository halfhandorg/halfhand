# Halfhand engineering standards

You are implementing Halfhand, a local-first CLI flight recorder for AI agents,
as specified by the requirements documents in the repo root
(halfhand-srs-*.md). Treat those documents as historical background: read them
when you need the original intent behind a behaviour, but do not cite their
section or requirement numbers. If a task conflicts with the documented
behaviour or the standards below, say so before writing code.

Do not put requirement or ticket IDs (SRS, FR-, NFR-, DR-, BUG-, ENH-, AC-,
"Area n") in code, comments, tests, help text or docs. Describe the behaviour
in plain words instead. hh-core/tests/no_requirement_ids.rs enforces this.

## Rust standards (non-negotiable)
- Edition 2021, MSRV 1.88 (`rust-version` in the workspace Cargo.toml,
  enforced by CI's `msrv` job). Workspace crates: hh-core, hh-record, hh (bin).
- No unwrap()/expect() outside #[cfg(test)] and the top of main(). Use ? with
  context. thiserror for library error enums, anyhow::Context in the binary.
- cargo clippy --workspace --all-targets -- -D warnings must pass after every
  task. Enable clippy::pedantic at crate level and #[allow] individual lints
  with a one-line justification comment.
- cargo fmt --check must pass. rustdoc on every public item (missing_docs deny
  in hh-core).
- Prefer std and small, well-maintained crates already listed in the
  requirements document's crate & dependency plan. Do not add a dependency
  without stating why in the commit message.
- Concurrency: single-writer for SQLite (one writer task fed by an mpsc
  channel). Never share a Connection across threads.
- All user-facing errors must be actionable: what failed, why, suggested fix.
- Every task ends with: tests written and passing, clippy clean, a short
  summary of decisions, and any deviations from the documented behaviour
  called out explicitly.

## CLI/UX standards
- Respect NO_COLOR and non-TTY output (plain, pipe-safe).
- Help text includes a usage example per subcommand.
- Beautiful means restrained: consistent glyphs (✓ ✗ ●), one accent color,
  aligned columns, humanized times. No spinners longer than the work they hide.

## Testing standards
- Unit tests colocated; integration tests in tests/ run hh end-to-end against
  fixture scripts in tests/fixtures/ using temp dirs (never the real data dir —
  set HH_DATA_DIR).
- Snapshot tests with insta for rendered output and adapter conversions.
- Do not mock SQLite; use real in-temp-dir databases.

# v1.0.0 addendum — the stability era

Halfhand is heading to 1.0.0. From now on, in addition to the existing
standards:

- Backward compatibility is a feature. Any change to CLI flags, --json output,
  the DB schema, exit codes, or env vars must be additive, or go through the
  deprecation policy in STABILITY.md (once it exists). If a task requires a
  breaking change, stop and propose it before implementing.
- Every parser that consumes untrusted or external input (Claude/Codex/Gemini
  JSONL, MCP JSON-RPC framing, imported bundles, config files) must have a
  fuzz target and must never panic on malformed input — errors only.
- Storage invariants are property-tested, not example-tested: blob refcounts,
  migration idempotency, step-number assignment, export→import round-trips.
- Performance is regression-gated: criterion benches live in benches/, and CI
  fails on >15% regression vs the committed baseline (use critcmp or
  cargo-criterion JSON diffing; a nightly job, not per-PR, is acceptable).
- New user-facing features ship with: docs page, --help example, CHANGELOG
  entry, and at least one insta snapshot of their human-readable output —
  in the same PR, or the feature is not done.
- Exit codes are part of the contract: 0 ok, 1 generic error, 2 usage error,
  3 session-not-found, 4 redaction/policy block. Document and test them.

# v1.1.0 addendum

- The v1.1.0 requirements (halfhand-srs-v1.1.0.md, when present) are
  background for this era's fixes and enhancements; describe them in plain
  words, never by ID.
- Adapter failures route through AdapterError and persist to
  sessions.adapter_degrade_reason. Silent degradation (a session
  marked adapter-active with zero structured steps and no reason given) was a
  real bug — don't reintroduce it.
- Config loading has three paths (config.toml, halfhand.toml, defaults). All
  three need tests if you touch resolution: an untested path once regressed
  silently and the wrong config file was loaded.
- No thread::sleep for test synchronization. Use channels, latches, or the
  watcher's test seams: sleep-based waits made the watcher and adapter tests
  flaky (notably on macOS).
- No platform-skip escape hatches. #[cfg_attr(target_os = "...", ignore)]
  needs an upstream issue link. Otherwise gate with #[cfg(unix)] and comment.
  Platform-specific ignores once hid real test failures on macOS/Windows.
- FTS5 stays in sync via triggers only. Never write to events_fts from
  application code.
- Homebrew formula updates flow through cargo-dist. No manual edits to
  halfhandorg/homebrew-tap: hand edits drift from what the release pipeline
  generates and are overwritten or break the next release.
- Every Stage ends with `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`