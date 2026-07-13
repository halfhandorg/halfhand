# Redaction design (v0.2)

Status: implemented. This is the design doc SRS §8 requested for the
redaction/secret-scrubbing pipeline, written before the implementation and
kept as the reference for its invariants. User-facing docs live in
[redaction.md](redaction.md); the disclosure policy lives in
[SECURITY.md](../SECURITY.md).

## Problem

Halfhand records everything an agent does — prompts, tool output, MCP
payloads, file snapshots, raw terminal bytes. Real sessions therefore contain
real secrets (SRS NFR-4 already warns about this). Two failure modes matter:

1. A secret is recorded and later *leaves the machine* in an export.
2. A secret sits in the local store longer than the user intended.

The design goal is **nothing leaves the machine unredacted by accident**,
and **local removal is possible and irreversible**. It is explicitly *not* a
goal to guarantee the local store never contains secrets — recording raw
locally is the default, matching Halfhand's local-first trust model.

## Detection engine (`hh_core::redact`)

A `Detectors` set runs pluggable detectors over any UTF-8 text. Each match
yields a `Finding { kind, span, hash8 }`. Built-in detectors are compiled
regexes for high-signal named token types:

| kind | pattern sketch |
|---|---|
| `aws-access-key-id` | `(AKIA\|ASIA\|ABIA\|ACCA\|A3T…)[A-Z0-9]{16}` |
| `github-token` | `gh[pousr]_…`, `github_pat_…` |
| `gitlab-token` | `glpat-…`, `glrt-…`, `gldt-…`, `glsoat-…`, `glcbt-…` |
| `slack-token` | `xox[baprs]-…` |
| `private-key` | `-----BEGIN … PRIVATE KEY-----` → matching `END` block |
| `jwt` | `eyJ…\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+` (three base64url parts) |
| `high-entropy` | ≥40-char base64/hex-charset token above a conservative Shannon-entropy threshold |
| `custom:<name>` | user regexes from config `[redaction] rules` |

Contract (tested as such): **false positives are acceptable; a false negative
on a named token type is a bug.** Property tests embed each named token type
in random surrounding text and assert detection; a fuzz target asserts the
engine never panics on arbitrary input.

The entropy detector is deliberately conservative: it only considers
unbroken runs of `[A-Za-z0-9+/=_-]` of length ≥ 40, requires high per-char
Shannon entropy, and skips pure-hex strings — Halfhand's own BLAKE3 hashes
(64 lowercase hex) appear throughout recorded data and must never be
redacted, or blob references would break.

### The replacement token

Matches are replaced with `{{REDACTED:<kind>:<hash8>}}` where `hash8` is the
first 8 hex chars of `BLAKE3(secret bytes)`. Properties:

- **One-way**: 8 hex chars (32 bits) of a keyed-nothing hash of a
  high-entropy input; the secret is not recoverable from it.
- **Correlatable**: the same secret yields the same `hash8` everywhere, so a
  user can trace one leaked credential across events and sessions without
  ever seeing it.
- **Fixed point**: the token itself never matches any detector, so redaction
  is idempotent (`redact(redact(x)) == redact(x)`, property-tested).

### JSON-aware redaction

Event bodies and overflowed blob payloads are JSON. Redacting the *encoded*
text would miss multi-line secrets (a PEM block is `\n`-escaped inside a JSON
string), so structured payloads are redacted by walking the parsed value tree
and rewriting each string scalar. Plain text (summaries, terminal output) is
redacted as text. Non-UTF-8 blob content is skipped (documented limitation —
binary content storage is off by default anyway).

## Enforcement points

There are exactly two, and they bracket the data's lifecycle:

**1. Record-time (opt-in, `[redaction] at_record = true`).** Matches are
replaced *before hitting disk*. Two chokepoints cover every write path:
the single-writer task redacts `summary` and `body_json` before `INSERT`,
and `BlobStore::put` redacts UTF-8 content before compressing. The recorder
(`hh run`, `hh mcp-proxy`) threads one shared `Arc<Detectors>` into both.
Because the blob is redacted before hashing, the stored hash is the hash of
the redacted content — content-addressing stays internally consistent, at
the cost that a stored file snapshot no longer hash-matches the on-disk
original. That is the point of record-time redaction and is documented.

**2. Export-time (always on by default).** `hh export` (JSON bundle or
`--html`) serializes the entire session into one JSON tree and passes it
through a **single redaction chokepoint** before any byte is written out.
There is no code path that writes export output without passing that
chokepoint. Opt-out requires `--no-redact` *plus* an interactive "yes"
confirmation; with a non-TTY stdin, `--no-redact` is refused outright — a
script cannot silently exfiltrate raw sessions.

Sessions may be recorded raw locally (default); enforcement point 2
guarantees nothing leaves the machine unredacted by accident.

## Commands

- **`hh scan <session|last|--all> [--json]`** — runs the detectors over every
  event summary, body, referenced blob, and the session's recorded command
  line. Reports `(type, step, location, hash8)` — never the secret itself.
  Exit code **4** when findings exist (the documented redaction/policy exit
  code), 0 when clean: CI can gate on it.
- **`hh redact <session> [--yes]`** — applies redaction in place:
  1. rewrites `events.summary` / `events.body_json`;
  2. rewrites affected blobs: redacted content is `put` as a new blob, every
     reference in this session (`events.blob_hash`,
     `file_changes.before_hash/after_hash`) is repointed, refcounts move
     with the references;
  3. originals that reach refcount 0 are **shredded** (best-effort overwrite
     with zeros + fsync, then unlink) and their rows GC'd;
  4. a redaction **audit event** (kind `lifecycle`,
     `body_json.redaction_audit = { types, hash8s, counts }`) is appended so
     the session self-documents what was removed;
  5. the WAL is checkpointed (`TRUNCATE`) and the DB `VACUUM`ed so plaintext
     does not survive in the WAL or freelist pages.

## Irreversibility invariants

After `hh redact <s>` completes, for every secret that was only referenced
by session `s`:

- **I1** No detector match for it exists in any row of `hh.db` — including
  the raw DB file bytes (guaranteed by the checkpoint + VACUUM step).
- **I2** No blob in the blob store decompresses to content containing it;
  the original blob file has been overwritten and removed.
- **I3** Refcount books stay balanced: `blobs.refcount` equals the number of
  referencing events across all sessions, and no referenced blob was
  deleted.
- **I4** The audit event exists and contains no secret material (types +
  hash8 only).

I1–I3 are property-tested (seeded secrets of every named type across
summary/body/blob/terminal locations; after redact, `hh scan` is empty, a
byte-search of `hh.db` finds nothing, and every surviving blob is clean).

A blob shared with *another* session keeps its original content — redaction
is per-session, and destroying another session's data would be wrong. `hh
scan --all` reveals such residues; the user docs say so explicitly.

Secure deletion is **best-effort** and documented as such: on journaling or
copy-on-write filesystems and SSDs with wear leveling, overwritten sectors
may survive physically. The overwrite raises the bar; it is not a forensic
guarantee.

## Config

```toml
[redaction]
at_record = false      # opt-in record-time scrubbing
entropy = true         # the conservative high-entropy detector
rules = [              # user-defined detectors (kind = "custom:<name>")
  { name = "acme-internal", pattern = "ACME-[0-9A-F]{16}" },
]
```

Unknown keys warn (SRS §4.2); an invalid `pattern` is an actionable error at
load time, not a silent no-op.

## Compatibility

Everything here is additive: new config table, new subcommands, new exit
code semantics only for the new `hh scan`, no schema migration (the audit
event reuses `kind = 'lifecycle'` precisely so the documented `events.kind`
value set is unchanged). Existing `--json` output is untouched; `hh scan
--json` and `hh export` are new, versioned surfaces (`"schema": 1`).

## Known limitations (documented in redaction.md)

- Secrets split across terminal chunks or interleaved with ANSI escapes can
  evade the text detectors.
- Non-UTF-8 (binary) blob content is not scanned.
- Record-time redaction cannot help a session recorded before it was enabled
  — that is what `hh redact` is for.
- SQLite `-wal`/`-shm` sidecars from *other* live processes, filesystem
  snapshots, and backups are outside Halfhand's reach.
