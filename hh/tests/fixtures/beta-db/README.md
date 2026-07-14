# Beta-created database fixture

This fixture was produced by **v0.1.0-beta.1** (git tag `v0.1.0-beta.1`) running
the `hh/tests/fixtures/fake_agent.py` agent, and is frozen as-is. It exists so
`hh/tests/migration_from_beta.rs` can prove that a 1.0 binary opens a database
created by the beta, applies any pending migrations, and lists / inspects /
exports / replays the session correctly — the forward-compatibility half of
STABILITY.md's storage contract.

Regeneration (do NOT do this in CI; the committed copy is the fixture):

```
git clone --branch v0.1.0-beta.1 --depth 1 <repo> /tmp/beta
cd /tmp/beta && cargo build --bin hh
HH_DATA_DIR=/tmp/beta-data HOME=/tmp/beta-home XDG_CONFIG_HOME=/tmp/beta-home \
  ./target/debug/hh run -- python3 hh/tests/fixtures/fake_agent.py
cp /tmp/beta-data/hh.db  <repo>/hh/tests/fixtures/beta-db/hh.db
cp -R /tmp/beta-data/blobs <repo>/hh/tests/fixtures/beta-db/blobs
```

Schema version recorded: 2 (frozen by the 1.0 conformance groundwork; beta.1 is
the freeze point, so this fixture exercises forward readability rather than a
schema delta).
