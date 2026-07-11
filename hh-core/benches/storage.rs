//! Criterion benches for the storage layer (CLAUDE.md v1.0.0 addendum / NFR-1:
//! "criterion benches live in benches/", regression-gated at >15% vs baseline).
//!
//! Four groups, mirroring the four perf-critical paths in the SRS:
//!   - `ingest` — `EventWriter::append_event` throughput (NFR-1 target ≥5k
//!     events/s). One durable round-trip per event.
//!   - `replay_index` — `Store::list_event_index` for 10k / 100k-event sessions
//!     (the `hh replay` open hot path; FR-3.5).
//!   - `blob_write` — `BlobStore::put` for 1 KiB / 1 MiB payloads.
//!   - `blob_read` — `BlobStore::get` (decompress + hash-verify) 1 KiB / 1 MiB.
//!
//! Deviation note: bench setup uses `.unwrap()`. CLAUDE.md forbids `unwrap()`
//! outside `#[cfg(test)]` and the top of `main()`, but benches are dev-only
//! tooling where a fixture/setup failure should panic the run loudly rather than
//! be converted to a user-facing error (the harness is never shipped). The same
//! justification applies to every criterion bench in this directory.

use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use hh_core::{AdapterStatus, AgentKind, BlobStore, Event, EventKind, NewSession, Store};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

/// Bounded criterion settings for the nightly gate: criterion's defaults
/// (100 samples × 5 s ≈ 8 min PER bench) would make the full run ~an hour, so
/// we trade a little confidence for a ~4 min total runtime. 30 samples × 1 s
/// still gives a stable median for a 15 % regression gate; `just bench` and the
/// nightly workflow both run with these settings. Tune up locally by editing
/// this config (or pass `--measurement-time`/`--sample-size` on the CLI).
fn bench_config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(1))
        .sample_size(30)
}

/// Build a throwaway store rooted in a fresh temp dir.
fn open_store() -> (TempDir, Store) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
    (tmp, store)
}

/// A minimal session row (matches the `replay::data` unit-test fixture).
fn new_session() -> NewSession {
    NewSession {
        id: hh_core::Uuid::now_v7(),
        started_at: 0,
        agent_kind: AgentKind::Generic,
        adapter_status: AdapterStatus::None,
        command: vec!["claude".into()],
        cwd: PathBuf::from("/tmp"),
        hostname: None,
        hh_version: "0.1.0-beta.1".into(),
        model: None,
        git_branch: None,
        git_sha: None,
        git_dirty: None,
    }
}

/// Bulk-insert `n` events into a fresh session in ONE transaction via a second
/// connection, then reopen the store. The writer thread's per-event channel
/// round-trip would make a 100k-event fixture take tens of seconds; this is
/// fixture setup, and only the *read* paths (the system under test) are timed.
fn build_indexed_session(n: i64) -> (TempDir, Store, String) {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("hh.db");
    let blobs = tmp.path().join("blobs");
    let store = Store::open(&db, &blobs).unwrap();
    let created = store.create_session(&new_session()).unwrap();
    let sid = created.id.clone();
    drop(store);

    {
        let conn = Connection::open(&db).unwrap();
        conn.execute("BEGIN", []).unwrap();
        let mut stmt = conn
            .prepare(
                "INSERT INTO events
                   (session_id, ts_ms, kind, step, summary, body_json,
                    blob_hash, correlates)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL)",
            )
            .unwrap();
        for i in 0i64..n {
            stmt.execute(params![
                &sid,
                i,
                "agent_message",
                Some((i / 4) + 1),
                format!("event {i}"),
                r#"{"text":"x"}"#,
            ])
            .unwrap();
        }
        drop(stmt);
        conn.execute("COMMIT", []).unwrap();
    }

    let store = Store::open(&db, &blobs).unwrap();
    (tmp, store, sid)
}

/// High-entropy bytes that resist zstd compression, so the blob benches measure
/// realistic compress/decompress + I/O cost rather than the trivial all-zeros
/// fast path (which would compress to a handful of bytes and hide the work).
/// Deterministic (no `rand` dependency); good enough to defeat zstd.
fn entropy_bytes(size: usize) -> Vec<u8> {
    let mut v = vec![0u8; size];
    // Intentional truncation: take the low byte of a 64-bit mix to get a
    // high-entropy value that resists zstd compression.
    #[allow(clippy::cast_possible_truncation)]
    for (i, b) in v.iter_mut().enumerate() {
        *b = (i as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_right(24) as u8;
    }
    v
}

/// `append_event` throughput (NFR-1). Each call is one durable round-trip
/// through the single-writer channel (channel send → writer-thread INSERT →
/// reply), so this measures the real ingest cost a recorder pays per event.
///
/// Batches `INGEST_BATCH` events per criterion iteration and reports
/// `Throughput::Elements(INGEST_BATCH)`. This bounds the session to a small
/// steady-state table (sample_size × INGEST_BATCH ≈ a few thousand rows, so
/// append cost does not drift as an index fills a huge table) while still
/// timing the per-event channel round-trip + autocommit, not criterion's own
/// per-iteration overhead. The baseline and any future run are compared
/// like-for-like by the regression gate.
const INGEST_BATCH: u64 = 128;
fn bench_ingest(c: &mut Criterion) {
    let (_tmp, store) = open_store();
    let created = store.create_session(&new_session()).unwrap();
    let sid = created.id.clone();
    let writer = store.event_writer().unwrap();
    let mut ts: i64 = 0;
    let mut group = c.benchmark_group("ingest");
    group.throughput(Throughput::Elements(INGEST_BATCH));
    group.bench_function("append_event", |b| {
        b.iter(|| {
            for _ in 0..INGEST_BATCH {
                ts += 1;
                let event = Event {
                    session_id: sid.clone(),
                    ts_ms: ts,
                    kind: EventKind::AgentMessage,
                    step: Some(1),
                    summary: "bench".into(),
                    body_json: Some(serde_json::json!({"text": "bench"})),
                    blob_hash: None,
                    blob_size: None,
                    correlates: None,
                };
                let id = writer.append_event(event).unwrap();
                black_box(id);
            }
        });
    });
    group.finish();
    let _ = writer.finish();
}

/// `list_event_index` (the `hh replay` open path) for 10k and 100k-event
/// sessions. The table is built once per size (outside the timed closure); the
/// measured closure re-reads it, so it exercises the SQL cursor + row-mapping +
/// `Vec` allocation that replay pays at startup, with no fixture-build noise.
fn bench_replay_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("replay_index");
    // `n` is i64 (the column type); `u64::try_from` (over `as` casting) keeps
    // clippy's `cast_possible_wrap` quiet for the `Throughput::Elements` count.
    for &n in &[10_000i64, 100_000] {
        let (_tmp, store, sid) = build_indexed_session(n);
        group.throughput(Throughput::Elements(u64::try_from(n).unwrap()));
        group.bench_with_input(BenchmarkId::from_parameter(n), &sid, |b, sid| {
            b.iter(|| {
                let index = store.list_event_index(black_box(sid)).unwrap();
                black_box(index);
            });
        });
    }
    group.finish();
}

/// `BlobStore::put` (BLAKE3 hash + zstd compress + atomic write) for 1 KiB and
/// 1 MiB. Each measured iteration writes into a FRESH temp blob dir (created in
/// the excluded setup closure), so the `path.exists()` fast path never skips
/// the real compress+write — this measures the write cost, not an exists-check.
///
/// The temp dir is RETURNED from the routine so criterion drops it OUTSIDE the
/// timed region: the `TempDir` drop removes the blob file + shard dir + temp
/// dir (several filesystem syscalls) which would otherwise dominate and add
/// high variance to the measured `put`. `BatchSize::SmallInput` bounds the
/// number of in-flight temp dirs (and so the peak disk use).
fn bench_blob_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("blob_write");
    for (label, size) in [("1KiB", 1024usize), ("1MiB", 1024 * 1024)] {
        let payload = entropy_bytes(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &payload,
            |b, payload| {
                b.iter_batched(
                    || {
                        let tmp = TempDir::new().unwrap();
                        let blobs = BlobStore::new(tmp.path().join("blobs"));
                        (tmp, blobs)
                    },
                    |(tmp, blobs)| {
                        let outcome = blobs.put(black_box(payload)).unwrap();
                        black_box(outcome);
                        // Hand `tmp` back so criterion drops it (rmdir + blob removal)
                        // outside the timed region — see the doc comment above.
                        tmp
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

/// `BlobStore::get` (file read + zstd decompress + BLAKE3 verify) for 1 KiB and
/// 1 MiB. The blob is written once per size (setup); the measured closure
/// re-reads it. Reads are pure — the file does not grow across iterations.
fn bench_blob_read(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    let blobs = BlobStore::new(tmp.path().join("blobs"));
    let mut group = c.benchmark_group("blob_read");
    for (label, size) in [("1KiB", 1024usize), ("1MiB", 1024 * 1024)] {
        let payload = entropy_bytes(size);
        let outcome = blobs.put(&payload).unwrap();
        let hash = outcome.hash.clone();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &hash, |b, hash| {
            b.iter(|| {
                let data = blobs.get(black_box(hash)).unwrap();
                black_box(data);
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = bench_config();
    targets = bench_ingest, bench_replay_index, bench_blob_write, bench_blob_read
}
criterion_main!(benches);
