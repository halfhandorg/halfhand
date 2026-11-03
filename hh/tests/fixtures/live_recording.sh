#!/bin/sh
# Live-recording fixture for the "second hh command must not interrupt a live
# recording" test (migration 0008). Prints a ready marker once `hh run` has
# created the session and spawned it, then blocks on stdin until the test
# releases it — so the test synchronizes on output, never on a sleep.
printf 'HH_LIVE_READY\n'
read line
printf 'released:%s\n' "$line"
