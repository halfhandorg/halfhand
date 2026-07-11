//! Criterion bench for Claude JSONL adapter conversion throughput (CLAUDE.md
//! v1.0.0 addendum / NFR-1). Requires the `fuzzing` feature, which exposes
//! `hh_core::adapter::fuzzing::fuzz_parse_line` — the existing dev-only entry
//! point into the otherwise-private Claude JSONL parser (`parse_record`). That
//! is the exact parse sequence the live tailer runs on every tailed transcript
//! line (UTF-8 → trim → serde_json::from_str → parse_record), so this measures
//! the conversion cost the recorder pays per transcript line.
//!
//! The fixture is a fixed set of small, representative records (user text,
//! assistant message with a thinking block + tool_use, a tool_result). They
//! stay well under the 256 KiB blob-spill threshold, so `parse_record` never
//! touches the blob store — the bench measures pure parse/convert work, not
//! disk I/O (that is `storage.rs`'s `blob_*` groups). Because the fixture is
//! fixed (not random), any content-addressed blob the parser might write is
//! deduped to a bounded set written once; no unbounded disk growth.
//!
//! Run with `cargo bench -p hh-core --features fuzzing` (or `just bench`).
//!
//! Deviation note: same `.unwrap()`-in-bench rationale as `storage.rs`.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use hh_core::adapter::fuzzing::fuzz_parse_line;
use std::time::Duration;

/// Bounded criterion settings — see `storage.rs::bench_config`. Same reduced
/// warmup/measurement/sample so the nightly run stays ~minutes, not ~an hour.
fn bench_config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(1))
        .sample_size(30)
}

/// Three representative Claude Code transcript records, serialized to JSONL
/// bytes. Mirrors the shapes covered by the adapter's unit-test snapshots
/// (FR-1.5) so the bench exercises the real conversion branches: a user
/// message, an assistant message with thinking + a tool_use, and a tool_result
/// user message.
fn fixture_lines() -> Vec<Vec<u8>> {
    [
        r#"{"type":"user","isSidechain":false,"isMeta":false,"message":{"role":"user","content":"List the files in the repo and show Cargo.toml."},"timestamp":"2026-07-02T06:14:40.699Z","cwd":"/tmp/work","sessionId":"s"}"#,
        r#"{"type":"assistant","isSidechain":false,"isMeta":false,"message":{"role":"assistant","model":"glm-5.2","content":[{"type":"thinking","thinking":"I'll list files then read Cargo.toml.","signature":""},{"type":"tool_use","id":"call_abc123","name":"Bash","input":{"command":"ls -la"}}]},"timestamp":"2026-07-02T06:14:41.000Z","cwd":"/tmp/work","sessionId":"s"}"#,
        r#"{"type":"user","isSidechain":false,"isMeta":false,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_abc123","content":"total 0\ndrwxr-xr-x 2 root root 40 Jul 10 03:12 ."}]},"timestamp":"2026-07-02T06:14:41.500Z","cwd":"/tmp/work","sessionId":"s"}"#,
    ]
    .into_iter()
    .map(|s| s.as_bytes().to_vec())
    .collect()
}

/// Conversion throughput in records/second. `Throughput::Elements(N)` makes
/// criterion report elements/s across the whole fixture per iteration.
fn bench_adapter_jsonl(c: &mut Criterion) {
    let lines = fixture_lines();
    let mut group = c.benchmark_group("adapter_jsonl");
    group.throughput(Throughput::Elements(lines.len() as u64));
    group.bench_function("parse_lines", |b| {
        b.iter(|| {
            for line in &lines {
                fuzz_parse_line(black_box(line));
            }
        });
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = bench_config();
    targets = bench_adapter_jsonl
}
criterion_main!(benches);
