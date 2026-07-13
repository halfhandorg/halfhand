#!/bin/sh
# Fixture fake agent that leaks secrets (redaction pipeline integration
# tests, docs/redaction-design.md). Prints a fake AWS access key id to the
# terminal and writes a fake GitHub token into a file, so seeded secrets land
# in both terminal-output events and a file-change blob. The values match the
# real token shapes but are not real credentials.
echo "agent starting"
echo "aws key: AKIAIOSFODNN7EXAMPLE"
echo "GITHUB_TOKEN=ghp_FAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKE0001" > .env-fixture
echo "wrote credentials file"
exit 0
