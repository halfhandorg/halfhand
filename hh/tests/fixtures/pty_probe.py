#!/usr/bin/env python3
"""PTY transparency probe (FR-1.1) for platforms without `tput` (Windows).

Proves the wrapped program runs inside a real (Con)PTY even when hh's own
stdout is piped: os.get_terminal_size() only reports a nonzero geometry when
stdout is a console, and the ANSI-colored line must pass through hh verbatim.
Unlike interactive.sh this does not read stdin -- raw stdin round-trips
through ConPTY are covered manually (see docs/platforms.md).
"""
import os
import sys

try:
    cols = os.get_terminal_size().columns
except OSError:
    cols = 0
print(f"cols={cols}")
print("\033[32mgreen-line\033[0m")
sys.stdout.flush()
