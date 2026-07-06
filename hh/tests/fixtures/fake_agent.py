#!/usr/bin/env python3
"""Fake agent fixture for `hh run` integration tests (FR-1).

Exercises the recorder end-to-end for a generic agent:
  * prints ANSI-colored output (terminal_output capture, FR-1.3),
  * creates, modifies, and deletes files with real time gaps so each change
    kind is observed as a distinct `file_change` row (FR-1.4),
  * writes under built-in-ignored paths (`target/`, `.git/`) which must produce
    *no* events,
  * exits 3 so the session finalizes with `status=error`, `exit_code=3`.

Each filesystem operation is followed by a short sleep so the operations land
outside the watcher's 100 ms debounce window (see hh-record watcher.rs) — this
keeps create/modify/delete as separate rows instead of coalescing, and makes
the fixture robust to per-OS watcher delivery latency. doomed.txt's
create-to-delete gap is wider than the others: macOS's FSEvents backend can
coalesce a whole create+modify+delete history for one path into a single
notification batch under CI load, so this path needs real separation, not
just enough to clear the 100 ms debounce window. The total runtime is ~3 s.
"""
import os
import sys
import time

# ANSI red banner — proves raw terminal bytes (including escapes) are captured.
sys.stdout.write("\033[31magent-start\033[0m hello from fake agent\n")
sys.stdout.flush()
time.sleep(0.2)

# A file we will delete later. Created early so the create and delete events
# are well separated in time (survives debounce coalescing, and gives macOS
# FSEvents room to deliver the create as its own notification rather than
# folding it into the same batch as the eventual delete) and the delete
# carries a before_hash (we observed the create earlier in the session).
with open("doomed.txt", "w") as f:
    f.write("bye-bye\n")
time.sleep(2.0)

# A brand-new file: exercises `created`.
with open("created.txt", "w") as f:
    f.write("created-content\n")
time.sleep(0.2)

# A pre-existing file (the test seeds "modified.txt" with "orig\n" before
# launching hh) gets overwritten: exercises `modified`. before_hash is None
# under lazy before-blob capture (we never observed the original); after_hash
# captures the new content.
with open("modified.txt", "w") as f:
    f.write("modified-content\n")
time.sleep(0.2)

# Delete the file we created at the start: exercises `deleted` with a
# before_hash (seen created earlier) and a null after_hash.
os.remove("doomed.txt")
time.sleep(0.2)

# Ignored paths — built-in ignores (`.git/`, `target/`); must produce NO
# file_change events.
os.makedirs("target", exist_ok=True)
with open("target/ignored.txt", "w") as f:
    f.write("nope\n")
os.makedirs(".git", exist_ok=True)
with open(".git/config", "w") as f:
    f.write("nope\n")

sys.stdout.write("done\n")
sys.stdout.flush()
sys.exit(3)