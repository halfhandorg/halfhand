<!-- Thanks for contributing to Halfhand! Fill this in so reviewers (and future
  readers) can see the change matches the project's done-definition. -->

## What & why

<!-- One or two sentences: what this changes and why. Reference an issue if any. -->

## SRS / stability check

- [ ] This change is **additive** to the 1.0-stable interfaces — CLI flags,
      `--json` output, DB schema, exit codes, env vars — OR a breaking change
      has been proposed and agreed per [STABILITY.md](../STABILITY.md).
- [ ] Any deviation from the SRS (FR/NFR/DR) is called out explicitly below.

<!-- If this touches a stable interface, explain how it stays backward-compatible
  or what the deprecation path is. -->

## Done-definition (CLAUDE.md)

- [ ] `cargo fmt --check` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo test --workspace` passes.
- [ ] No `unwrap()`/`expect()` outside `#[cfg(test)]` and the top of `main()`;
      errors use `?` with context.
- [ ] User-facing errors are actionable (what failed, why, suggested fix).

## If this is a user-facing feature

- [ ] Docs page added/updated (and referenced in `docs/SUMMARY.md` if new).
- [ ] `--help` example added for any new subcommand/flag.
- [ ] `CHANGELOG.md` entry under `[Unreleased]`.
- [ ] An insta snapshot (or robust output assertion) of the human-readable
      output, where applicable.

## If this adds a parser of untrusted input (adapter JSONL, MCP framing,
## bundles, config)

- [ ] Never panics on malformed input — errors only.
- [ ] Fuzz target added under `fuzz/`.
- [ ] Fixture corpus added under `tests/fixtures/`.

## Notes for reviewers

<!-- Anything non-obvious: perf tradeoffs, why a dependency was added (commit
  message should also state why), concurrency notes (single-writer for SQLite). -->