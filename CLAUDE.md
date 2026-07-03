# Halfhand engineering standards

You are implementing Halfhand, a local-first CLI flight recorder for AI agents,
per the SRS in halfhand-srs-v0.1.0-beta.1.md. Read the SRS before any task and
treat its FR/NFR/DR numbers as the source of truth. If a task conflicts with the
SRS, say so before writing code.

## Rust standards (non-negotiable)
- Edition 2021, MSRV 1.75. Workspace crates: hh-core, hh-record, hh (bin).
- No unwrap()/expect() outside #[cfg(test)] and the top of main(). Use ? with
  context. thiserror for library error enums, anyhow::Context in the binary.
- cargo clippy --workspace --all-targets -- -D warnings must pass after every
  task. Enable clippy::pedantic at crate level and #[allow] individual lints
  with a one-line justification comment.
- cargo fmt --check must pass. rustdoc on every public item (missing_docs deny
  in hh-core).
- Prefer std and small, well-maintained crates already listed in the SRS §6.
  Do not add a dependency without stating why in the commit message.
- Concurrency: single-writer for SQLite (one writer task fed by an mpsc
  channel). Never share a Connection across threads.
- All user-facing errors must be actionable: what failed, why, suggested fix.
- Every task ends with: tests written and passing, clippy clean, a short
  summary of decisions, and any deviations from the SRS called out explicitly.

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
