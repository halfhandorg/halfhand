# v1.1.0-created database fixture

Produced by **hh 1.1.0** (`32bfe61`, `origin/main` before migration 0007) and
frozen as-is. `hh/tests/migration_from_v1.rs` opens it with the current
binary and proves migration 0007 loses nothing: every pre-existing row and
column reads back byte-identical, the new columns appear, and the legacy
`sessions.model` is carried into `model_id`.

Schema version recorded: 6.

Contents:

- `34a047`: a Claude Desktop adapter session recorded from
  `hh-core/tests/fixtures/claude-desktop/session.jsonl` (`adapter_status =
  active`, legacy `model = claude-sonnet-5`).
- `a468c3`: a generic PTY session of `hh/tests/fixtures/fixture_agent.sh`
  (exit 3, one file change).

Regeneration (not in CI; the committed copy is the fixture). From a checkout
of the target commit, build `hh`, then record both sessions with a temp
`HOME`, `XDG_CONFIG_HOME` and `HH_DATA_DIR`, staging the Desktop transcript
the way `record_claude_desktop_fixture_session` in `hh/tests/cli.rs` does.
Checkpoint the WAL (`PRAGMA wal_checkpoint(TRUNCATE)`, `journal_mode=DELETE`)
before copying `hh.db` and `blobs/` here.
