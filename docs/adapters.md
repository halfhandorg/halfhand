# Adapters

An **adapter** teaches Halfhand how to parse a specific agent's JSONL stream
into structured events (prompts, tool calls, tool results). When you
`hh run -- <cmd>`, Halfhand auto-detects the adapter from the command; force
one with `--adapter`.

## Recognized adapters

| Adapter         | Detect from                                   | Records                                 |
|-----------------|-----------------------------------------------|-----------------------------------------|
| `claude-code`   | `claude`                                      | Claude Code's JSONL turns               |
| `claude-desktop`| `--adapter claude-desktop` only (see below)   | Claude Desktop Code-tab turns           |
| `codex-cli`     | `codex`                                       | OpenAI Codex CLI turns                  |
| `gemini-cli`    | `gemini`                                      | Gemini CLI turns                        |

Any command without a matching adapter is still recorded — you get the PTY
output and file changes, just not the parsed agent turns.

## Recording a Claude Desktop session

Claude Desktop is a GUI app, so there is no command for `hh run` to wrap and
nothing to auto-detect from argv. Instead, `hh run --adapter claude-desktop`
records *for as long as the wrapped command runs* and, in parallel, finds the
Desktop Code-tab session open in the current directory and tails its
transcript. Wrap a shell and exit it when you are done:

```bash
cd ~/proj
hh run --adapter claude-desktop -- $SHELL   # or: -- sleep 3600
# ... work in Claude Desktop's Code tab, in ~/proj ...
exit                                         # ends the recording
hh inspect last
```

Start the Desktop session (or send it a prompt) *while* the recording is
running: the adapter looks for a session that is new since `hh run` started
or active after it. An idle session that was opened earlier and never touched
again is not picked up.

### How the transcript is found

Desktop's Code tab embeds a Claude Code CLI, so the transcript is the same
JSONL Claude Code writes, in the same place — `hh` reuses the Claude Code
parser unchanged. What differs is discovery:

1. **Desktop's session index (primary).** Desktop writes one
   `local_<uuid>.json` per session under its config directory, carrying the
   session's `cwd` and the `cliSessionId` that names the transcript file.
   The adapter matches the index record for the current directory (skipping
   archived and deleted sessions) and tails
   `~/.claude/projects/<cwd-slug>/<cliSessionId>.jsonl`.
2. **Entrypoint-tagged transcript (fallback).** If the index is not where
   `hh` expects it, a new transcript for the current directory is accepted
   only if its records say `entrypoint: "claude-desktop"`. A terminal
   `claude` session in the same directory (`entrypoint: "cli"`) is rejected
   with a reason pointing at `hh run -- claude`.

| OS      | Session index                                                        | Transcript                                      | Status                     |
|---------|----------------------------------------------------------------------|-------------------------------------------------|----------------------------|
| Linux   | `$XDG_CONFIG_HOME/Claude/claude-code-sessions/` (default `~/.config/Claude/…`) | `~/.claude/projects/<cwd-slug>/<id>.jsonl` | **Confirmed** (fixture captured here) |
| macOS   | `~/Library/Application Support/Claude/claude-code-sessions/`         | `~/.claude/projects/<cwd-slug>/<id>.jsonl`      | Unverified — expected by analogy; please report |
| Windows | `%APPDATA%\Claude\claude-code-sessions\`                             | `%USERPROFILE%\.claude\projects\<cwd-slug>\<id>.jsonl` | Unverified — the slug rule for drive letters has not been checked |

On macOS and Windows the paths are the ones Electron's `userData` convention
predicts, but no fixture has been captured there yet. If discovery fails the
session is recorded with `adapter_status = degraded` and a reason that names
the directories that were searched — it never reports `active` with zero
steps. Run `HH_DEBUG=1 hh run --adapter claude-desktop -- …` for the full
discovery trace and attach it to an issue.

When the recording ends, the adapter reads the transcript to its end before
finalizing, so turns written right up to the moment you exited the shell are
kept. Turns Desktop writes *after* that are not — start a new recording.

## Adding a new adapter

Adapters consume **untrusted** JSONL, so per the 1.0 stability addendum every
adapter parser must:

1. Never panic on malformed input — errors only.
2. Have a fuzz target under `fuzz/` (e.g. `fuzz/fuzz_targets/<name>.rs`).
3. Ship with a fixture corpus under `tests/fixtures/<adapter>/` for
   integration tests — a sanitized *real* capture with a `NOTES.md` on its
   provenance and where the agent writes session data per OS (see
   `hh-core/tests/fixtures/claude-desktop/` for the model), never a
   hand-written approximation.

See `CONTRIBUTING.md` ("Adding an adapter") for the fixture-capture guide and
the exact files to touch (`hh-core`'s adapter module, the detection rules, the
fuzz target, and the fixture corpus).