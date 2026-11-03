# Exit codes

`hh`'s exit codes are part of its compatibility contract
([Stability policy](./stability.md), section (a)): an existing code's meaning
for an existing command never changes, and new codes are only ever added. A
script or CI job can rely on everything on this page.

This page describes what `hh` **actually returns** today, checked against the
code and pinned by tests in `hh/tests/cli_conformance.rs`. Where that differs
from what you might expect from the one-line summary
(`0` ok · `1` generic error · `2` usage error · `3` session not found ·
`4` redaction/policy block), the difference is listed under
[Known gaps](#known-gaps). Those gaps are documented behavior; changing any
of them would itself be a breaking change.

## Summary

| Code | Meaning | Returned by |
|---|---|---|
| `0` | Success | Every command, when it succeeds. `hh run` / `hh mcp-proxy`: see [pass-through](#hh-run-and-hh-mcp-proxy-pass-the-wrapped-commands-code-through). |
| `1` | Generic error | Any command, for any failure not listed below. |
| `2` | Usage error | Any command, when the command line itself is invalid (clap). |
| `3` | Session not found | `inspect`, `replay`, `export`, `delete`, `rename`, `scan <session>`, `redact` |
| `4` | Redaction/policy block | `hh scan` when it finds secrets |
| other | The wrapped command's own code | `hh run`, `hh mcp-proxy` |

## `0` — success

The command did what was asked. Some commands give `0` a narrower meaning:

| Command | `0` means |
|---|---|
| `hh scan` | No secrets found (clean). |
| `hh doctor` | Every check passed. |
| `hh init` | Setup succeeded, including "already set up; nothing changed". |
| `hh run`, `hh mcp-proxy` | The **wrapped** command exited `0` (see below). |
| `hh --help`, `hh <sub> --help`, `hh help`, `hh --version` | Help or version printed. |

## `1` — generic error

Anything that went wrong and isn't one of the codes below. Every such error
prints an actionable `✗ <what failed>` / `why:` / `hint:` block to stderr.
Examples, all observed:

- The data directory or database can't be opened (any command that opens the
  store).
- `hh import`: the file can't be read, or the bundle fails validation.
  There is deliberately no separate "corrupt bundle" code
  ([Export & import](./export-import.md#exit-codes)).
- `hh search`: an invalid FTS5 query (for example an unterminated `"`).
- `hh doctor`: at least one check failed ([Doctor](./doctor.md#exit-codes)).
- `hh init`: a directory, the database or the config file could not be
  created, read, renamed or backed up; the existing config is malformed; or
  `--force` while a legacy `halfhand.toml` is the file in use
  ([Setup](./init.md#exit-codes)).
- `hh gc`, `hh stats`: a prune, vacuum or read error.
- A refused irreversible action in a non-interactive shell:
  `hh delete` or `hh redact` without `--yes`, and `hh export --no-redact`
  (which always requires an interactive confirmation).
- `hh replay` without `--web` when stdout is not a terminal.
- `hh inspect --json --diff` (the two output formats are mutually exclusive).
- `hh run --adapter <name>` with an unknown adapter name.
- `hh run -- <command>` when the command can't be started at all (for
  example it does not exist).
- `hh mcp-proxy` with `HH_SESSION_ID` set to a session that does not exist.

## `2` — usage error

The command line could not be parsed, so nothing ran. This comes from the
argument parser (clap), before any subcommand logic:

- an unknown flag (`hh list --bogus`) or subcommand (`hh frobnicate`);
- a flag value of the wrong type (`hh list --limit many`);
- a missing required argument (`hh delete` with no session);
- flags that conflict (`hh export --html --bundle`,
  `hh scan <session> --all`, `hh inspect --context --step 1`);
- an invalid session name: empty or blank, longer than 100 characters, or
  containing control characters (`hh rename last ""`, `hh run --name "" -- …`),
  or `hh rename` with neither a name nor `--clear`;
- `hh` with no subcommand at all (help is printed to stderr).

## `3` — session not found

The session argument (a short id, full id, or `last`) didn't resolve to a
recorded session. `last` on an empty store counts. Returned by every command
that takes a session argument:

| Command | Notes |
|---|---|
| `hh inspect [<session>]` | default `last` |
| `hh replay [<session>]` | `--web`, or in a terminal; without a terminal the TTY check fails first (`1`) |
| `hh export [<session>]` | default `last` |
| `hh delete <session>` | |
| `hh rename <session>` | |
| `hh scan [<session>]` | default `last`; `--all` on an empty store is clean (`0`) |
| `hh redact <session>` | |

## `4` — redaction or policy block

`hh scan` found at least one secret, in the session it was given or, with
`--all`, in any session. `--json` keeps the same code. This is the only place
`4` is used today, so a CI job can gate on `hh scan --all`
([Redaction](./redaction.md#exit-codes)).

## `hh run` and `hh mcp-proxy` pass the wrapped command's code through

`hh run -- make` behaves like `make`: once recording has started, `hh run`
exits with **the wrapped command's own exit code**, whatever it is. The same
holds for `hh mcp-proxy` and the MCP server it wraps.

- `hh run -- false` exits `1`; `hh run -- sh -c 'exit 3'` exits `3`;
  `hh run -- sh -c 'exit 4'` exits `4`. For these two commands, `2`, `3` and
  `4` mean whatever the wrapped command meant by them, **not** usage error,
  session not found or policy block.
- A wrapped command killed by a signal, or whose code can't be determined,
  makes `hh` exit `1`.
- Errors in `hh` itself before the command starts (an unknown `--adapter`,
  a command that can't be spawned, a store that can't be opened) are `1`, and
  bad flags are `2`.

The recorded code is also stored: `hh list --json` / `hh inspect --json`
report it as `exit_code` ([JSON reference](./json.md)).

## Known gaps

Places where today's behavior differs from the one-line contract
`0 ok · 1 generic · 2 usage · 3 session not found · 4 policy block`. They are
documented, tested and stable; none of them is changed here, because
changing an exit code is a breaking change under the stability policy.

1. **Pass-through.** `hh run` and `hh mcp-proxy` return the wrapped
   command's code, so for them `2`/`3`/`4` do not carry their contract
   meaning (see above).
2. **Some "session not found" cases exit `1`.**
   - `hh mcp-proxy` with an unknown `HH_SESSION_ID`.
   - `hh replay <session>` without a terminal: the terminal check runs
     before the session lookup.
3. **Some usage-style errors exit `1`, not `2`.** These are checked by `hh`
   after parsing, not by the argument parser:
   `hh inspect --json --diff`, `hh run --adapter <unknown>`, and `hh replay`
   without a terminal.
4. **Policy refusals other than `scan` exit `1`, not `4`.**
   `hh export --no-redact` without an interactive confirmation, and
   `hh delete` / `hh redact` without `--yes` in a non-interactive shell.
   `4` means exactly "`hh scan` found secrets".
