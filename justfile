# Halfhand dev tasks. Install `just`: https://github.com/casey/just
#
# CI runs a short (60s/target) nightly fuzz smoke-test and a per-PR coverage
# gate; these recipes are for longer local runs of the same tooling.

# Fuzz one target for `seconds` (default 300s = 5 min). Requires nightly +
# `cargo install cargo-fuzz`.
fuzz target seconds="300":
    cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}}

# Fuzz all five targets sequentially, `seconds` each (default 300s = 5 min).
fuzz-all seconds="300":
    for t in claude_jsonl codex_jsonl gemini_jsonl mcp_frame import_bundle config_toml blob_decompress redact_detect; do \
        echo "=== $t ==="; \
        cargo +nightly fuzz run $t -- -max_total_time={{seconds}} || exit 1; \
    done

# Minimize a crash/hang input found by `fuzz`, e.g.:
#   just fuzz-min claude_jsonl fuzz/artifacts/claude_jsonl/crash-abc123
fuzz-min target input:
    cargo +nightly fuzz tmin {{target}} {{input}}

# hh-core coverage report (HTML), gated at an 80% line floor in CI.
cov:
    cargo llvm-cov --package hh-core --html --open

# Same coverage check CI runs (fails under 80% lines on hh-core).
cov-check:
    cargo llvm-cov --package hh-core --fail-under-lines 80

# Property tests only (proptest suites are part of `cargo test`, this just
# filters to them for a faster local loop).
proptest:
    cargo test --workspace -- prop_ blob_refcount_prop migration_idempotency_prop

# Run all criterion benches (CLAUDE.md v1.0.0 addendum / NFR-1). `--features
# fuzzing` enables the `adapter_jsonl` bench (which uses the dev-only
# `adapter::fuzzing` parser entry point); the three storage benches run
# ungated. The nightly regression gate (`.github/workflows/bench-nightly.yml`)
# runs the same command. Results land in `target/criterion/`.
bench:
    cargo bench -p hh-core --features fuzzing

# Run only the (fast) storage benches, ungated — no `fuzzing` feature needed.
# Useful for a quick local loop without rebuilding the adapter/fuzz surface.
bench-storage:
    cargo bench -p hh-core --bench storage

# Compare the latest criterion run (target/criterion) against a saved baseline
# dir, failing on >15% regression. Same script the nightly gate uses. Save a
# baseline with `just bench-save-baseline <dir>` first, then:
#   just bench-compare ./bench-baseline
bench-compare baseline-dir:
    python3 .github/scripts/bench_compare.py --current target/criterion --baseline {{baseline-dir}} --threshold 0.15

# Snapshot the latest criterion run (target/criterion/<group>/<bench>/new/
# estimates.json) into <dir> as a comparison baseline for `bench-compare`.
# `bench-baseline/` is gitignored (a local/CI cache artifact, not committed).
bench-save-baseline baseline-dir="bench-baseline":
    mkdir -p {{baseline-dir}} && \
    find target/criterion -path '*/new/estimates.json' | while read -r est; do \
        rel="$${est#target/criterion/}"; \
        rel="$$(dirname "$$(dirname "$$rel")")"; \
        mkdir -p "{{baseline-dir}}/$$rel"; \
        cp "$$est" "{{baseline-dir}}/$$rel/estimates.json"; \
    done && \
    echo "Saved baseline to {{baseline-dir}}"

# Generate release assets (shell completions + `hh.1` man page) into <dir>.
# Same command cargo-dist's extra-artifacts step runs on the build host. The
# generator lives in the `hh-dist` crate so `cargo install halfhand` still
# installs only the `hh` binary.
dist-assets dir="target/dist-assets":
    cargo run -p hh-dist -- {{dir}}

# Regenerate cargo-dist's release workflow (.github/workflows/release.yml)
# from dist-workspace.toml. Requires `cargo install cargo-dist --locked
# --version 0.31.0`. Run after editing dist-workspace.toml and commit the
# generated workflow (see CONTRIBUTING.md "Cutting a release").
dist-generate:
    cargo dist generate

# Dry-run the release plan for a tag without building anything. Requires
# cargo-dist (see dist-generate). Example: just dist-plan v1.0.0
dist-plan tag="v1.0.0":
    cargo dist plan --tag {{tag}}

# `hh list` cold-start manual check (Area 4: <50 ms on a warm cache). NOT
# CI-gated — wall clock is too noisy across CI runners; this is a developer-
# experience target, not a correctness contract. Requires `hyperfine`
# (`cargo install hyperfine`), which is NOT a build dependency. Builds a release
# binary, seeds a temp data dir with one session, warms the OS cache, and times
# `hh list`. See docs/performance.md for what makes `hh list` stay fast.
cold-start:
    cargo build --release --bin hh && \
    data="$$(mktemp -d)" && \
    HH_DATA_DIR="$$data" ./target/release/hh run -- echo warmup >/dev/null 2>/dev/null || true && \
    hyperfine --warmup 3 --runs 20 "HH_DATA_DIR=$$data ./target/release/hh list" ; \
    status=$$? ; rm -rf "$$data" ; exit $$status
