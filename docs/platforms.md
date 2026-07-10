# Platform support

Halfhand supports three platforms: Linux, macOS, and Windows. All three run
the full test suite in CI (`{ubuntu, macos, windows} x {stable, msrv}`) —
Windows is a supported platform, not a build-only target.

This page documents platform-specific behavior and known limitations. Where a
limitation is a real gap rather than an inherent platform difference, it links
a tracking issue instead of being papered over.

## PTY backend

`hh run` proxies the wrapped agent through a real PTY (`portable-pty`,
FR-1.1):

- Linux: the `pty` backend (`openpty(3)`).
- macOS: the same Unix `pty` backend.
- Windows: the ConPTY backend (`CreatePseudoConsole`), Microsoft's native
  pseudo-console API (Windows 10 1809+). CI exercises it directly against
  `.ps1` fixture agents (`tests/fixtures/fake_agent.ps1`,
  `fixture_agent.ps1`, `interactive.ps1`) so terminal capture, raw-mode
  stdin/stdout proxying, and file-change recording are verified against the
  real backend on `windows-latest`, not skipped.

### Known limitation: no resize forwarding on Windows

`hh run` forwards terminal resizes to the child by registering a `SIGWINCH`
handler (`hh-record/src/runner.rs`). `SIGWINCH` does not exist on Windows —
console resizes are not delivered as a signal there. Today this means a
Windows user resizing their terminal mid-recording does not resize the
child's PTY; the child keeps the window size it was spawned with.

This is a real gap, not a fundamental platform limitation: Windows consoles
do support querying the current buffer size, so a polling-based resize check
(e.g. on the same cadence as the existing signal-flag poll loop) could close
it. Not implemented in this change — tracking issue: *to be filed* (see the
PR that introduced this doc for status).

## Data directory

Config/data paths are resolved via the `directories` crate
(`hh-core/src/config.rs`), not a hardcoded `~/.something`:

- Linux: `$XDG_DATA_HOME/halfhand` (data), `$XDG_CONFIG_HOME/halfhand` (config) —
  falling back to `~/.local/share/halfhand` / `~/.config/halfhand`.
- macOS: `~/Library/Application Support/halfhand`.
- Windows: `{FOLDERID_RoamingAppData}\halfhand\data` (data) and
  `{FOLDERID_RoamingAppData}\halfhand\config` (config) — typically
  `%APPDATA%\halfhand\...`.

## File permissions vs. Windows ACLs

The data dir, blobs dir, and blob files are created `0700`/`0600` on Unix
(`hh-core/src/blob.rs`, `hh-core/src/store.rs`), restricting them to the
owning user. These calls are `#[cfg(unix)]`-gated and are simply absent on
Windows — there is no equivalent `chmod`-style call in this codebase for
Windows today.

This is not a silent gap: Windows has no POSIX permission bits to set in the
first place. A newly created directory under `%APPDATA%\<user>\...` inherits
NTFS ACLs from its parent, which for a per-user roaming profile directory
already restrict access to the owning user and Administrators by default on
a standard single-user Windows install. Locking this down further (e.g. an
explicit ACE removing inherited access for other accounts on a shared
machine) would need the `windows-acl` crate or raw `SetNamedSecurityInfo`
calls and is not implemented here — if you run Halfhand on a shared Windows
machine where other local accounts should not read your recordings, treat
the data dir the same as any other sensitive per-user folder on that machine.

## ANSI / color output

`hh`'s plain-text color output (`hh/src/render.rs`, gated on `NO_COLOR` and
`is_terminal()`) writes raw ANSI escape codes rather than going through a
styling library's command layer. Legacy Windows consoles do not interpret
those bytes unless `ENABLE_VIRTUAL_TERMINAL_PROCESSING` is turned on for the
console first. `render::use_color()`/`use_color_stderr()` call
`crossterm::ansi_support::supports_ansi()` on Windows to enable it (a no-op,
always-true check on Unix).

The interactive replay TUI (`hh replay`, built on `ratatui` +
`CrosstermBackend`) does not need this: crossterm's own `execute!`/`queue!`
command dispatch already probes and enables VT processing internally for
every command it issues.

## Claude Code adapter paths

The adapter tails `~/.claude/projects/<slug>/*.jsonl`
(`hh-core/src/adapter.rs`). `claude_projects_dir()` resolves `$HOME` or
falls back to `%USERPROFILE%` on Windows. The `<slug>` is the session cwd
with `/`, `\`, `.`, and `:` all mapped to `-`.

The `:` mapping matters specifically for Windows: a drive letter (`C:\Users\me`)
would otherwise leave a literal colon in the slug, and a bare `:` inside an
NTFS path component is alternate-data-stream syntax, not a plain character —
`projects.join(slug)` would target a stream on a same-named file, not a
directory. Since Claude Code itself has to create this directory on disk, its
real algorithm cannot emit a bare colon either, so stripping it here is the
only self-consistent choice. Covered by `slugify_windows_style_paths` in
`hh-core/src/adapter.rs` (drive letters, UNC paths, nested worktree-style
paths, paths with spaces — all typed `Path`/`PathBuf` fixtures, not string
munging).

## Recorded file-change paths

`file_changes.path` (and the mirrored `body_json.path` on `file_change`
events) is always rendered with `/` separators regardless of host platform
(`hh-record/src/watcher.rs::to_forward_slash_string`), so diff headers in
`hh inspect --diff` and replay read the same git-style `a/`/`b/` convention
on every platform. This goes through `Path::components()`, not a blind
`\`→`/` string replace, so a literal `\` byte inside a Unix filename
component is left untouched.

### Known limitation: case-sensitivity in the FS watcher

The watcher's `known`/`existing` maps key file identity by `PathBuf` byte
comparison (`hh-record/src/watcher.rs`). On a case-insensitive filesystem —
the Windows and macOS default — a case-only rename (`Foo.txt` → `foo.txt`)
is indistinguishable from two different paths, and would be recorded as a
spurious create+delete pair instead of a single change. Linux's typically
case-sensitive filesystems are unaffected.

Fixing this generally requires knowing the actual case-sensitivity of the
watched filesystem (queryable, but not reliably knowable from `cfg` alone —
NTFS can enable per-directory case sensitivity, and not every Linux
filesystem is case-sensitive either), which is a larger change than this
pass. Tracking issue: *to be filed*.

## MSRV

The workspace pins `rust-version = "1.75"` (`Cargo.toml`). CI's `test` job
matrix includes an `msrv` leg on all three platforms, resolved dynamically
from that field (not hardcoded a second time in the workflow) so the two can
never silently drift apart.

## Tests intentionally platform-gated

- `sigkill_of_hh_mid_run_leaves_interrupted_session` is `#[cfg(unix)]`: it
  asserts behavior after a real `SIGKILL`, which has no Windows equivalent
  (Windows process termination is `TerminateProcess`, a different
  mechanism/semantics). Not covered by an analogous Windows test in this
  change.
- The Claude Code adapter's `claude` shim in `hh/tests/cli.rs`
  (`write_claude_shim`, `record_claude_shim_session`, `claude_adapter_e2e`)
  is `#[cfg(unix)]`: it relies on a `#!/usr/bin/env python3` shebang shim
  executed directly off `PATH`, which Windows cannot invoke the same way.
  The adapter's actual path/slug logic it exercises is instead covered
  directly and portably by `hh-core/src/adapter.rs`'s unit tests (see
  above) rather than through this same end-to-end harness.
