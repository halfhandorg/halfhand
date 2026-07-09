# `hh doctor` — recording-stack health check

`hh doctor` is a read-only diagnostic for the Halfhand recording stack. It runs a
fixed set of checks and prints one `✓` / `✗` line per check, exiting nonzero if
any check fails. It does not mutate the database (`PRAGMA integrity_check` is a
read-only probe) and writes nothing to your data dir beyond a self-cleaning
watcher probe file it removes immediately.

```
$ hh doctor
✓ data dir writable — /home/me/.local/share/halfhand is writable
✓ database integrity — PRAGMA integrity_check: ok
✗ config resolution — config: /home/me/.config/halfhand/config.toml; ignored non-canonical file(s): /home/me/.config/halfhand/halfhand.toml — move their contents into config.toml so they take effect
✓ claude jsonl discoverable — /home/me/.claude/projects/-home-me-work/abc.jsonl (newest record type: user)
✓ watcher smoke test — file-change event delivered
```

Run it when a session finalized `ok` but recorded `0 steps` or `0 files changed`,
or before a recording you cannot afford to lose. It targets exactly the failure
modes behind that class of silent breakage.

## Checks

| Check | What it verifies | What a failure means |
| --- | --- | --- |
| `data dir writable` | Halfhand can create + write + delete a file in the data dir | The recorder cannot persist sessions. Fix the dir's permissions or set `HH_DATA_DIR` to a writable directory. |
| `database integrity` | `PRAGMA integrity_check` returns `ok` | The SQLite DB is corrupt; back up the data dir and re-record. |
| `config resolution` | The canonical `config.toml` is in effect and no non-canonical file is being silently ignored | A `halfhand.toml` / `hh.toml` is present but ignored — its ignore globs / `data_dir` never applied. Move its contents into `config.toml`. |
| `claude jsonl discoverable` | `~/.claude/projects/<slug>/` has a transcript for the current cwd, and its first `type`-bearing record parses as JSON | The Claude Code adapter tailer reads these files; a missing or unparseable transcript is the direct cause of a session that records `0 steps`. Run a Claude Code session from this directory first. |
| `watcher smoke test` | `notify` delivers a file-change event on this platform | A watcher that silently never fires explains `0 files changed`. |

## `--json`

`hh doctor --json` emits a stable, documented object (carrying `schema:1`)
with a `status` of `ok` or `fail` and a per-check array, for scripting / CI:

```jsonc
{
  "schema": 1,
  "status": "fail",
  "checks": [
    { "name": "data dir writable",        "status": "ok",   "detail": "…" },
    { "name": "database integrity",       "status": "ok",   "detail": "PRAGMA integrity_check: ok" },
    { "name": "config resolution",        "status": "fail", "detail": "…" },
    { "name": "claude jsonl discoverable", "status": "ok",   "detail": "…" },
    { "name": "watcher smoke test",        "status": "ok",   "detail": "file-change event delivered" }
  ]
}
```

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Every check passed. |
| `1` | At least one check failed. |

(These match Halfhand's exit-code contract: `0` ok, `1` generic error.)

## Notes

- The Claude-Code-transcript check is read-only with respect to your data: it
  opens the newest `*.jsonl` for your cwd and parses its first record; it never
  writes to your `~/.claude` tree.
- `hh doctor` reuses the same `notify` backend the recorder uses, so a watcher
  failure here explains a session that recorded `0 files changed`.
- Running `hh doctor` opens the store the same way `hh run` does (including the
  best-effort reconcile of stale `recording` sessions into `interrupted`), so it
  exercises the same config-resolution path that a real recording would.