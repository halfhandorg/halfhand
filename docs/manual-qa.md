# Manual QA — v0.1.0-beta.1 release

> **SRS reconstruction note.** `CLAUDE.md` references
> `halfhand-srs-v0.1.0-beta.1.md`, but that file is **not in the repo or git
> history** (verified). The acceptance-criteria mapping below is reconstructed
> from the in-code SRS citations (`grep -r "SRS"` — FR/NFR/DR tags in
> `hh-core`, `hh-record`, `hh/tests/cli.rs`) and from the release task's own
> descriptions of AC-1 and AC-6. Treat the AC *numbering* as inferred, not
> verbatim. Each row points at the concrete test or manual step that proves
> the behaviour, so the mapping is checkable regardless of the SRS text.

This document covers the two acceptance criteria that cannot run in CI
(AC-1: a real Claude Code run, and AC-6: the fresh-machine experience) and
gives an at-a-glance matrix of how **every** §7 acceptance criterion is
discharged — by an automated test, by this manual checklist, or both.

## §7 acceptance-criteria matrix

| AC | Proves | Discharged by |
|----|--------|---------------|
| AC-1 | A real `claude` session records structured events, then `hh replay` / `hh inspect` read them back faithfully. | Manual checklist below. Automatable parts (adapter parsing, `hh run -- claude` end-to-end, degraded mode, read-side on a structured session) are covered by tests — see "Automatable parts of AC-1". |
| AC-2 | A generic agent run is captured end-to-end: ANSI terminal output, file create/modify/delete with correct hashes + blob round-trips, ignored paths skipped, exit code + status propagated. | `run_records_fake_agent_with_full_capture`, `run_provides_a_real_pty_to_the_child`, `run_records_terminal_chunks_reassemble` (FR-1) — `hh/tests/cli.rs`. |
| AC-3 | `hh mcp-proxy` records correlated JSON-RPC traffic, spills ≥256 KiB payloads to the blob store, attaches to a parent session via `HH_SESSION_ID`, and records round-trip `latency_ms`. | `mcp_proxy_*` tests (FR-2) — `hh/tests/cli.rs`. |
| AC-4 | Crash-safety: SIGKILL of `hh` mid-recording leaves a readable session that the next invocation reconciles to `interrupted`. | `sigkill_of_hh_mid_run_leaves_interrupted_session` (FR-1.7) — `hh/tests/cli.rs`. |
| AC-5 | `hh inspect --json` / `hh list --json` emit output `jq` can parse and that conforms to `docs/json.md` (schema:1, documented fields). | `inspect_json_is_valid_against_jq`, `list_shows_aligned_table_and_json` (FR-4 / FR-5) — `hh/tests/cli.rs`. |
| AC-6 | A fresh machine can install `hh` and complete the 30-second quickstart. | Manual checklist below. CI already builds from a clean checkout on Ubuntu, macOS, and Windows (build-only) on every push — the automatable part of "fresh-machine". |

If a row above has no automated test listed, it is in the manual checklists
below. Anything not in CI is run by hand for each release and the result
recorded in the release announcement.

---

## AC-1 manual checklist — real Claude Code run

This cannot run in CI: it requires the real `claude` CLI, an Anthropic API
key, and outbound network from the *wrapped* process (Halfhand itself still
makes zero network calls — NFR-2; only the agent under record does). It also
spends real tokens. Run this by hand before tagging a release.

You need: `claude` on PATH, `ANTHROPIC_API_KEY` exported, a throwaway working
dir, and `HH_DATA_DIR` pointed at a temp dir.

```bash
mkdir -p /tmp/ac1 && cd /tmp/ac1
export HH_DATA_DIR=/tmp/ac1/hh-data
hh run -- claude          # ask it something small, e.g. "list files", let it finish
```

- [ ] **Records.** `hh run` prints the epilogue `✓ Recorded session <short-id> · … · hh replay <short-id>` and exits 0.
- [ ] **Detected as Claude Code.** `hh list --json` shows the new session with `agent_kind: "claude-code"` and `adapter_status: "active"`.
- [ ] **Structured events captured.** `hh inspect <short-id>` shows steps for the user message, tool call(s), and tool result(s) — not just raw terminal output. `hh inspect <short-id> --json` emits one NDJSON object per event, each with `schema: 1`.
- [ ] **Correlation.** The `tool_result` event's `correlates` points at the `tool_call` event that produced it (`hh inspect --json | jq 'select(.kind=="tool_result") | .correlates'`).
- [ ] **Replay is faithful.** `hh replay <short-id>` (in a real terminal) walks the timeline; the assistant's text, tool calls, and results appear in order. It is a **faithful playback** of the captured timeline, *not* a re-execution of the agent — the agent is not re-invoked and no network calls are made during replay.
- [ ] **Delete cleans up.** `hh delete <short-id> --yes` removes the session and the `✓ Deleted` epilogue prints; `hh list` no longer shows it.

### Automatable parts of AC-1

Everything except "point `hh run` at the *real* `claude` with a live API key"
is automated. These tests use a `claude` shim on PATH that writes a
transcript into a temp `$HOME/.claude/projects/<slug>/` and/or committed
transcript fixtures, so they run hermetically in CI:

- **Adapter locates + tails a real transcript and records structured events
  with correlation.** `claude_adapter_e2e` (FR-1.5) — `hh/tests/cli.rs`.
  Installs a `claude` shim, runs `hh run -- claude`, asserts
  `agent_kind=claude-code`, `adapter_status=active`, `model` + `usage_json`
  persisted, and `user_message` / `tool_call` / `tool_result` events with
  `tool_result.correlates` → the `tool_call`.
- **Adapter parses structured transcripts.** The insta snapshot tests in
  `hh-core/src/adapter.rs` synthesize Claude-Code-shaped transcript records
  (user prompts, assistant tool-use + model/usage, tool results, subagent and
  tool-result-file cases) in temp dirs and run the pure parser over them,
  asserting structured output.
- **Degraded mode.** `claude_adapter_degrades_on_missing_projects_dir`
  (FR-1.5) — with no `~/.claude/projects`, the adapter warns once, records
  `adapter_status=degraded`, and the PTY session still finalizes `ok`.
- **Read-side on a structured session.** `inspect_and_list_on_claude_code_session`
  (AC-1) — `hh/tests/cli.rs`. After a shim `claude` run, `hh list --json`
  reports `agent_kind=claude-code` and `hh inspect last --json` emits the
  structured events as NDJSON with `schema:1`.

The only thing left to a human is the live-API-key smoke against the genuine
`claude` binary, which the checklist above covers.

---

## AC-6 manual checklist — fresh-machine experience

The automatable part (build from a clean checkout on each OS) already runs in
`ci.yml` on every push: `cargo build/test --workspace --locked` on
`ubuntu-latest` and `macos-latest`, plus a build-only job on `windows-latest`
(Windows PTY is best-effort per SRS §2.2). The release workflow
(`.github/workflows/release.yml`) additionally builds release binaries for
x86_64/aarch64 on macOS and Linux (musl where feasible) from a clean checkout.

The human part, run once per release on a machine without a Halfhand
checkout:

- [ ] **Install from source.** `cargo install halfhand` (or download the
  release binary + checksum-verify it) puts `hh` on PATH with no prior
  Halfhand state present.
- [ ] **`hh --version` works.** Prints `hh 0.1.0-beta.1 (<git-sha>)`.
- [ ] **`hh --help` lists every subcommand** with a one-line example each
  (`run`, `replay`, `inspect`, `list`, `delete`, `mcp-proxy`).
- [ ] **30-second quickstart.** From the README:
      `hh run -- claude` → `hh replay last` → `hh inspect last` completes
      without reading any docs beyond the README hero/quickstart.
- [ ] **No real data dir touched.** The run uses `HH_DATA_DIR` or the
  platform default; confirm nothing was written under an unexpected path.
- [ ] **NO_COLOR / non-TTY.** `hh list | cat` prints a plain, pipe-safe
  table with no ANSI codes; `NO_COLOR=1 hh list` does the same in a TTY.
- [ ] **Secrets caveat is discoverable.** `hh run --help` documents that
  `--record-input` is off by default because keystrokes may contain secrets
  (NFR-4); `hh --help` long-about notes that recorded sessions can contain
  secrets and points at `hh delete`.

Record the result (pass/fail per line) in the release announcement. A failure
on any line blocks the release.