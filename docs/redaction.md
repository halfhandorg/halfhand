# Redaction — keeping secrets out of your recordings

Halfhand records everything an agent does: prompts, tool output, MCP
payloads, file snapshots, terminal bytes. Real sessions therefore contain
real secrets. The redaction pipeline gives you three tools:

| command / config | what it does |
|---|---|
| `hh scan <session\|last\|--all>` | report detected secrets (never printing them); exit 4 if any |
| `hh redact <session>` | irreversibly remove secrets from a recorded session in place |
| `[redaction] at_record = true` | scrub matches *before they ever hit disk* while recording |

And one guarantee that is always on: **`hh export` output is redacted by
default.** Sessions may be recorded raw locally, but nothing leaves the
machine unredacted by accident. Design rationale and invariants live in
[redaction-design.md](redaction-design.md).

## Threat model

Redaction protects against:

- **Accidental exfiltration** — sharing an export (JSON or `--html`) that
  contains a credential the agent read, printed, or wrote during a session.
- **Retention** — a token that landed in a recording weeks ago and outlived
  its rotation, sitting in `hh.db`/`blobs/` on your laptop.

It does **not** protect against: an attacker with live access to your
machine, secrets captured by tools *outside* Halfhand (shell history, the
agent's own logs), or filesystem-level copies Halfhand cannot see
(snapshots, backups, Time Machine).

## What is caught

Built-in detectors for high-signal named token types:

- AWS access key ids (`AKIA…`, `ASIA…`, and friends)
- GitHub tokens (`ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`, `github_pat_…`)
- GitLab tokens (`glpat-…`, `glrt-…`, `gldt-…`, `glsoat-…`, `glcbt-…`)
- Slack tokens (`xoxb-`/`xoxa-`/`xoxp-`/`xoxr-`/`xoxs-`/`xoxe-`)
- PEM private-key blocks (including header-only fragments left by summary
  truncation)
- JWTs
- Generic high-entropy strings (≥ 40 chars of key-like charset above a
  conservative entropy threshold; pure-hex strings like git SHAs and blob
  hashes are deliberately exempt)
- Your own patterns, via config (below)

The contract, enforced by property tests and a fuzz target: **false
positives are acceptable; a false negative on a named token type is a
bug** — [report it](../SECURITY.md).

Each match is replaced with `{{REDACTED:<type>:<hash8>}}`. The `hash8` is
the first 8 hex chars of the secret's BLAKE3 hash: you can trace *the same*
secret across events and sessions (same tag everywhere) without the secret
being stored or recoverable.

## What is NOT caught

Be honest with yourself about these before sharing anything:

- Secrets with no recognizable shape (a password like `hunter2`, an API key
  that is short or low-entropy) — add a `[redaction] rules` pattern if
  yours have a known prefix.
- Secrets split across terminal-output chunk boundaries or interleaved with
  ANSI color codes.
- Secrets inside binary content (binary blobs are not scanned; binary file
  capture is off by default anyway).
- Fragments: a *partial* token cut off by the 120-char summary limit may
  only be caught by the entropy detector.
- Copies outside the store: SQLite sidecars of other processes, filesystem
  snapshots, backups made before you ran `hh redact`.

## Scanning

```console
$ hh scan last
● session a1b2c3 — 3 secret occurrence(s)
  TYPE               STEP  LOCATION   HASH8     COUNT
  aws-access-key-id  3     body       a1b2c3d4  2
  github-token       5     file .env  deadbeef  1
✗ 3 secret occurrence(s) detected — `hh redact <session>` removes them irreversibly
```

`hh scan --all` sweeps every session; `--json` emits a stable object (see
[json.md](json.md)). The exit code is part of the contract: **0** clean,
**4** findings — wire `hh scan --all` into CI to keep a shared runner's
store clean.

## Redacting in place

```console
$ hh redact a1b2c3
● Irreversibly redact secrets from session a1b2c3 (ok, 42 steps)? Originals are destroyed. [y/N] y
✓ Redacted session a1b2c3 · 3 secret occurrence(s) removed · 5 event(s) rewritten · 2 blob(s) rewritten (2 shredded)
  aws-access-key-id    a1b2c3d4  ×2
  github-token         deadbeef  ×1
```

What happens (see the design doc for the property-tested invariants):
events and affected blobs are rewritten; original blobs whose last
reference was in this session are overwritten with zeros and deleted; the
WAL is checkpointed and the database `VACUUM`ed so no plaintext copy
survives in `hh.db`; and a `redaction_audit` lifecycle event is appended so
the session records what was removed (types and hash8 tags only).

Notes:

- **Irreversible.** There is no undo. The prompt requires a TTY; `--yes`
  skips it for scripts.
- A blob *shared with another session* keeps its original content — run
  `hh scan --all` and redact the other sessions too.
- The zero-overwrite is best-effort: journaling/copy-on-write filesystems
  and SSD wear leveling can retain old sectors physically. It raises the
  bar; it is not forensic erasure.

## Exporting

```console
$ hh export last --out session.json     # redacted, always, by default
$ hh export last --html --out session.html
$ hh export last | jq .session          # stdout works too
```

The whole bundle passes through one redaction chokepoint before a byte is
written. To export raw, `--no-redact` requires typing `yes` at an
interactive prompt — and is refused outright when stdin is not a TTY, so no
script can exfiltrate a raw session by accident. There is deliberately no
`--yes` bypass for this.

## Record-time redaction

```toml
# ~/.config/halfhand/config.toml
[redaction]
at_record = true        # default: false
entropy = true          # default: true — the high-entropy detector
rules = [               # your own detectors; findings show as custom:<name>
  { name = "acme-internal", pattern = "ACME-[0-9A-F]{16}" },
]
```

With `at_record = true`, matches are replaced *before hitting disk*: event
summaries/bodies are scrubbed on the writer task, and blob content (file
snapshots, large payloads) is scrubbed before hashing and compression. The
`hash8` tags still let you correlate one secret across events.

Trade-offs to know about:

- What is stored is the *redacted* text — replay/inspect show tokens where
  secrets were, and a stored file snapshot no longer hash-matches the file
  on disk (diffs still work; they diff the stored snapshots).
- False positives are destructive here: the original is never recorded. If
  the conservative entropy detector still bites you (e.g. sessions full of
  base64 payloads), set `entropy = false` and rely on the named types +
  your own rules.
- It cannot help sessions recorded before it was enabled — that is what
  `hh redact` is for.

`rules` patterns use Rust `regex` syntax (linear-time; no lookaround or
backreferences). An invalid pattern is a hard error, not a silent no-op —
a detector you configured must never quietly fail to load.

## Ready-made rules for common services

The built-ins above cover AWS, GitHub, GitLab, Slack, PEM keys, and JWTs.
For gaps they don't cover yet — AI platforms, payment processors, package
registries, and database URLs with embedded credentials — see the full
`config.toml` (with a copy-paste-safe creation script) in the
[Configuration section of the README](../README.md#configuration).

## Exit codes

Part of the CLI contract (see the README): `0` ok · `1` generic error ·
`2` usage error · `3` session not found · **`4` redaction/policy block**
(`hh scan` with findings).
