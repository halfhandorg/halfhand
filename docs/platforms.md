# Platform support

Halfhand is developed and released for Linux, macOS, and Windows. This page
records what "supported" means per platform, where behavior differs, and what
is known not to work. It amends SRS §2.2, which originally listed Windows as
build-only: as of this change Windows runs the full test suite in CI
(`{ubuntu, macos, windows} × {stable, MSRV}`; the MSRV leg is a build-only
check on exactly the `rust-version` toolchain declared in `Cargo.toml`).

| Platform | CI            | PTY backend            | FS watcher backend       |
| -------- | ------------- | ---------------------- | ------------------------ |
| Linux    | full tests    | Unix PTY (`openpty`)   | inotify                  |
| macOS    | full tests    | Unix PTY (`openpty`)   | FSEvents                 |
| Windows  | full tests    | ConPTY (portable-pty)  | ReadDirectoryChangesW    |

## Where files live

Paths come from the [`directories`](https://crates.io/crates/directories)
crate (SRS §2.3):

| | Linux | macOS | Windows |
| --- | --- | --- | --- |
| data (`hh.db`, blobs) | `$XDG_DATA_HOME/halfhand` (default `~/.local/share/halfhand`) | `~/Library/Application Support/halfhand` | `%APPDATA%\halfhand\data` |
| config | `$XDG_CONFIG_HOME/halfhand/config.toml` | `~/Library/Application Support/halfhand/config.toml` | `%APPDATA%\halfhand\config\config.toml` |

`HH_DATA_DIR` overrides the data directory on every platform.

Relative paths stored in recordings (`file_changes.path`, event summaries)
are always `/`-separated, on every platform, so a recording made on Windows
inspects and replays identically elsewhere (DR-2: the schema is a public
interface). Absolute paths (`sessions.cwd`) are stored as the OS reports
them.

## File permissions (NFR-4)

On Unix, the data directory, blob shard directories, and blob files are
created with `0700`/`0600` and a test asserts it. Those mode bits do not
exist on Windows; the `chmod` calls are compiled only on Unix
(`#[cfg(unix)]`).

**Windows note:** Halfhand relies on the default ACLs of the profile
directory instead. `%APPDATA%` inherits an ACL that grants access to the
owning user (plus `SYSTEM` and `Administrators`) and no other interactive
user, which matches the intent of `0700`. Halfhand does not set explicit
ACLs. If you point `HH_DATA_DIR` somewhere outside your user profile (a
shared folder, a FAT/exFAT volume), nothing restricts access to the
recordings — choose the location accordingly.

## PTY recording on Windows (ConPTY)

`hh run` uses portable-pty's ConPTY backend. CI exercises it end-to-end with
the fixture agent in two variants — Python (`fixture_agent.py`) and
PowerShell (`fixture_agent.ps1`) — covering terminal-output capture,
file-change capture, exit-code propagation, and console-geometry visibility
(`pty_probe.py`).

Known differences and limitations, stated plainly:

- **Recorded bytes are ConPTY's rendering, not the child's raw output.**
  ConPTY interprets the child's console API calls and re-emits VT sequences.
  Text content survives, but the exact escape-sequence stream can differ
  from what the same program would emit on Unix, and may include cursor
  positioning the child never wrote. Replay output is faithful to what the
  terminal showed, not byte-identical to the child's writes.
- **Window resizes are not forwarded to the child.** Resize forwarding is
  driven by `SIGWINCH`, which does not exist on Windows; the child keeps its
  spawn-time geometry. Tracked in
  [#11](https://github.com/halfhandorg/halfhand/issues/11).
- **Interactive stdin round-trips are verified manually, not in CI.** The
  stdin proxy compiles and runs on Windows, but the Unix echo test has no
  ConPTY counterpart yet (console input semantics make an un-flaky test
  nontrivial). Tracked in
  [#12](https://github.com/halfhandorg/halfhand/issues/12).
- **ANSI in hh's own output** (epilogue, `hh list`, errors) requires virtual
  terminal processing. hh enables it via crossterm at startup; on consoles
  where that fails (very old conhost), hh falls back to plain, uncolored
  output rather than printing escape garbage.
- **Ctrl-C/signal semantics differ.** On Unix, `SIGTERM`/`SIGINT` to hh mark
  the session `interrupted`. On Windows, signal-hook maps these to console
  ctrl handlers; a hard kill (`taskkill /F`) behaves like the SIGKILL case —
  the session is reconciled to `interrupted` on the next `hh` invocation
  (FR-1.7), which is covered by tests on all platforms.

## Claude Code adapter on Windows

The adapter locates transcripts under `%USERPROFILE%\.claude\projects`
(`HOME` is honored first if set, which is what the tests use). The project
slug is computed from the session cwd with `/`, `\`, `.`, and `:` each
mapped to `-`, so `C:\Users\me\proj` becomes `C--Users-me-proj`. The slug is
inferred from observed transcripts, not a documented Claude Code contract —
on any mismatch the adapter falls back to scanning all project directories
and matching the first record's `cwd` field, so a wrong slug degrades to a
slower lookup, not a miss. Unit tests cover Windows-style cwds with typed
`PathBuf` fixtures; the end-to-end shim test is Unix-only (it relies on a
shebang shim on `PATH`), while the degraded-mode path (no projects
directory) is tested on every platform.

## MSRV

`rust-version = "1.75"` in the workspace `Cargo.toml` is the policy line.
CI builds the workspace with `--locked` on exactly that toolchain for all
three OSes, so both the code and the committed `Cargo.lock` must stay
1.75-compatible. Some dependencies are deliberately held back for this (see
the `lru` pin in `hh/Cargo.toml`).
