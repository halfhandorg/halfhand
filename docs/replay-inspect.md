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
```

The `--json` shape is documented and frozen as of 1.0 (additive-only); see
[JSON reference](./json.md) and the [Stability policy](./stability.md).

## `hh list` — every recording

```bash
hh list                 # newest first, default 20
hh list --limit 50
hh list --json
```