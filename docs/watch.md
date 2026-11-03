# Watching a directory (`hh watch`)

`hh run -- <agent>` records an agent it launches. Some agents can't be
launched that way: a GUI app, an IDE extension, or an agent session that is
already running in another terminal. `hh watch` records what such an agent
**does** — every file it creates, edits, renames or deletes in a directory —
without starting it.

```bash
hh watch                          # the current directory, until Ctrl-C
hh watch ~/code/site              # a specific directory
hh watch ~/code/site --daemon     # in the background
hh watch --status                 # what is being watched
hh watch ~/code/site --stop       # finish a background watch
hh watch --idle-timeout 15m       # finish after 15 minutes without changes
```

A watch session is an ordinary session: it appears in `hh list`, and
`hh replay`, `hh inspect --diff`, `hh export`, `hh scan` and `hh search` work
on it like any other.

## Worked example: recording a GUI agent's work

Say a desktop coding agent is about to refactor `~/code/site`, and you want a
record of exactly what it changed.

1. Start watching the project before you give the agent its task:

   ```console
   $ hh watch ~/code/site --daemon --name "desktop agent: auth refactor"
   ● Watching /home/me/code/site in the background · session 3fa2c1 · pid 48213 · stop with `hh watch --stop`
   ```

   From the moment that line prints, every change under the directory is
   recorded. The files already there are the session's baseline, so the
   first edit to each one diffs against what it held before the agent
   touched it.

2. Work with the agent in its own window as usual. Check on the watch at any
   time:

   ```console
   $ hh watch --status
   PATH               SESSION PID   MODE       STARTED FILES
   ------------------ ------- ----- ---------- ------- -----
   /home/me/code/site 3fa2c1  48213 background 5m ago  4
   ```

3. When the agent is done, stop the watch. The session is finalized before
   the command returns:

   ```console
   $ hh watch ~/code/site --stop
   ✓ Stopped watching /home/me/code/site · session 3fa2c1 · 12m4s · 6 steps · 4 files changed · hh replay 3fa2c1
   ```

4. Review what happened:

   ```bash
   hh replay 3fa2c1          # step through each change, with diffs
   hh inspect 3fa2c1 --diff  # or print everything as one unified diff
   ```

Without `--daemon`, `hh watch` stays in the foreground and Ctrl-C finishes the
session, printing the same summary `hh run` prints.

## What gets captured

Only file changes: the same watcher, ignore rules (`.git/`, `node_modules/`,
`target/`, the root `.gitignore`, `[record] ignore`), size limit
(`[record] max_file_size`), binary handling and before-side rules as
`hh run` ([Recording](./recording.md)). Record-time redaction
(`[redaction] at_record`) applies too.

There is no wrapped command, so there is **no terminal output and no
transcript** — a watch records what was done, not what the agent said or
which tools it called. A change is recorded whichever process makes it: the
agent, you in your editor, or a build tool. Watch a directory you are not also
editing if you want the record to be the agent's work alone.

In the session data a watch session has:

| Field | Value |
|---|---|
| `command` | `[]` (`hh list` shows `(watch: file changes only)`) |
| `capture_tier` | `effects`, set when the session starts |
| `agent_kind` | `generic` |
| `exit_code` | `null` |
| `status` | `ok` when stopped by Ctrl-C, `--stop`, SIGTERM or the idle timeout |

`hh replay` states the gap in its header: `watched directory — file changes
only, no terminal or transcript`.

## One watcher per directory

A directory has at most one watcher. A second `hh watch` on the same
directory — however the path is spelled — prints the running session and
exits `0` without starting another:

```console
$ hh watch ~/code/site
● Already watching /home/me/code/site · session 3fa2c1 · pid 48213 · `hh watch --stop` to finish it
```

Watching a directory and, separately, a subdirectory of it are two watchers,
and a change in the subdirectory is recorded by both.

## Stopping

| How | Result |
|---|---|
| Ctrl-C (SIGINT) in the foreground | Session finalized as `ok`. |
| `hh watch [path] --stop` | Sends SIGTERM to the watcher for that directory and waits (up to 30 s) until its session is finalized. Running it when nothing is watched prints `No watcher running` and exits `0`. |
| `kill <pid>` (SIGTERM) | Same as `--stop`, without waiting. |
| `--idle-timeout <duration>` | Finishes on its own after that long with no filesystem activity in the directory (`90s`, `15m`, `2h`; a bare number is seconds). Off by default. Any filesystem event counts as activity, ignored paths included. |

A change made just before the stop is still recorded: on shutdown the watcher
drains events still in flight and rescans the directory for anything it
missed.

If a watcher dies without finalizing (killed with SIGKILL, a crash, a reboot),
the next `hh` command of any kind marks its session `interrupted` — the same
check that recovers a crashed `hh run` — and clears its lock, so the directory
can be watched again. Everything recorded up to the crash is kept.

## Background watchers (`--daemon`)

`--daemon` starts the watcher detached from the terminal: in its own process
group, ignoring SIGHUP, with no stdin. It returns once the watcher is
observing. Files it keeps under the data dir:

| File | Contents |
|---|---|
| `<data dir>/watch/<key>.lock` | The watcher's pid, process start time, directory and session (JSON). It is the PID file, and it is also what enforces one watcher per directory. It is removed when the session has been finalized. |
| `<data dir>/watch/<key>.log` | The background watcher's warnings and its final summary line. Appended to by every background watch of that directory. |

`<key>` is derived from the directory's canonical path.

`--daemon` and `--stop` are not available on Windows yet; there, run
`hh watch` in its own terminal and press Ctrl-C to finish.

## Directories hh refuses to watch

`hh watch` refuses your home directory, any directory that contains it, and a
filesystem root (`/`, `C:\`). Watching one would exhaust the operating
system's file-watch limits and record everything you do, not just the agent's
work. Watch the project directory the agent works in instead.

```console
$ hh watch ~
✗ refusing to watch /home/me
  why: it is your home directory — watching it would exhaust the operating system's file-watch limits and record everything you do, not just the agent's work
  hint: watch the project directory the agent works in, e.g. `hh watch ~/code/my-project`
```

On Linux, a large project can still run into the per-user inotify watch
limit; `hh doctor --capture` reports it ([Effect capture](./capture.md)).

## Exit codes

| Code | When |
|---|---|
| `0` | The session was recorded and finalized; or a watcher already holds the directory (it is reported); or `--stop` found nothing to stop; or `--status`. |
| `1` | The watch could not start or finish: the store can't be opened, the directory can't be watched at all, a background watcher failed to start, `--stop` could not signal the watcher or it did not finish in time, or `--daemon`/`--stop` on Windows. |
| `2` | Usage error: an unknown or conflicting flag, a bad `--idle-timeout`, a path that does not exist or is not a directory, or a refused directory (home, an ancestor of home, a filesystem root). |

See [Exit codes](./exit-codes.md).
