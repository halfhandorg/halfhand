# Replay & Inspect

## `hh replay` — faithful playback

```bash
hh replay last          # interactive TUI on the most recent session
hh replay a1b2c3        # by id
hh replay last --web    # write a self-contained HTML page and print its path
```

The TUI shows a step timeline alongside a detail pane (message text, tool
calls and results, unified diffs for file changes). `--web` exports the same
content as a single offline HTML file — no terminal, browser, or server
needed; the path is printed to stdout.

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