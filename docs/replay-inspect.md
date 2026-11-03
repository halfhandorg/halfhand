# Replay & Inspect

## `hh replay` — faithful playback

```bash
hh replay last          # interactive TUI on the most recent session
hh replay a1b2c3        # by id
hh replay last --web    # write a self-contained HTML page and print its path
hh replay last --web --open   # …and open it in your default browser
hh replay last --web -o -     # write the HTML to stdout instead
```

The TUI shows a step timeline alongside a detail pane (message text, tool
calls and results, unified diffs for file changes). `--web` exports the same
content as a single offline HTML file — including the session's terminal
output, in a collapsed section under the header (colors kept, other escape
sequences stripped, redacted, capped at 2 MiB by default with a visible
truncation marker). No terminal or server is needed. The file is written to
`<data_dir>/exports/hh-session-<id>.html` (or `-o FILE`) and its path is
printed on stdout. `--open` launches your browser only when stdout is a
terminal and `CI` is unset; if no opener exists (headless, SSH) the path is
still printed and the command still succeeds. See
[`docs/export-import.md`](export-import.md#the-html-replay-page---html-hh-replay---web)
for the safety model, size cap, and destinations.

### The replay TUI

```text
 a1b2c3  auth refactor  ✓ ok
 model  Claude Opus 4.8   provider  Anthropic   duration 1m15s                      steps 10
 tokens 51,540            est. cost $0.05       capture  transcript · claude-code   files 4
────────────────────────────────────────────────────────────────────────────────────────────────────
┌ Timeline · 2/10 ──────────────────────────┐┌ Step 2 · TOOL · 4.0s ───────────────────────────────┐
│00:00   USER    1.0s  add token auth to log││✗ failed · latency 3.2s                              │
│00:01 ✗ TOOL    4.0s  tool_call: Bash      ││                                                     │
│00:05   AGENT   1.0s  I'll add a token para││TOOL — tool_call: Bash                               │
│00:06 ● FILE   1m00s  modified src/auth.rs ││{                                                    │
│01:06 ● FILE    1.0s  created README.md    ││  "input": {                                         │
│01:07   FILE    1.0s  renamed src/mod.rs   ││    "command": "cargo test"                          │
│01:08   FILE    1.0s  modified Cargo.toml  ││  },                                                 │
│01:09   FILE    1.0s  modified src/auth.rs ││  "name": "Bash"                                     │
│01:10 ✗ ERR     2.0s  error: rate limited  ││}                                                    │
└───────────────────────────────────────────┘└─────────────────────────────────────────────────────┘
 ↵ open  ·  n next moment  ·  f files                                                        ? help
```

**The header** is always visible: session name, model, provider, wall-clock
duration, step count, token usage, estimated list-price cost, capture tier
and adapter, and files changed. A value Halfhand does not know prints as `—`,
never as `0` (see [the dash convention](./context.md#the-dash-convention)).

**Capture gaps are stated, not hidden.** When a session has no structured
transcript events (PTY and file-system capture only), the header says
`⚠ no transcript — terminal and file capture only`; a
[`hh watch`](./watch.md) session says
`⚠ watched directory — file changes only, no terminal or transcript`. A degraded adapter
(`⚠ transcript adapter degraded (<reason>)`) and events the recorder could not
write (`⚠ 12 events not recorded`) are shown the same way.

**The timeline** shows each step's offset from the session start, its kind,
and how long it took — until the next step starts, or for the last step,
until the last agent event or the session end. A duration Halfhand cannot
know is `—`. The slowest steps (the top tenth, and never under one second)
are drawn in the accent colour, so a slow step stands out. The mark column
shows `✗` for an error or a failed tool call and `●` for the slowest step
and the largest diff.

**Diffs.** Selecting a file change shows a unified diff with old and new line
numbers, the path, and a summary such as `modified · +2 −2 · before from the
git index`. The summary names where the before-side came from (earlier in the
session, the git index, or the session-start snapshot). If the before-side
was never captured, the view says `before content not captured` and shows the
file as it was after the change, without `+` marks: marking every line as
added would claim the agent wrote all of it. A rename is titled
`old/path → new/path`. Binary files say so instead of diffing. Press
`Enter` to read a diff full width.

**Files touched** (`f`) lists every file the session changed with its edit
count, and notes files that were created, deleted, renamed or binary. The
detail pane previews the selected file's latest change. `Enter` narrows the
timeline to that file's changes, so `d` steps through its diffs; `Esc`
returns to the list.

**Moments** (`n` / `N`) cycle through what is worth seeing in a long
session: every error, every failed tool call, the slowest step, and the
largest diff (the file change with the most added and removed lines). The
status line says which moment you are on (`moment 2/4 · slowest step ·
1m00s · modified src/auth.rs`). If a filter hides a moment, the filter is cleared to show it.

The status line shows the three keys that matter in the current view; `?`
lists them all.

#### Keybindings

| Key | Action |
|---|---|
| `j / ↓` | Next step. With the detail pane focused or full width, scroll down. |
| `k / ↑` | Previous step, or scroll up. |
| `g / Home` | First step (or top of the detail pane). |
| `G / End` | Last step. |
| `PgUp / PgDn` | Page the list or the detail pane. |
| `Tab` | Switch focus between the list and the detail pane. |
| `n / N` | Next / previous moment: errors, failed tool calls, the slowest step, the largest diff. |
| `d / D` | Next / previous file change. |
| `/` | Filter steps by kind (`tool`, `file`, `err`, …) or summary text. `Enter` keeps the filter, `Esc` clears it. |
| `J` | Jump to a time offset (`mm:ss` or `hh:mm:ss`). |
| `Enter` | Open the selected step full width; in files touched, open the file's changes. Press again to return. |
| `f` | Files touched. |
| `t` | Show or hide terminal output between steps. |
| `Esc` | Back: leave full width, leave a file's changes, clear the filter, or cancel a prompt. |
| `?` | Show every keybinding. Any key closes it. |
| `q` | Quit. |

#### Colour and themes

The TUI uses one accent colour (cyan on dark terminals, blue on light ones)
for focus, titles, slow steps and moment marks. The only other colours carry
meaning: red for errors and removed lines, green for added lines. Set the
palette with `[replay] theme = "dark" | "light" | "auto"` in your config;
`auto` follows the `COLORFGBG` hint your terminal may export and otherwise
uses dark. With `NO_COLOR` set, the TUI uses no colour at all, only bold,
dim, reverse video and strikethrough for removed lines.

`hh replay` needs an interactive terminal. With stdout piped or redirected it
exits with an error that points you to `hh inspect` (or use `hh replay --web`).
Large sessions stay responsive: the timeline keeps only a one-line index per
event in memory, and a frame loads just the selected step's content.

### What "faithful" means (and where it ends)

Faithful playback is a **transcript**, not a re-execution. `hh replay` shows
what the agent did and said, in order, with the exact tool inputs/outputs and
file diffs it produced. It does **not** re-run the agent, re-issue tool calls,
or reproduce side effects on your filesystem. Anything the agent did outside
the recorded stream (e.g. a tool with effects Halfhand can't capture) won't be
reproduced — it will only appear as the recorded tool result.

## `hh inspect` — non-interactive detail

```bash
hh inspect last                  # summary + step table
hh inspect a1b2c3 --step 7       # full detail of step 7
hh inspect last --json | jq      # stable JSON (single object with --step)
hh inspect last --diff           # concatenated unified diff of file changes
hh inspect last --failed         # jump to the first error/failed step
hh inspect last --context        # model, provider, tokens, durations, cost
```

The summary header shows the session's name (if any) and, when the transcript
reported one, every model the session used with its step range. The step
table's ELAPSED column is the time from each step to the next (the last step
runs to the last agent event). `--context` prints the session context panel —
see [Session context](./context.md).

Name a session when you record it, or later:

```bash
hh run --name "auth refactor" -- claude
hh rename last "auth refactor, take 2"
hh rename a1b2c3 --clear
```

The `--json` shape is documented and frozen as of 1.0 (additive-only); see
[JSON reference](./json.md) and the [Stability policy](./stability.md).

## `hh list` — every recording

```bash
hh list                 # newest first, default 20
hh list --limit 50
hh list --json
```