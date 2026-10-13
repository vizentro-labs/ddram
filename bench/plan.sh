#!/usr/bin/env bash
# Plan-driven memory benchmarks: ddram with an access plan vs ddram v0.1
# heuristics vs kernel mmap (and kernel swap for the chunks workload), every
# engine under the same cgroup memory limit, runs interleaved so drift in the
# backing disk affects all engines alike (Linux, root required).
#
#   bench/plan.sh [SIZE] [BUDGET] [DIR]
#
# Environment: ROUNDS (2), BIN, OUT, SWAP (1 = create a swap file in DIR for
# the swap engine), LAYERS (32), EXPERTS (8), ACTIVE (2), TOKENS (4).
set -uo pipefail
SIZE=${1:-8G}
BUDGET=${2:-2G}
DIR=${3:-$PWD/bench-data}
BIN=${BIN:-target/release/ddram}
OUT=${OUT:-bench/results-plan-$(date +%Y%m%d-%H%M%S).txt}
ROUNDS=${ROUNDS:-2}
SWAP=${SWAP:-1}
LAYERS=${LAYERS:-32}
EXPERTS=${EXPERTS:-8}
ACTIVE=${ACTIVE:-2}
TOKENS=${TOKENS:-4}
mkdir -p "$DIR"
[ -x "$BIN" ] || cargo build -q --release -p ddram-cli
sysctl -qw vm.max_map_count=1048576 || true
: > "$OUT"
run() {
  sync; echo 3 > /proc/sys/vm/drop_caches || true
  echo ">>> $*" | tee -a "$OUT"
  "$BIN" "$@" --cgroup --budget "$BUDGET" --dir "$DIR" 2>&1 | grep -v '^  ' | tee -a "$OUT"
}
# 1. Chunks: a fixed non-sequential order of 4 MiB chunks, swept repeatedly.
SWAPFILE=""
if [ "$SWAP" = 1 ]; then
  SWAPFILE="$DIR/ddram-bench.swap"
  if ! swapon --show | grep -q "$SWAPFILE"; then
    fallocate -l "$SIZE" "$SWAPFILE" 2>/dev/null || dd if=/dev/zero of="$SWAPFILE" bs=1M count=$(( $(numfmt --from=iec "$SIZE") >> 20 )) status=none
    chmod 600 "$SWAPFILE" && mkswap -q "$SWAPFILE" && swapon "$SWAPFILE" || { echo "swap setup failed"; SWAPFILE=""; }
  fi
fi
for r in $(seq "$ROUNDS"); do
  run bench --engine ddram --workload chunks --size "$SIZE" --passes 3 --threads 4
  run bench --engine ddram --workload chunks --size "$SIZE" --passes 3 --threads 4 --plan
  run bench --engine mmap  --workload chunks --size "$SIZE" --passes 3 --threads 4
  [ -n "$SWAPFILE" ] && run bench --engine swap --workload chunks --size "$SIZE" --passes 3 --threads 4
done
if [ -n "$SWAPFILE" ]; then swapoff "$SWAPFILE"; rm -f "$SWAPFILE"; fi
# 2. Dense decode: every token reads every layer.
for r in $(seq "$ROUNDS"); do
  run llm --engine ddram --model-size "$SIZE" --layers "$LAYERS" --tokens "$TOKENS"
  run llm --engine ddram --model-size "$SIZE" --layers "$LAYERS" --tokens "$TOKENS" --plan
  run llm --engine mmap  --model-size "$SIZE" --layers "$LAYERS" --tokens "$TOKENS"
done
# 3. Mixture-of-experts decode: ACTIVE of EXPERTS experts per layer per token.
for r in $(seq "$ROUNDS"); do
  run llm --engine ddram --model-size "$SIZE" --layers "$LAYERS" --tokens "$TOKENS" --experts "$EXPERTS" --active "$ACTIVE"
  run llm --engine ddram --model-size "$SIZE" --layers "$LAYERS" --tokens "$TOKENS" --experts "$EXPERTS" --active "$ACTIVE" --plan
  run llm --engine mmap  --model-size "$SIZE" --layers "$LAYERS" --tokens "$TOKENS" --experts "$EXPERTS" --active "$ACTIVE"
done
echo
grep '^RESULT' "$OUT" | sed 's/^RESULT //'
