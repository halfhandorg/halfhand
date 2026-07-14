# Stability

Halfhand is heading to `1.0.0`. This document is the promise `1.0.0` makes and
every release after it keeps: what's frozen, what's allowed to grow, and what
has to go through a deprecation cycle instead of just changing. It formalizes
the "backward compatibility is a feature" rule from `CLAUDE.md`'s v1.0.0
addendum.

Everything here applies from `1.0.0` onward. Before `1.0.0` (the current
`0.1.x-beta` series), the on-disk and JSON schemas may still move faster than
this policy allows — pin a beta version in CI rather than tracking `latest`
until `1.0.0` ships (see `README.md`).

## (a) CLI: flags, subcommands, exit codes

- Every documented subcommand, flag, and its current default behavior is
  stable. New subcommands and new flags are always additive — they may be
  added freely, never as a reason to change what an existing invocation does.
- **Removing** a subcommand or flag, or changing what an existing flag
  combination does, requires a deprecation cycle: at least one minor release
  where the old behavior still works but prints a warning (via
  `hh_core::warn_deprecated`, the standard one-line stderr format — see
  `hh-core/src/deprecation.rs`), before it can be removed in a later minor
  release. Halfhand does not do major-version-only removals within the CLI —
  a deprecation warning must have shipped first, full stop.
- Exit codes are part of the contract, tested by
  `hh/tests/cli_conformance.rs`:

  | Code | Meaning | Where |
  |---|---|---|
  | `0` | success | every command on success |
  | `1` | generic error | any other failure (including a refused irreversible action, e.g. `hh redact`/`hh export --no-redact` refusing a non-interactive confirmation) |
  | `2` | usage error | clap's own flag/argument parsing (unknown flag, missing required arg) |
  | `3` | session not found | any subcommand that resolves a session id/`last` argument and fails to |
  | `4` | redaction/policy block | `hh scan` exiting with findings — the one place this code is used today, so CI can gate on `hh scan --all` |

  New exit codes may be added for new commands; an existing code's meaning
  for an existing command never changes.
- `--help` output (both `hh --help` and every `hh <sub> --help`) is
  snapshot-tested (`hh/tests/cli_conformance.rs`); wording may improve, but a
  documented flag or subcommand does not silently disappear from it.

## (b) `--json`

- `schema` is frozen at **`2`** for the 1.0 series. See `docs/json.md` for
  the full field-by-field reference and the diff from `schema:1`.
- Additive changes — a new field, a new possible value for an existing
  field (e.g. a new `agent_kind`), a new `body` shape for an event kind — do
  **not** bump `schema`. They're documented in `docs/json.md` as they land.
  A documented field is always present once schema `2` documents it (never
  conditionally omitted); consumers must ignore fields and enum values they
  don't recognize rather than error.
- A field is never removed, renamed, or retyped within `schema:2`. That kind
  of change ships as a new `schema` value with both shapes documented side
  by side in `docs/json.md`, gated behind a major version of `hh`.
- `hh-core::event::JSON_SCHEMA_VERSION` is the single constant every
  JSON-emitting call site reads `schema` from (session objects, event/step
  objects, and the `doctor`/`gc`/`stats`/`scan`/`export` objects) — one
  counter for the whole family, not one per command.

## (c) On-disk database

- Migrations are forward-only and applied automatically, idempotently, on
  every `Store::open` (`hh-core/src/migrations.rs`, DR-1). There is no
  down-migration and none is planned.
- Any `1.x` release of `hh` can open and correctly read a database written
  by any earlier `1.x` release. A migration only ever adds (a table, a
  column, an index) — see `hh-core/src/migrations.rs`'s own module doc: "the
  v1.0.0 addendum permits additive schema changes … without a deprecation
  cycle; a *breaking* change to an existing table must go through
  STABILITY.md's policy instead." No migration under this policy breaks that
  promise; if one ever needs to, it requires a major version of `hh` and an
  explicit, documented migration path (e.g. `hh export --bundle` on the old
  version, `hh import` on the new one) — not a silent schema break.
- `hh.db` **remains directly queryable** with `sqlite3` (DR-2) — that
  transparency isn't going away. But the SQL schema itself (table/column
  names, types, indexes) is *not* frozen the way `--json` is: migrations may
  still reshape it release to release. **The supported programmatic
  interface is `--json` and `hh export` bundles, not raw SQL** — build
  tooling against those, and treat direct `hh.db` queries as a debugging/
  power-user affordance, not an API.

## (d) MSRV

- The minimum supported Rust version is stated once, in the workspace
  `Cargo.toml` (`rust-version.workspace = true` in `hh-core`/`hh-record`/
  `hh`; currently `1.75`).
- An MSRV bump only happens in a **minor** release, never a patch release,
  and is called out in `CHANGELOG.md`.

## (e) `hh-core` semver policy

- `hh-core`'s public Rust API (types, functions, trait, including the
  `Adapter` trait new adapters implement) follows semver for real: additive
  changes are always fine; a breaking change to a `1.x` `hh-core` requires a
  major version.
- This is enforced in CI today, ahead of the crate ever being published:
  `.github/workflows/ci.yml`'s `semver-checks` job runs
  `cargo semver-checks check-release -p hh-core --baseline-rev origin/main
  --release-type minor` on every PR — `--release-type minor` means
  "additions are fine, breakage is not," exactly this policy. The one
  standing exclusion (`--exclude enum_marked_non_exhaustive`) covers enums
  that predate the `#[non_exhaustive]` convention and were marked as a
  one-time cleanup, not a recurring allowance.
- Growth-prone public types (`SessionRow`, `Config`, the error enums,
  `AgentKind`, …) are `#[non_exhaustive]` specifically so a new field or
  variant stays additive under that check instead of registering as a
  break. New public types added after `1.0.0` should default to
  `#[non_exhaustive]` unless there's a specific reason a caller needs
  exhaustive matching or construction.

## What this document does not cover

- The replay TUI's keybindings and rendered layout, and `hh export --html`'s
  page — these are user experience, not a machine-readable contract, and can
  keep improving without a deprecation cycle.
- `hh.db`'s SQL schema shape (see (c) above) — deliberately not frozen.
- Anything under `docs/adr/` — architecture decisions, not compatibility
  promises.
