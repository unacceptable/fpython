#!/bin/sh
# Differential test: run each tests/run/*.fpy under CPython and fpython and compare stdout.
# Usage: tests/difftest.sh [files...]
FPY=${FPY:-target/debug/fpython}
TMP=${TMPDIR:-/tmp}/fpy-difftest.$$
mkdir -p "$TMP"
pass=0; fail=0
files="$*"
[ -z "$files" ] && files=$(ls tests/run/*.fpy)
for f in $files; do
  b=$(basename "$f" .fpy)
  python3 "$f" > "$TMP/$b.py.out" 2>&1
  $FPY run "$f" > "$TMP/$b.fpy.out" 2>&1
  if cmp -s "$TMP/$b.py.out" "$TMP/$b.fpy.out"; then
    pass=$((pass+1))
  else
    fail=$((fail+1)); echo "FAIL $f"; diff "$TMP/$b.py.out" "$TMP/$b.fpy.out" | head -20
  fi
done
echo "passed=$pass failed=$fail"
rm -rf "$TMP"
[ $fail -eq 0 ]
