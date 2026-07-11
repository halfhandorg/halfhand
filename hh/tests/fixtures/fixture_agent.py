#!/usr/bin/env python3
"""Fixture fake agent for `hh run` integration tests (SRS acceptance #2).

Python port of fixture_agent.sh for platforms without a POSIX `sh` (Windows,
where hh spawns it through portable-pty's ConPTY backend). Prints to stdout,
writes a file to its cwd, and exits nonzero -- enough to exercise
terminal-output capture, file-change capture, and the error status path.
Behavior must stay in lockstep with fixture_agent.sh and fixture_agent.ps1.
"""
import sys

print("agent starting")
print("writing a file")
with open("fixture_output.txt", "w") as f:
    f.write("hello from fixture\n")
print("writing again")
with open("fixture_output.txt", "w") as f:
    f.write("hello v2 from fixture\n")
print("now exiting with error")
sys.stdout.flush()
sys.exit(3)
