#!/bin/sh
# Diff the native binary against reference answers on every seeds/sN that has seeds/sN-ref.
set -u
BIN=${BIN:-target/release/pipeline}; PY=${PY:-.venv/bin/python}; fail=0
for ref in seeds/*-ref; do
  s=${ref%-ref}; o=target/tmp/check-$(basename "$s"); rm -rf "$o"
  DATA_DIR=$s OUTPUT_DIR=$o NCPU=8 "$BIN" 2>/dev/null || { echo "$s: CRASH"; fail=1; continue; }
  r=$($PY scripts/compare.py "$o" "$ref" | tail -1); echo "$s: $r"; [ "$r" = MATCH ] || fail=1
done
exit $fail
