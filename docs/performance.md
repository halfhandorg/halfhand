# Performance

Halfhand is a local-first flight recorder, so its performance budget is felt,
not announced: `hh replay` opening instantly, `hh list` staying out of the way
of a shell prompt, and a long recording not falling behind the agent. This page
is the reference for the four performance-critical paths the SRS names, how to
measure them, and what the regression gate does and does not enforce.

The numbers below are **reference, not the gate**. The gate compares
tonight's run against last night's run on the *same runner class*; the absolute
figures shift with hardware. Treat the reference numbers as "roughly what a
developer machine sees," and the gate as "did we get slower than yesterday."

## The four benches

Criterion benches live in `hh-core/benches/` (CLAUDE.md v1.0.0 addendum / NFR-1).
Four groups, one per perf-critical path:

| Group | What it measures | SRS hook |
| --- | --- | --- |
| `ingest` | `EventWriter::append_event` throughput — one durable round-trip through the single-writer channel per event (channel send → writer-thread INSERT → reply). | NFR-1 (≥5,000 events/s) |
| `replay_index` | `Store::list_event_index` for 10k and 100k-event sessions — the `hh replay` open hot path. | FR-3.5 (<300 ms for 10k); Area 2 (<1 s for 100k) |
| `blob_write` | `BlobStore::put` (BLAKE3 + zstd + atomic write) for 1 KiB / 1 MiB. | NFR-1 (file-change blob round-trips) |
| `blob_read` | `BlobStore::get` (read + zstd decompress + BLAKE3 verify) for 1 KiB / 1 MiB. | replay / inspect body fetch |
| `adapter_jsonl` | Claude JSONL line → event conversion via the dev-only `adapter::fuzzing` parser entry. | FR-1.5 adapter tail |

`ingest` batches 128 events per criterion iteration and reports
`Throughput::Elements(128)`, bounding the session to a small steady-state table
so append cost does not drift as an index fills. `blob_write` returns its temp
dir from the measured closure so the `TempDir` drop (the blob file + shard dir +
rmdir) happens *outside* the timed region — otherwise that syscall noise would
dominate and add false variance.

Run them:

```
just bench                      # all four (enables `fuzzing` for adapter_jsonl)
just bench-storage               # the three storage benches, ungated (faster loop)
cargo bench -p hh-core --features fuzzing   # what the nightly gate runs
```

The nightly gate (`cargo bench` then `bench_compare.py`) runs with bounded
criterion settings (1 s warmup, 1 s measurement, 30 samples ≈ 4 min total);
criterion's defaults (100 × 5 s ≈ 8 min *per bench*) would make the run ~an
hour. Tune up locally by editing `bench_config()` in the bench files or passing
`--measurement-time` / `--sample-size` on the CLI.

## Reference numbers

From a `cargo bench -p hh-core --features fuzzing` run (developer-class Linux
machine, bounded settings). These are reference points, not pass/fail:

| Bench | Median | Throughput |
| --- | --- | --- |
| `ingest/append_event` | 18.9 ms / 128 events | ~6.8k events/s (storage layer) |
| `replay_index/10000` | 5.0 ms | FR-3.5 <300 ms ✓ |
| `replay_index/100000` | 49 ms | Area 2 <1 s ✓ |
| `blob_write/1KiB` | 6.1 ms | — |
| `blob_write/1MiB` | 11.6 ms | ~86 MiB/s |
| `blob_read/1KiB` | 6.7 µs | — |
| `blob_read/1MiB` | 1.05 ms | ~950 MiB/s |
| `adapter_jsonl/parse_lines` | 7.6 µs / 3 lines | ~395k records/s |

### NFR-1 (ingest ≥5,000 events/s)

The bench measures the *storage layer* in isolation — `append_event` through the
single-writer channel with `synchronous = NORMAL` (no per-commit fsync; the WAL
fsyncs at checkpoint, matching NFR-3's "fsync on session finalize" design). At
that layer it sustains ~6.8k events/s, above the 5k/s target.

NFR-1 is an end-to-end target, and the real recorder (PTY reader → adapter
parse → serialize → channel → writer) runs slower than the storage layer
alone. The `synchronous = NORMAL` fix lifted end-to-end ingest ~5× off the old
per-event-fsync baseline (see the `### Performance` entry in the CHANGELOG);
closing the remaining gap to 5k/s end-to-end needs statement caching + a batched
writer, tracked as follow-up. The regression gate below protects the storage-
layer gain from slipping back.

### Big sessions (Area 2)

`hh replay` opens by loading the event index (`list_event_index`), then fetches
bodies lazily through a bounded 50-entry LRU cache (`ReplayData`). At 100k
events the index load is 49 ms and the body cache is verified bounded at capacity
across unbounded scrolling (`get_evicts_lru_to_stay_bounded_at_capacity`), so
open stays <1 s and navigation does not grow memory with session size.

`hh inspect --json` streams a session as NDJSON through
`Store::for_each_event_detail` — one `EventDetail` at a time, never collecting
the whole session into RAM — so a 100k-event session inspects in constant
memory. (An `#[ignore]` test, `for_each_event_detail_streams_100k_session`,
exercises the 100k path end-to-end with `--ignored`.)

## The nightly regression gate

`.github/workflows/bench-nightly.yml` runs the benches nightly and on
`workflow_dispatch`, then `.github/scripts/bench_compare.py` compares each
bench's median against the previous night's baseline, **failing on >15%
regression**. A bench is a regression when
`current_median > baseline_median × 1.15`.

### Why a rolling baseline, not a committed one

A baseline committed to git and generated on one machine, compared on another,
is dominated by hardware differences, not code changes — a 15% gate would be
meaningless across an Intel laptop vs an ARM cloud runner. Instead the workflow
uses an `actions/cache` **rolling baseline keyed by runner OS**: each run
restores the previous run's baseline (`bench-baseline-<OS>-`), compares against
it, then — only on a passing run — saves its own results as the new baseline
(`bench-baseline-<OS>-<run_id>`). Current-vs-baseline is therefore the same
runner class, and the 15% threshold absorbs pool variance within that class.
A regressed run is never adopted as the baseline (the seed step runs only on
`if: success()`), so the gate always tracks the last *passing* run.

### Establishing, not failing, on a cold cache

When the cache is cold (first run, or after eviction), there is no baseline to
compare against. Benches present in the current run but absent from the baseline
are reported as **establishing** — no comparison, never a failure — and the
run seeds the baseline for next time. The compare script's exit codes are part
of the contract: `0` pass, `2` regression, `3` establishing/could-not-compare.

### Local comparison

```
just bench                                  # run the benches (writes target/criterion)
just bench-save-baseline ./bench-baseline   # snapshot this run as the baseline
# ... make a change ...
just bench                                  # run again
just bench-compare ./bench-baseline         # same script the nightly gate uses
```

`bench-baseline/` is gitignored (a local/CI cache artifact, not committed).
`bench-compare` walks `target/criterion/<group>/<bench>/new/estimates.json`
(criterion writes this on every run — no special flag) and reads each median
point estimate in nanoseconds, criterion's native unit for `estimates.json`.

## `hh list` cold start (Area 4)

**Target:** `hh list` cold start <50 ms on a warm cache. This is a manual check,
not CI-gated — wall-clock timing is too noisy across CI runners to gate on, and
the 50 ms budget is a developer-experience target, not a correctness contract.

The two things that used to make `hh list` scale with recorded history are both
fixed:

- **No blob-dir scan on open.** `Store::open` `mkdir -p`s the blob dir but does
  not read it; blob-dir scans only happen in `hh gc` (prune) and `hh stats`
  (footprint), never in `list`. `list` runs one `SELECT` against `sessions`.
- **O(rows-needing-heal) step probe.** `Store::open`'s step self-heal used to
  scan every event on every invocation, so cold start grew with total history.
  Migration 0002 added a partial index
  `idx_events_needs_heal` (`WHERE step IS NULL AND kind != 'terminal_output'`),
  making the probe O(rows that actually need healing) — near-zero for a healthy
  store. Additive (a new index on an existing table); no breaking change.

### Running the check (hyperfine)

`hyperfine` is a separate install (`cargo install hyperfine`); it is not a build
dependency. The `just cold-start` recipe builds a release binary, seeds a temp
data dir with a couple of sessions, warms the OS cache, and runs `hh list`
under hyperfine:

```
just cold-start
```

which runs (roughly):

```
cargo build --release --bin hh
export HH_DATA_DIR="$(mktemp -d)"
./target/release/hh run -- echo warmup >/dev/null   # seed a session
hyperfine --warmup 3 --runs 20 \
  "HH_DATA_DIR=$HH_DATA_DIR ./target/release/hh list"
```

On a warm cache (after `--warmup 3`), a developer-class machine sees `hh list`
around 2 ms — well under the 50 ms budget. The budget has headroom for the
config parse and store open (migrations are idempotent and cheap; the heal probe
is near-zero) without scaling with how much you have recorded.

> If `hyperfine` is unavailable, a quick fallback is a shell loop:
> ```
> export HH_DATA_DIR="$(mktemp -d)"; ./target/release/hh run -- echo x >/dev/null
> for i in $(seq 1 20); do /usr/bin/time -f '%e' ./target/release/hh list >/dev/null; done
> ```
> (coarser than hyperfine, but enough to spot a regression into the tens of ms).