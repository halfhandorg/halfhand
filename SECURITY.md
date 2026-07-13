# Security Policy

Halfhand is a local-first flight recorder for AI agents. Its security
posture rests on three properties:

1. **No network** — `hh` makes zero outbound network calls (SRS NFR-2),
   enforced by a CI tripwire that fails the build if an HTTP client crate
   enters the dependency graph.
2. **Local permissions** — the database and blob store are created
   `0600`/`0700`; keystroke recording is off by default (NFR-4).
3. **Redaction** — exports are redacted by default, `hh scan` detects
   recorded secrets, and `hh redact` removes them irreversibly. See
   [docs/redaction.md](docs/redaction.md) for the threat model and
   [docs/redaction-design.md](docs/redaction-design.md) for the invariants.

## Reporting a vulnerability

Please report security issues **privately** via
[GitHub Security Advisories](https://github.com/halfhandorg/halfhand/security/advisories/new)
("Report a vulnerability" on the repository's Security tab). Do not open a
public issue for anything you believe is exploitable.

Include what you can: affected version (`hh --version`), platform, a
reproduction or proof of concept, and your assessment of impact.

What you can expect from us:

- **Acknowledgement within 7 days**, and a triage verdict (accepted /
  needs-info / declined) within 14.
- A fix or documented mitigation for accepted reports in the next release,
  with severity-appropriate urgency.
- Credit in the release notes and advisory, unless you prefer otherwise.
- No legal action for good-faith research within the scope below.

## Scope

In scope:

- **Redaction bypasses**: a *named token type* (AWS keys, GitHub/GitLab
  tokens, Slack tokens, PEM private keys, JWTs) that survives `hh export`'s
  default redaction, `hh redact`, or record-time redaction is a
  vulnerability by contract — false negatives on named types are bugs.
- Recovery of redacted secrets from `hh.db` or the blob store after
  `hh redact` completed successfully (excluding filesystem-level copies
  outside Halfhand's reach — snapshots, backups, wear-leveled sectors,
  which are documented best-effort limits).
- Panics or memory unsafety triggered by malformed external input (agent
  JSONL transcripts, MCP JSON-RPC frames, imported/opened databases,
  config files) — these parsers are fuzzed and must fail with errors only.
- Any outbound network activity from the `hh` binary.
- File-permission regressions on the data directory.

Out of scope:

- Secrets missed by the *generic* high-entropy detector (documented
  best-effort; tune with `[redaction] rules`).
- Attacks requiring an attacker who already has your local user account.
- The behavior of wrapped agents themselves.

## Supported versions

Pre-1.0, only the latest released beta receives security fixes. Pin a
version in CI and track releases.
