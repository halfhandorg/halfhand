#!/bin/sh
# Fixture fake agent for the file-change before-side regression tests
# (hh/tests/before_side.rs). Run in a work dir that already holds f.txt (20
# lines, "line 1".."line 20"), del.txt and old.txt. It makes the three kinds
# of change whose before-side used to be lost for a pre-existing file:
#   - a one-line in-place edit of f.txt (line 7 -> "line SEVEN"),
#   - deleting del.txt,
#   - renaming old.txt to new.txt.
# POSIX sh only (no `sed -i`, whose syntax differs between GNU and BSD): the
# edit is computed into a temp file outside the watched dir and written back
# over f.txt in place, preserving every other line — including any edit the
# developer made before the session started.
set -eu
tmp=$(mktemp)
sed 's/^line 7$/line SEVEN/' f.txt > "$tmp"
cat "$tmp" > f.txt
rm -f "$tmp"
rm del.txt
mv old.txt new.txt
echo "agent done"
