//! Criterion bench for FTS5 search throughput (CLAUDE.md v1.0.0 addendum /
//! NFR-1). Seeds a store with 100 sessions of ~1000 events each, then measures
//! search latency for various query types. Target: <100 ms for a typical query.
//!
//! Run with `cargo bench -p hh-core` (or `just bench`).

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use hh_core::event::{Event, EventKind, NewSession};
use hh_core::store::{SearchFilters, Store};
use hh_core::{AgentKind, AdapterStatus};
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use uuid::Uuid;

/// Bounded criterion settings — same reduced warmup/measurement/sample as the
/// existing storage benches so the nightly run stays ~minutes, not ~an hour.
fn bench_config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(1))
        .sample_size(30)
}

/// Seed a store with `session_count` sessions, each with `events_per_session`
/// events containing varied text for FTS5 search.
#[allow(clippy::explicit_counter_loop)] // s is a loop variable, not a counter
fn seed_store(
    dir: &Path,
    session_count: u32,
    events_per_session: u32,
) -> Store {
    let store = Store::open(&dir.join("hh.db"), &dir.join("blobs")).unwrap();
    let mut started_at = 1_700_000_000_000i64;

    for s in 0..session_count {
        let new = NewSession {
            id: Uuid::now_v7(),
            started_at,
            agent_kind: AgentKind::ClaudeCode,
            adapter_status: AdapterStatus::Active,
            command: vec!["claude".into()],
            cwd: Path::new("/tmp").to_path_buf(),
            hostname: None,
            hh_version: "0.1.0-beta.1".into(),
            model: None,
            git_branch: None,
            git_sha: None,
            git_dirty: None,
        };
        let created = store.create_session(&new).unwrap();
        let writer = store.event_writer().unwrap();

        for e in 0..events_per_session {
            let kind = match e % 5 {
                0 => EventKind::UserMessage,
                1 => EventKind::AgentMessage,
                2 => EventKind::ToolCall,
                3 => EventKind::ToolResult,
                _ => EventKind::Thinking,
            };
            let summary = format!("event {s}/{e}: {}",
                match kind {
                    EventKind::UserMessage => "user asked about file system",
                    EventKind::AgentMessage => "agent responded with solution",
                    EventKind::ToolCall => "tool_call: Bash ls -la",
                    EventKind::ToolResult => "tool_result: file contents returned",
                    _ => "thinking about the problem",
                }
            );
            let body_text = format!(
                "{{\"text\":\"Session {s} event {e}: {} additional context here\"}}",
                match kind {
                    EventKind::UserMessage => "list files in the current directory",
                    EventKind::AgentMessage => "here is the solution to your problem",
                    EventKind::ToolCall => "Bash command to list directory contents",
                    EventKind::ToolResult => "total 42\ndrwxr-xr-x file1.txt\ndrwxr-xr-x file2.rs",
                    _ => "let me think about this step by step",
                }
            );
            let body: serde_json::Value = serde_json::from_str(&body_text).unwrap();
            writer.append_event(Event {
                session_id: created.id.clone(),
                ts_ms: i64::from(e) * 100,
                kind,
                step: Some(i64::from(e) + 1),
                summary,
                body_json: Some(body),
                blob_hash: None,
                blob_size: None,
                correlates: None,
            }).unwrap();
        }
        writer.finish().unwrap();
        store.finalize_session(&created.id, started_at + 100_000, Some(0), hh_core::SessionStatus::Ok).unwrap();
        started_at += 1;
    }
    store
}

/// Measure search latency for a single-word query.
fn bench_search_single_word(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    let store = seed_store(tmp.path(), 100, 1000);
    let filters = SearchFilters::default();

    let mut group = c.benchmark_group("search");
    group.throughput(Throughput::Elements(1));
    group.bench_function("single_word", |b| {
        b.iter(|| {
            let results = store.search(black_box("\"directory\""), &filters).unwrap();
            black_box(results);
        });
    });
    group.finish();
}

/// Measure search latency for a phrase query.
fn bench_search_phrase(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    let store = seed_store(tmp.path(), 100, 1000);
    let filters = SearchFilters::default();

    let mut group = c.benchmark_group("search");
    group.throughput(Throughput::Elements(1));
    group.bench_function("phrase", |b| {
        b.iter(|| {
            let results = store.search(black_box("\"list files\""), &filters).unwrap();
            black_box(results);
        });
    });
    group.finish();
}

/// Measure search latency with a filter.
fn bench_search_filtered(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    let store = seed_store(tmp.path(), 100, 1000);
    let filters = SearchFilters {
        event_kind: Some(EventKind::ToolCall),
        ..Default::default()
    };

    let mut group = c.benchmark_group("search");
    group.throughput(Throughput::Elements(1));
    group.bench_function("filtered_by_kind", |b| {
        b.iter(|| {
            let results = store.search(black_box("\"Bash\""), &filters).unwrap();
            black_box(results);
        });
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = bench_config();
    targets = bench_search_single_word, bench_search_phrase, bench_search_filtered
}
criterion_main!(benches);
