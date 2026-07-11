# `hh gc` — reclaim disk space

`hh gc` reclaims space the recorder can no longer reach. It prunes orphaned blob
files and stale `blobs` rows, then `VACUUM`s the SQLite database to shrink
`hh.db` on disk. It is the only command that compacts the store, and it is safe:
it removes only blobs no live event references, never referenced data.

```
$ hh gc
✓ Pruned 3 orphan blob file(s) · 1.5 MiB reclaimed · 2 stale row(s) removed
✓ Vacuumed the database (hh.db compacted on disk)
```

## What it removes

| Kind | What it is | Where it comes from |
| --- | --- | --- |
| Orphan blob file | A `blobs/<h2>/<hash>.zst` whose `blobs` row is missing or has refcount `0` | A crash between writing the blob and committing the referencing event, or a `hh delete` that decremented the refcount but crashed before removing the file. |
| Stale `blobs` row | A row whose backing file is missing (refcount `> 0`, file gone) | The file was deleted out of band; the row is removed so a future reference re-creates the blob instead of pointing at a missing file. |

Blobs are content-addressed and refcounted, so `hh delete` already frees a blob
the moment the last session that references it is deleted. `hh gc` only sweeps
the crash/leftover cases `hh delete` does not reach — run it after a crash, or
occasionally to reclaim space and compact the DB.

## `VACUUM` and `--no-vacuum`

`VACUUM` rebuilds `hh.db` into a compacted file. It needs temporary free space
roughly equal to the current DB size and an exclusive connection (no other `hh`
process using the data dir); `hh gc` will tell you so and suggest `--no-vacuum`
if it cannot take that connection.

`--no-vacuum` skips the rebuild and only prunes — useful on a nearly-full disk
or when you just want the quick sweep:

```
$ hh gc --no-vacuum
✓ Pruned 3 orphan blob file(s) · 1.5 MiB reclaimed · 2 stale row(s) removed
● Skipped vacuum (--no-vacuum; run `hh gc` without it to compact)
```

## `--json`

`hh gc --json` emits a stable object (carrying `schema:1`):

```jsonc
{
  "schema": 1,
  "orphan_files_removed": 3,
  "orphan_bytes_reclaimed": 1572864,
  "orphan_rows_removed": 2,
  "vacuumed": true
}
```

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | GC completed. |
| `1` | A prune or vacuum error (e.g. another `hh` process holds the data dir — see the hint). |

(These match Halfhand's exit-code contract: `0` ok, `1` generic error.)

## Notes

- `hh gc` never removes a blob any live event references. Shared blobs survive
  until the last session that uses them is deleted; `hh gc` is orthogonal to
  that lifecycle.
- Every `hh run` already checkpoints the WAL on finalize (so `hh.db` is
  copy-safe at rest after a clean session). `hh gc`'s job is compaction, not
  checkpointing — see `docs/performance.md`.
- `hh gc` is idempotent: running it twice does nothing the second time (counts
  are `0`).