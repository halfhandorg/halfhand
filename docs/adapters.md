# Adapters

An **adapter** teaches Halfhand how to parse a specific agent's JSONL stream
into structured events (prompts, tool calls, tool results). When you
`hh run -- <cmd>`, Halfhand auto-detects the adapter from the command; force
one with `--adapter`.

## Recognized adapters

| Adapter         | Detect from        | Records                                 |
|-----------------|--------------------|-----------------------------------------|
| `claude-code`   | `claude`           | Claude Code's JSONL turns               |
| `claude-desktop`| Claude Desktop     | Desktop session turns                   |
| `codex-cli`     | `codex`            | OpenAI Codex CLI turns                  |
| `gemini-cli`    | `gemini`           | Gemini CLI turns                        |

Any command without a matching adapter is still recorded — you get the PTY
output and file changes, just not the parsed agent turns.

## Adding a new adapter

Adapters consume **untrusted** JSONL, so per the 1.0 stability addendum every
adapter parser must:

1. Never panic on malformed input — errors only.
2. Have a fuzz target under `fuzz/` (e.g. `fuzz/fuzz_targets/<name>.rs`).
3. Ship with a fixture corpus under `tests/fixtures/<adapter>/` for
   integration tests.

See `CONTRIBUTING.md` ("Adding an adapter") for the fixture-capture guide and
the exact files to touch (`hh-core`'s adapter module, the detection rules, the
fuzz target, and the fixture corpus).