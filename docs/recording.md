# Recording

`hh run` wraps a command in a PTY and records what happens.

```bash
hh run -- claude              # record a Claude Code session (auto-detected)
hh run -- python3 my_agent.py # record any command
hh run -- codex               # any other agent: terminal output + file changes
```

Everything after `--` is the command to record.

## What gets captured

- **Terminal output** — ANSI/UTF-8 bytes (and binary chunks) from the PTY.
- **File changes** — create/modify/delete under the working dir, with BLAKE3
  before/after hashes and zstd-compressed contents.
- **Agent turns** — for Claude Code only, the agent's internal prompts, tool
  calls and tool results, as structured events. Other agents (including Codex
  CLI and Gemini CLI) get terminal output and file changes only. See
  [Adapters](./adapters.md).
- **MCP traffic** — when the agent's MCP servers are wrapped in
  `hh mcp-proxy`.

## File changes: where the before-side comes from

A diff needs two sides. The after-side of a file change is whatever is on
disk when the watcher reads it. The before-side is the content the file had
just before the change, and the recorder resolves it in this order:

| `before_source` | When | Before-side content |
|---|---|---|
| `live` | The watcher already recorded this path earlier in the session. | That earlier change's after-side. |
| `git` | The file existed at session start **and was clean against the git index**. | The index blob, read with `git cat-file --filters` (same line endings and filters as a checkout) and stored when the file first changes. |
| `baseline` | The file existed at session start but was **dirty**, **untracked**, or **outside a git repository**. | The file's content, captured when `hh run` started. |
| `none` | A created file, or one whose previous content was not captured (see limits below). | None. The diff shows only added lines (or "content not captured" for a delete). |

So a one-line edit to a file that existed before the session shows as one
removed and one added line. Deleting a pre-existing file shows the removed
content. That holds in a git repository and outside one.

### The git-clean rule

Halfhand uses git only for files that were clean against the index when the
session started. It never diffs against `HEAD`, and it never uses the index
for a file you had already edited. Say you have uncommitted edits to
`main.rs`, start `hh run`, and the agent changes one more line. The recorded
diff shows only the agent's line. Diffing against git would show your edits
as well and attribute them to the agent. Dirty and untracked files get a
real snapshot of their content at session start (`baseline`).

"Clean" means: tracked at stage 0 (not mid-merge), a regular file (not a
symlink or submodule), and not listed by `git diff-files`. Halfhand never
refreshes or writes your index. If git's stat cache is stale, a file whose
content didn't change can be reported dirty. That only means its content is
captured as a `baseline` instead. A `git` before-side is also checked
against the file's session-start content hash and dropped (`none`) if they
differ, so a smudge filter or a mid-session index rewrite can't put the
wrong text in a diff.

### Limits

The same rules apply to before-sides as to live captures:

- Files over `max_file_size` (default 4 MiB) are not captured at all.
- Binary files (a NUL byte in the first 8 KiB) store no content unless
  `record_binary = true`.
- Baseline content (dirty, untracked or non-git files) is held in memory
  for the session, up to `[record] baseline_budget` in total (default
  **32 MiB**). Git-clean files don't count against the budget. Files past it
  record their first change with `before_source = "none"`, and `hh run`
  prints one note saying how many files that affected. Raise the budget, or
  commit or stash first, if you need diffs for a large dirty tree. Set it to
  `0` to turn baseline capture off.

```toml
[record]
baseline_budget = "128MiB"   # default: 32MiB; 0 disables baseline capture
```

Nothing is written to the blob store at session start. A before-side is
stored only when its file actually changes, through the same (redacting,
with `[redaction] at_record`) blob store as every other capture. Each
`git`/`baseline` before-side blob is refcounted by its own `file_changes`
row, so `hh rm` and `hh gc` reclaim it like any other blob, `hh scan` and
`hh redact` cover it, and `hh export --bundle` / `hh import` carry it.

### Renames

A rename is recorded as **one** change on the new path, with the old path
in `renamed_from` (`hh inspect --json` shows it in the event `body`).
`hh inspect --diff` prints it the way git does:

```
diff -- a/old.txt b/new.txt
rename from old.txt
rename to new.txt
```

A rename that also changed the content is followed by the usual hunks. The
two halves of a rename are paired by the OS when it reports them together
(Linux inotify, Windows). Otherwise they're paired by identical content
within the same debounce window (macOS FSEvents reports each half
separately). Empty files are never paired by content. Moving a file onto
an existing path is recorded as a modify of the target plus a delete of the
source. If the watcher misses both halves (for example, every file inside a
renamed directory), the shutdown rescan records them and pairs them by
content too.

## Flags

- `--adapter <name>` — force an adapter instead of auto-detecting.
  `claude-code` is the supported value. `codex-cli`, `gemini-cli` and
  `claude-desktop` are **deprecated and scheduled for removal in the next
  minor release**: they still work as before but print a one-line
  deprecation warning, and the Codex and Gemini adapters never captured
  structured events. See [Adapters](./adapters.md#deprecated-flags).
- `--record-input` — also record your keystrokes. **Off by default** because
  keystrokes may contain secrets (SRS NFR-4). Enable only when you need to
  reproduce exact input, and scan/redact before sharing.

## Dropped events and the `⚠` marker

Halfhand never drops events silently. If an event cannot be written to the
database (a full disk, an I/O error, a read-only data dir), recording keeps
going and Halfhand:

1. prints **one** warning line to stderr at the first failure, naming what
   failed and why — not one line per event, so a full disk does not flood
   your terminal;
2. counts every event it could not write, and reports the total in the
   end-of-run summary, with a `⚠` instead of `✓`:

   ```
   ⚠ Recorded session a1b2c3 · 4m32s · 42 steps · 7 files changed · 1312 events dropped · hh replay a1b2c3
   hh: warning: 1312 events could not be written and are missing from session a1b2c3 (first failure: …) — the session is marked degraded; …
   ```

3. marks the session **degraded**: `hh list` and `hh inspect` show its status
   as `✓ ok ⚠` — the same marker used when the agent adapter degrades — so a
   session with gaps in its timeline never looks like a clean `ok`. The count
   and the first failure's cause are saved with the session
   (`dropped_events` / `dropped_events_reason` in `hh list --json`), survive
   `hh export` / `hh import`, and are reported by `hh doctor`.

The session's `status` still reflects how the recorded command exited
(`ok`, `error`, `interrupted`); `⚠` means "this recording is incomplete or
less detailed than it should be", for either reason. To find out why, run
`hh doctor`. It names the cause and the affected sessions; the usual fix is
freeing disk space or fixing permissions on the data dir.

## Health check

`hh doctor` is a read-only probe of the recording stack (PTY, watcher, store).
Run it before a session you suspect is broken, or from CI:

```bash
hh doctor
hh doctor --json | jq
```

See [Doctor](./doctor.md).

## Session status

A session's `status` (shown by `hh list`, `hh inspect` and `--json`) is one of:

| Status | Meaning |
|---|---|
| `recording` | The recording is still running. |
| `ok` | The recorded command exited 0. |
| `error` | The recorded command exited non-zero. |
| `interrupted` | The `hh` process recording the session stopped before finalizing it (it was killed, crashed, or the machine went down). The session is still inspectable up to the last event written. |

### How a crashed recording becomes `interrupted`

When `hh run` starts, the session records which process is recording it: the
`hh` process's id **and** its start time. Every `hh` command (`hh list`,
`hh inspect`, another `hh run`, …) checks each `recording` session and marks it
`interrupted` only when that recorder is gone:

- no process has the recorded id, or
- a process has it, but with a different start time. Process ids are reused,
  so a crashed recorder's id can later belong to an unrelated program; the
  start time tells them apart.

So you can run `hh list` or `hh inspect` in another terminal while
`hh run` is recording, and the live session keeps showing `recording`.

Liveness is checked on Linux (`/proc`), macOS and Windows. When it cannot be
determined (an unsupported platform, no permission to see the process,
`/proc` not mounted, or a data dir shared between machines), Halfhand leaves
the session `recording` rather than risk marking a live recording
`interrupted`. A session stuck in `recording` this way is still fully
inspectable. Sessions recorded by Halfhand versions before this check have no
recorder recorded; they are marked `interrupted` as before.

## Exit code

`hh run` exits with the **recorded command's own exit code**, so
`hh run -- make` can stand in for `make` in a script. That means `2`, `3` and
`4` from `hh run` are the command's codes, not Halfhand's usage /
session-not-found / policy codes. A command killed by a signal makes `hh run`
exit `1`, and so does a failure in `hh` itself before the command starts. See
[Exit codes](./exit-codes.md).
