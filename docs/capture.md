# Effect capture and capture health

`hh run` records what an agent *did* to your files by watching the working
directory. This page covers what that watcher sees, what it can't see, the
limits on each platform, and how to read `hh doctor --capture`, the report
that tells you whether a recording is complete.

## What effect capture sees

The recorder watches one root: the directory you ran `hh run` from (the
session's `cwd`), recursively. For every file under it that is not ignored, it
records:

- **creates**, with the new content;
- **modifications**, with a real before-side (see
  [Recording → where the before-side comes from](recording.md#file-changes-where-the-before-side-comes-from))
  and the new content;
- **deletes**, with the deleted content as the before-side;
- **renames**, as one change on the new path that names the old path
  (`renamed_from`), not as an unrelated delete and create.

Changes reach the database by two routes:

1. **Live.** The OS file-event backend (inotify on Linux, FSEvents on macOS,
   `ReadDirectoryChangesW` on Windows) reports the change while the agent
   runs. After a short debounce it is reconciled against the disk and
   recorded.
2. **Reconciliation.** When the agent exits, the recorder drains late events
   and then re-walks the root. Anything the backend never reported is
   recovered here by comparing the disk with the session-start baseline and
   with what was already recorded. This includes changes inside a renamed
   directory and events a backend dropped.

The session's exit code and terminal output are recorded too, so a fidelity
check can compare the whole run against what the agent did.

## What it cannot see

- **Anything outside the root.** A write to `~/.bashrc` or `/tmp` from an
  agent started in `~/project` is not recorded.
- **Ignored paths.** Paths matched by the root `.gitignore`, your global
  gitignore, built-in ignores (`.git/`, `node_modules/`, `target/`) and
  `[record] ignore` are skipped on purpose.
- **Files over `max_file_size`** (default 4 MiB). Binary files store no
  content unless `record_binary = true`.
- **Intermediate states.** A file written three times inside one debounce
  window is one change. A file created and deleted inside one window is not
  recorded.
- **Before-sides beyond the baseline budget.** A dirty, untracked or non-git
  file past `[record] baseline_budget` records its first change with
  `before_source = "none"`.
- **Who made the change.** Every change under the root during the session is
  attributed to the session, even if your editor made it.

## Per-platform limits

| Platform | Backend | Limit | What `hh doctor --capture` reports |
|---|---|---|---|
| Linux | inotify | One watch per directory, capped per user by `fs.inotify.max_user_watches`. When the cap is hit, new directories go unwatched. Reconciliation still finds their files at exit, but nothing is recorded live. | **inotify watch headroom**: free watches for your user (the cap minus the `inotify wd:` entries in every `/proc/<pid>/fdinfo` it can read). Warns below 1024 free. Fix with `sysctl fs.inotify.max_user_watches=524288`. |
| macOS | FSEvents | Folders protected by privacy permissions (Desktop, Documents, Downloads, iCloud Drive, removable volumes) cannot be read until your terminal has access. FSEvents reports nothing for them. Deliveries can also arrive late or not at all on busy or CI machines, and reconciliation covers that. | **privacy-protected root**: whether the most recent session's root can be read. `blocked` means grant your terminal access under System Settings → Privacy & Security → Files and Folders (or Full Disk Access). |
| Windows | ReadDirectoryChangesW | The kernel event buffer can overflow under a burst of changes, and the backend then loses events. Reconciliation recovers the net changes at exit. | **event-buffer overflow**: always `—`. Overflow signals are not persisted yet. |

## Reading a capture-health report

```
$ hh doctor --capture
✓ data dir writable — /home/me/.local/share/halfhand is writable
…

capture health — session 0190aa (error, exit 3)
  ● watched roots           /home/me/work  the session cwd; whether the watch succeeded is not persisted
  ● observed live           —              not persisted per event
  ● recovered by rescan     —              not persisted per event
  ● file changes            5              2 created, 2 modified, 1 deleted; 1 renamed
  ✓ dropped events          0
  ● permission denied       —              not persisted; `hh run` prints a warning when a watch is refused
  ✓ before-side coverage    100%           3 of 3 modified/deleted changes have a real before-side
  ✓ inotify watch headroom  65000 free     536 of 65536 watches in use by this user
```

The report covers the **most recent session** and appears after the usual
doctor checks. It is informational: `--capture` never changes `hh doctor`'s
exit code. `✓` is healthy, `✗` needs attention, and `●` is information or an
unknown value.

**Unknown is a dash, never 0.** Some facts are not stored by the recorder
today, so they always print `—` (`null` in JSON):

- **observed live / recovered by rescan**: which route each change took is
  not stored per event.
- **permission denied**: a refused watch prints a warning during `hh run`
  but is not stored.
- **event-buffer overflow** (Windows): not stored.

The **watched roots** row shows the session's cwd, which is the root the
recorder watches. Whether that watch succeeded is not stored either.

### Before-side coverage

This is the number that tells you whether capture worked. It is the share of
**modified and deleted** changes whose before-side is real content (a
`before_source` of `live`, `git` or `baseline`). Creates are left out
because a new file has no before-side. Rows recorded before before-sides were
tracked (`before_source` is `NULL`) are left out and counted separately.

- **≥ 95%**: healthy (`✓`).
- **below 95%** (`✗`): some edits will look like whole-file additions in
  `hh inspect --diff`. The usual causes are files over `max_file_size`,
  binary files, or a dirty tree larger than `baseline_budget`.
- **`—`**: nothing to measure. The session modified or deleted no files.

### Dropped events

A non-zero count means the recorder could not write some events, and the
session's timeline has gaps. The first cause is shown. See
[Recording → Dropped events](recording.md#dropped-events-and-the--marker).

## `--json`

`hh doctor --capture --json` adds one `capture` object to the usual doctor
JSON. Every existing field is unchanged.

```jsonc
{
  "schema": 2,
  "status": "ok",
  "checks": [ /* unchanged */ ],
  "capture": {
    "session": { "id": "0190…", "short_id": "0190aa", "status": "error", "exit_code": 3 },
    "watched_roots": ["/home/me/work"],
    "events_live": null,          // not persisted
    "events_reconciled": null,    // not persisted
    "file_changes": { "total": 5, "created": 2, "modified": 2, "deleted": 1, "renamed": 1 },
    "dropped_events": 0,
    "dropped_events_reason": null,
    "permission_denied": null,    // not persisted
    "before_side_coverage": { "percent": 100.0, "covered": 3, "eligible": 3, "unknown_provenance": 0 },
    "platform": { "os": "linux", "check": "inotify watch headroom", "status": "ok", "value": "65000 free", "detail": "…" }
  }
}
```

With no recorded sessions, `session`, `file_changes`, `dropped_events`,
`before_side_coverage` and the not-persisted fields are `null`, and
`watched_roots` is `[]`. `before_side_coverage.percent` is `null` when there
is nothing to measure. `platform.status` is `ok`, `warn` or `unknown`.
