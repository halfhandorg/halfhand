# Contributing to Halfhand

Thanks for helping make Halfhand better. This guide covers dev setup, the
done-definition every PR must meet, how to add a new agent adapter (with the
fixture-capture guide), and the fuzz/bench workflows. Read
[CLAUDE.md](CLAUDE.md) for the non-negotiable engineering standards and
[STABILITY.md](STABILITY.md) for the 1.0 backward-compatibility rules — both
apply to every change.

## Dev setup

You need Rust 1.75+ (MSRV) and `just` for the convenience tasks.

```bash
git clone https://github.com/halfhandorg/halfhand
cd halfhand
cargo build --workspace          # builds hh-core, hh-record, hh (bin), hh-dist
cargo test --workspace           # unit + integration + property tests
```

The `hh` binary is `target/debug/hh`. Integration tests run the real binary
against throwaway data dirs (`HH_DATA_DIR`); they never touch your real
data dir. See `hh/tests/cli.rs`.

### The four gates (run before pushing)

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs these on every PR, plus a no-network-crate check (the binary must link
no HTTP client — NFR-2) and a Windows build-only job.

## Done-definition

Every PR meets the [CLAUDE.md](CLAUDE.md) standards — the
[PR template](.github/PULL_REQUEST_TEMPLATE.md) is a checklist of them. In
short:

- `cargo fmt --check`, clippy with `-D warnings`, and `cargo test --workspace`
  all pass.
- No `unwrap()`/`expect()` outside `#[cfg(test)]` and the top of `main()`; use
  `?` with context (`anyhow::Context` in the binary, `thiserror` in libs).
- User-facing errors are actionable: what failed, why, a suggested fix.
- **User-facing features** ship in the same PR with: a docs page (linked in
  `docs/SUMMARY.md`), a `--help` example for any new subcommand/flag, a
  `CHANGELOG.md` entry under `[Unreleased]`, and an insta snapshot (or robust
  output assertion) of the human-readable output.
- **Backward compatibility** is a feature: changes to CLI flags, `--json`
  output, the DB schema, exit codes, or env vars must be additive or go through
  the [STABILITY.md](STABILITY.md) deprecation policy. If a task requires a
  breaking change, stop and propose it first.

## Stability duties

Before changing a 1.0-stable interface (CLI flags, `--json` schema, DB schema,
exit codes, env vars), read [STABILITY.md](STABILITY.md). Prefer additive
changes. If you must remove or rename something, add the new form first, mark
the old one deprecated (with a runtime warning that points at the new form),
and only remove it after the documented grace period. Bump the schema/JSON
version field if you extend it.

## Adding an agent adapter

Adapters consume **untrusted** JSONL from the agent's own transcript, so the
1.0 addendum requires three things for every adapter: never panic on malformed
input (errors only), a fuzz target, and a fixture corpus.

1. **Capture a fixture corpus.** Run the agent under `hh run` and copy a
   representative slice of its JSONL transcript (normal turns, a tool call +
   tool result pair, a multi-line message, and at least one malformed/truncated
   line) into `hh/tests/fixtures/<adapter>/`. Keep it small and secret-free —
   these ship in the repo. Redact anything sensitive by hand; fixtures must not
   contain real secrets.

2. **Add the parser** in `hh-core/src/adapter.rs`: a new `Adapter` impl, a new
   `AgentKind` variant in `hh-core/src/event.rs`, a new branch in `select()`
   (the `"name" => Some(Box::new(...))` match around line 175), and a detection
   rule (basename match, like `is_claude_desktop` at ~line 196). The parser
   function (e.g. `parse_codex_record`) must return `Result`/`Option` and never
   panic — feed it garbage in unit tests.

3. **Add a fuzz target** under `fuzz/fuzz_targets/<adapter>_jsonl.rs` mirroring
   `claude_jsonl.rs` / `codex_jsonl.rs` / `gemini_jsonl.rs`, and register it in
   `fuzz/Cargo.toml`. The harness calls the parser on arbitrary bytes; it must
   never crash. Add the target name to `just fuzz-all` in the `justfile`.

4. **Add integration coverage** in `hh/tests/` that runs the adapter against the
   fixture corpus end-to-end and asserts the expected events/steps.

5. **Document it** in [docs/adapters.md](docs/adapters.md) (the table of
   recognized adapters) and add a `CHANGELOG.md` entry.

## Fuzzing

Fuzz targets live in `fuzz/` (a detached crate using `cargo-fuzz` + nightly).
There are eight: `claude_jsonl`, `codex_jsonl`, `gemini_jsonl`, `mcp_frame`,
`import_bundle`, `config_toml`, `blob_decompress`, `redact_detect`.

```bash
cargo install cargo-fuzz        # once
rustup toolchain install nightly

just fuzz claude_jsonl 300      # fuzz one target for 300s
just fuzz-all 300               # all targets, 300s each
just fuzz-min claude_jsonl fuzz/artifacts/claude_jsonl/crash-<id>   # minimize
```

CI runs a 60s/target nightly smoke-test. Long runs are local. Every parser of
external input **must** have a target here — never `panic!` on malformed input.

## Property tests & coverage

Storage invariants are property-tested (proptest), not example-tested: blob
refcounts, migration idempotency, step-number assignment, export→import
round-trips.

```bash
just proptest                  # run only the property suites
just cov                       # hh-core HTML coverage report
just cov-check                 # the 80%-line CI gate
```

## Benchmarks

Performance is regression-gated. Criterion benches live in `hh-core/benches/`;
the nightly job fails on >15% regression vs the committed baseline.

```bash
just bench                     # all criterion benches
just bench-storage             # only the (fast) storage benches
just bench-save-baseline ./bench-baseline
just bench-compare ./bench-baseline   # fail on >15% regression
```

`just cold-start` is a manual `hh list` timing check (needs `hyperfine`); it is
not CI-gated because wall clock is too noisy across runners.

## Cutting a release

Releases are driven by [cargo-dist](https://axo.dev/cargo-dist/) from
[`dist-workspace.toml`](dist-workspace.toml). The generated
`.github/workflows/release.yml` is produced by `cargo dist generate` — re-run
it after editing `dist-workspace.toml` and commit the result.

```bash
cargo install cargo-dist --locked --version 0.31.0
just dist-generate             # regenerate .github/workflows/release.yml
just dist-plan v1.0.0          # dry-run the release plan (no builds)
```

Then tag `v<x.y.z>` and push it; cargo-dist builds per-target binaries (with
the git sha embedded via `HH_BUILD_SHA` — NFR-8), attaches SHA-256 checksums
and keyless Sigstore attestations, publishes the shell/powershell installers,
generates completion scripts + the `hh.1` man page via `hh-dist`, and pushes
the Homebrew formula to `halfhand-org/homebrew-tap`.

The docs site (mdbook under `docs/`) publishes to GitHub Pages on release via
`.github/workflows/docs.yml`.

## Release assets (completions + man page)

The `hh-dist` workspace crate generates shell completions and the man page from
the single `Cli` definition in `hh/src/cli.rs` (so they never drift). It is
`publish = false` and excluded from cargo-dist's `packages` list, so
`cargo install halfhand` still installs only `hh`.

```bash
just dist-assets               # generate into target/dist-assets/
```

## Questions

Open a [discussion](https://github.com/halfhandorg/halfhand/discussions) before
a large change — happy to scope it with you.