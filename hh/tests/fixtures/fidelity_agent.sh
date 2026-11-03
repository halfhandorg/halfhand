#!/bin/sh
# Fixture fake agent for the capture-fidelity harness (hh/tests/fidelity.rs).
# Run in a work dir that already holds keep.txt (20 lines, "line 1".."line
# 20"), old.txt and del.txt. It performs a KNOWN set of effects, each of
# which the test then asserts against the recorded database:
#   - creates two files (made.txt, made2.txt),
#   - modifies one line of a pre-existing file (keep.txt: line 7 -> "line SEVEN"),
#   - renames a file (old.txt -> new.txt),
#   - deletes a file (del.txt),
#   - runs a command whose output is known (wc -l on keep.txt),
#   - exits non-zero once (exit 3, as the very last action).
# POSIX sh only (no `sed -i`, whose syntax differs between GNU and BSD): the
# edit is computed into a temp file outside the watched dir and written back
# over keep.txt in place.
set -eu
printf 'first created file\n' > made.txt
printf 'second created file\nwith two lines\n' > made2.txt
tmp=$(mktemp)
sed 's/^line 7$/line SEVEN/' keep.txt > "$tmp"
cat "$tmp" > keep.txt
rm -f "$tmp"
mv old.txt new.txt
rm del.txt
lines=$(wc -l < keep.txt | tr -d ' ')
echo "fidelity-command-output lines=$lines"
exit 3
