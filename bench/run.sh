#!/usr/bin/env bash
# Full benchmark matrix: ddram vs kernel mmap (page cache) vs kernel swap,
# each under the same cgroup memory limit (Linux, root required; the swap
# engine needs an active swap device).
#
#   bench/run.sh [SIZE] [BUDGET] [DIR]
#
# Environment: ENGINES (default "ddram mmap swap"), SEQ_BLOCK (1M),
# RAND_BLOCK (16K; blocks below 64K need a larger vm.max_map_count, which
# this script raises), BIN, OUT.
set -uo pipefail
SIZE=${1:-8G}
BUDGET=${2:-2G}
DIR=${3:-$PWD/bench-data}
BIN=${BIN:-target/release/ddram}
OUT=${OUT:-bench/results-$(date +%Y%m%d-%H%M%S).txt}
ENGINES=${ENGINES:-ddram mmap swap}
SEQ_BLOCK=${SEQ_BLOCK:-1M}
RAND_BLOCK=${RAND_BLOCK:-16K}
mkdir -p "$DIR"
[ -x "$BIN" ] || cargo build -q --release -p ddram-cli
sysctl -qw vm.max_map_count=1048576 || true
: > "$OUT"
run() {
  sync; echo 3 > /proc/sys/vm/drop_caches || true
  echo ">>> $*" | tee -a "$OUT"
  "$BIN" bench --cgroup --size "$SIZE" --budget "$BUDGET" --dir "$DIR" "$@" 2>&1 \
    | grep -v '^  ' | tee -a "$OUT"
}
for eng in $ENGINES; do
  run --engine "$eng" --block "$SEQ_BLOCK" --workload cyclic --passes 3
  run --engine "$eng" --block "$SEQ_BLOCK" --workload cyclic --passes 3 --threads 4
  run --engine "$eng" --block "$RAND_BLOCK" --workload random --ops 200000 --threads 4
  run --engine "$eng" --block "$RAND_BLOCK" --workload zipf --ops 1000000 --threads 4
  run --engine "$eng" --block "$RAND_BLOCK" --workload zipf-write --ops 1000000 --threads 4
done
echo
grep '^RESULT' "$OUT" | sed 's/^RESULT //'
