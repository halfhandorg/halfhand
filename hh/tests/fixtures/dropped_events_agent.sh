#!/bin/sh
# Fixture for the dropped-events tests: prints well over 16 KiB (several
# 8 KiB terminal_output chunks), writes one file, and exits 0. POSIX sh.
i=0
while [ "$i" -lt 400 ]; do
  echo "line $i: the quick brown fox jumps over the lazy dog 0123456789"
  i=$((i + 1))
done
echo "survivor" > survivor.txt
echo "done"
