#!/bin/sh
# Time the native binary N times on a dataset: bench.sh [DATA=bench] [N=5]
# Prints every wall time, then min and median. Pinned to CPUS (8) cores, NCPU=CPUS.
set -eu
DATA=${1:-bench}; N=${2:-5}; BIN=${BIN:-target/release/pipeline}; CPUS=${CPUS:-8}
OUT=target/tmp/bench-out; TF=target/tmp/bench-time; mkdir -p target/tmp; all=""
for i in $(seq "$N"); do
  rm -rf "$OUT"; mkdir -p "$OUT"
  DATA_DIR="$DATA" OUTPUT_DIR="$OUT" NCPU="$CPUS" /usr/bin/time -f "%e" -o "$TF" \
    taskset -c 0-$((CPUS - 1)) "$BIN" 2>/dev/null
  all="$all $(cat "$TF")"
done
echo "runs:$all"
echo "$all" | tr ' ' '\n' | grep . | sort -n | awk '{a[NR]=$1} END {printf "min %.2f  median %.2f  (n=%d)\n", a[1], a[int((NR+1)/2)], NR}'
