#!/bin/sh
# Long-running fixture for the SIGKILL / interrupted-session test (AC-4).
# Sleeps long enough that the test can SIGKILL `hh` mid-recording.
sleep 30