# Halfhand dev tasks. Install `just`: https://github.com/casey/just
#
# CI runs a short (60s/target) nightly fuzz smoke-test and a per-PR coverage
# gate; these recipes are for longer local runs of the same tooling.

# Fuzz one target for `seconds` (default 300s = 5 min). Requires nightly +
# `cargo install cargo-fuzz`.
fuzz target seconds="300":
    cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}}

# Fuzz all four targets sequentially, `seconds` each (default 300s = 5 min).
fuzz-all seconds="300":
    for t in claude_jsonl mcp_frame config_toml blob_decompress; do \
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
