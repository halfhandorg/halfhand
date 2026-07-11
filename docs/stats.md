# `hh stats` — store inventory

`hh stats` is a read-only summary of the Halfhand store: session and event
counts, blob usage, on-disk footprint (the `hh.db` file plus its `-wal`/`-shm`
sidecars and the compressed blob directory), and the largest sessions by event
count. It mutates nothing.

```
$ hh stats
hh stats — /home/me/.local/share/halfhand

sessions  2
events    5
blobs     1 (1.5 KiB on disk, 14 B uncompressed)
disk      hh.db 3.0 MiB · WAL 0 B · blobs 1.5 KiB · total 3.0 MiB

largest sessions
  a1b2c3  4 events
  d4e5f6  1 events
```

`--top N` controls how many largest sessions are listed (default `5`; `--top 0`
lists none but still reports the totals).

## Reading the rows

- **sessions / events** — total rows in `sessions` / `events`.
- **blobs** — `count` blob files, their compressed size on disk, and their
  uncompressed size (what they decode to). Compression ratios vary; the
  uncompressed figure is what the data would cost without zstd.
- **disk** — `hh.db` (the SQLite file), the WAL and shared-memory sidecars, the
  blobs directory, and their sum. A freshly finalized session has a small
  (often `0 B`) WAL because `hh run` checkpoints it on finalize.
- **largest sessions** — the top `N` sessions by event count, with their
  6-char short id, so you can `hh inspect <id>` the biggest ones.

## `--json`

`hh stats --json` emits a stable object (carrying `schema:1`):

```jsonc
{
  "schema": 1,
  "sessions": 2,
  "events": 5,
  "blobs": {
    "count": 1,
    "uncompressed_bytes": 14,
    "on_disk_bytes": 1536
  },
  "disk": {
    "db_bytes": 3145728,
    "wal_bytes": 0,
    "shm_bytes": 32768,
    "blobs_dir_bytes": 1536,
    "total_bytes": 3179904
  },
  "largest_sessions": [
    { "id": "00000000-…", "short_id": "a1b2c3", "events": 4 },
    { "id": "00000000-…", "short_id": "d4e5f6", "events": 1 }
  ]
}
```

`disk.total_bytes` is `db_bytes + wal_bytes + shm_bytes + blobs_dir_bytes`.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Stats gathered. |
| `1` | A read error (e.g. the data dir is unreadable). |

(These match Halfhand's exit-code contract: `0` ok, `1` generic error.)

## Notes

- `hh stats` is read-only and safe to run any time, including mid-recording
  (it opens the store the same way `hh list` does, without blocking a
  concurrent `hh run`).
- To actually reclaim the space `hh stats` reports as reclaimable, run
  `hh gc` (see `docs/gc.md`).