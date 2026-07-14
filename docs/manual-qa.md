# Manual QA — 1.0.0 release

> **Print this page.** Run every checkbox by hand on each OS before tagging
> `v1.0.0`. The tag does not go out until every box on this page is ticked and
> the result is recorded in the release announcement. The automated half of
> this gate is [`docs/1.0-definition-of-done.md`](./1.0-definition-of-done.md);
> this page is the human half — the things CI cannot check because they need a
> real agent, a live API key, a real PTY, or a human eye on rendered output.

**How to use:** three columns below — **Linux**, **macOS**, **Windows**. Tick
each OS you actually ran on. Windows is build-only / best-effort per SRS §2.2
(the PTY layer is Unix-first), so Windows rows are marked *(build-only)* where
the runtime path is not claimed; still run them and record what happened.

Record the `hh --version` string and the OS for each run at the top:

```
hh version: ____________________   date: ____________
Linux [ ]   macOS [ ]   Windows [ ]
```

---

## 0. Fresh-machine install

On a machine with no prior Halfhand state, for each OS:

- [ ] **Linux** — **macOS** — **Windows** *(build-only)*: `cargo install halfhand`
  (or: download the release archive for this OS, verify its SHA-256, and put
  `hh` on PATH). No prior `~/.halfhand` / `HH_DATA_DIR` present.
- [ ] **Linux** — **macOS** — **Windows**: `hh --version` prints
  `hh 1.0.0 (<git-sha>)` (no `(dirty)`).
- [ ] **Linux** — **macOS** — **Windows**: `hh --help` lists every subcommand
  (`run`, `replay`, `inspect`, `list`, `delete`, `mcp-proxy`, `doctor`, `gc`,
  `stats`, `scan`, `redact`, `export`, `import`, `search`, `completions`),
  each with a one-line example.
- [ ] **Linux** — **macOS** — **Windows**: `hh completions <shell>` emits a
  script to stdout and exits 0 (run for the shell you actually use).
- [ ] **Linux** — **macOS** — **Windows**: 30-second quickstart from the README
  completes without reading any docs beyond the hero/quickstart.
- [ ] **Linux** — **macOS**: no real data dir touched unexpectedly — the run
  stays under `HH_DATA_DIR` or the platform default.
- [ ] **Linux** — **macOS**: `hh list | cat` is plain, pipe-safe, no ANSI;
  `NO_COLOR=1 hh list` in a TTY is the same.

---

## 1. Real Claude Code run (AC-1)

Needs the real `claude` CLI on PATH, `ANTHROPIC_API_KEY` exported, a throwaway
work dir, and `HH_DATA_DIR` at a temp path. Halfhand itself makes zero network
calls (NFR-2); only the wrapped agent does. This spends real tokens.

```bash
mkdir -p /tmp/hh-qa && cd /tmp/hh-qa
export HH_DATA_DIR=/tmp/hh-qa/data
hh run -- claude        # ask something small, e.g. "list files here", let it finish
```

- [ ] **Linux** — **macOS**: **Records.** `hh run` prints
  `✓ Recorded session <short-id> · … · hh replay <short-id>` and exits 0.
- [ ] **Linux** — **macOS**: **Detected as Claude Code.** `hh list --json`
  shows `agent_kind: "claude-code"`, `adapter_status: "active"`.
- [ ] **Linux** — **macOS**: **Structured events.** `hh inspect <id>` shows
  steps for the user message, tool call(s), tool result(s) — not only terminal
  output. `hh inspect <id> --json` is NDJSON, one object per event, `schema: 2`.
- [ ] **Linux** — **macOS**: **Correlation.** `tool_result.correlates` points
  at the producing `tool_call`
  (`hh inspect <id> --json | jq 'select(.kind=="tool_result") | .correlates'`).
- [ ] **Linux** — **macOS**: **Faithful replay.** `hh replay <id>` in a real
  terminal walks the timeline; assistant text, tool calls, results appear in
  order. This is **faithful playback** of the captured timeline, not a
  re-execution — the agent is not re-invoked, no network during replay.
- [ ] **Linux** — **macOS**: **Delete.** `hh delete <id> --yes` prints
  `✓ Deleted`; `hh list` no longer shows it.

---

## 2. Codex CLI run (if available)

Only if `codex` is installed and you have access. If Codex is not available on
any OS, write **N/A** for that OS and move on — do not block the release on an
adapter you cannot exercise.

```bash
cd /tmp/hh-qa && export HH_DATA_DIR=/tmp/hh-qa/data
hh run -- codex          # small prompt, let it finish
```

- [ ] **Linux** — **macOS**: **Records.** `✓ Recorded session <id>`, exit 0.
- [ ] **Linux** — **macOS**: **Detected as Codex.** `hh list --json` shows
  `agent_kind: "codex-cli"`.
- [ ] **Linux** — **macOS**: **Structured events.** `hh inspect <id> --json`
  emits NDJSON with `schema: 2`; tool calls/results correlate.
- [ ] **Linux** — **macOS**: **Replay + inspect** read back correctly.

If Codex was unavailable on every OS: record **Codex: N/A this release** and
note it in the release announcement (the adapter + its fuzz target still ship;
this is a coverage caveat, not a blocker).

---

## 3. MCP proxy (AC-3)

Needs an MCP server you can spawn. The stdio proxy records correlated
JSON-RPC, spills large payloads to the blob store, and round-trips latency.

```bash
cd /tmp/hh-qa && export HH_DATA_DIR=/tmp/hh-qa/data
hh mcp-proxy -- uvx <some-mcp-server>      # or: -- node server.js
# from another terminal, send it an initialize + a tool call, then exit
```

- [ ] **Linux** — **macOS**: **Records.** A session appears in `hh list` with
  `agent_kind` reflecting the mcp-only session.
- [ ] **Linux** — **macOS**: **Correlated traffic.** `hh inspect <id> --json`
  shows `mcp_request` / `mcp_response` pairs with `correlates` linking them.
- [ ] **Linux** — **macOS**: **Large payload spill.** A request/response ≥256
  KiB is stored as a blob (body references the blob, not inline plaintext).
- [ ] **Linux** — **macOS**: **Latency.** Round-trip `latency_ms` is present
  on response events.

---

## 4. Redact → export → import → HTML, on every agent

The full sharing pipeline, run end-to-end on a session that **contains a
planted secret** (so redaction is provably doing work). Do this for at least
one Claude Code session and one Codex session (if Codex ran above).

```bash
# plant a secret in the work dir before/after the run so it lands in a captured
# file change or prompt, e.g. echo "AKIA0123456789EXAMPLE" > creds.txt
SID=$(hh list --json | jq -r '.[0].short_id')
hh scan $SID                  # expect findings, exit 4
hh redact $SID --yes          # rewrites events + blobs in place
hh scan $SID                  # now clean, exit 0
hh export $SID --bundle -o /tmp/hh-qa/sess.hh
hh export $SID --html  -o /tmp/hh-qa/sess.html
hh export $SID        -o /tmp/hh-qa/sess.json   # redacted by default
HH_DATA_DIR=/tmp/hh-qa/imported hh import /tmp/hh-qa/sess.hh
```

- [ ] **Linux** — **macOS**: **Scan finds the secret** and exits **4**
  (STABILITY.md (a) exit-code contract). The secret value is **not** printed —
  only type/step/location/correlation tag.
- [ ] **Linux** — **macOS**: **Redact rewrites in place.** `hh redact --yes`
  replaces the secret with `{{REDACTED:<type>:<hash8>}}`; originals are securely
  deleted and the DB compacted.
- [ ] **Linux** — **macOS**: **Post-redact scan is clean** and exits **0**.
- [ ] **Linux** — **macOS**: **Export is redacted by default** — open
  `sess.json` and `sess.html` and confirm the secret is gone and the
  `{{REDACTED:…}}` token is present. (Exporting unredacted requires
  `--no-redact` + an interactive TTY confirmation; a script can never do it.)
- [ ] **Linux** — **macOS**: **Bundle import round-trips.** `hh import sess.hh`
  into a fresh `HH_DATA_DIR` succeeds; the imported session's `imported_from`
  preserves the original id; `hh inspect <new-id> --json` matches the original
  (minus the id) and the secret is still redacted.
- [ ] **Linux** — **macOS**: **HTML replay page** opens in a browser, renders
  the timeline, and contains no plaintext secret (view-source / Ctrl-F the
  planted value).

---

## 5. Crash-safety spot check (AC-4)

Not OS-specific; run once on Linux.

```bash
cd /tmp/hh-qa && export HH_DATA_DIR=/tmp/hh-qa/data
hh run -- python3 -c 'import time; print("working"); time.sleep(30)' &
HH_PID=$!
sleep 2; kill -KILL $HH_PID        # SIGKILL mid-recording
hh list                             # next invocation reconciles to interrupted
hh inspect last                     # still readable
```

- [ ] **Linux**: The SIGKILLed session shows `status: interrupted` (or
  equivalent) in `hh list`, and `hh inspect last` reads it back without
  erroring — no partial-write corruption, no orphaned blob crash.

---

## 6. Sign-off

- [ ] Every checkbox above is ticked for at least **Linux and macOS**, with the
  per-OS columns filled (or **N/A** / **build-only** noted where called out).
- [ ] Codex outcome recorded (ran on which OS, or N/A with reason).
- [ ] Any deviation from this checklist is written down in the release
  announcement with a justification.
- [ ] `docs/1.0-definition-of-done.md` (the automated half) is green on the
  release branch.

**Only after all of the above:** proceed to the tag-and-release runbook in
`RELEASING.md`. The maintainer tags `v1.0.0`; Halfhand's automation does not
tag itself.