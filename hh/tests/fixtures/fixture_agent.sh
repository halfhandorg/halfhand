#!/bin/sh
# Fixture fake agent for `hh run` integration tests (SRS acceptance #2).
# Prints to stdout, writes a file to its cwd, and exits nonzero — enough to
# exercise terminal-output capture, file-change capture, and the error
# status path. POSIX sh so the test does not depend on python3.
echo "agent starting"
echo "writing a file"
echo "hello from fixture" > fixture_output.txt
echo "writing again"
echo "hello v2 from fixture" > fixture_output.txt
echo "now exiting with error"
exit 3