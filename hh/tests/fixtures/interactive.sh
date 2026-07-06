#!/bin/sh
# Interactive PTY transparency fixture (FR-1.1).
#
# Proves the wrapped program is indistinguishable from running it directly in a
# terminal: `tput cols` only succeeds on a real tty, so it printing a nonzero
# column count (80, the PTY default) demonstrates the child runs inside a PTY
# even when hh's own stdout is piped. It then prints an ANSI-colored line and
# reads one line from stdin to exercise raw stdin forwarding + echo.
printf 'cols=%s\n' "$(tput cols 2>/dev/null || echo 0)"
printf '\033[32mgreen-line\033[0m\n'
read line
printf 'echo:%s\n' "$line"