#!/usr/bin/env bash
# llama.cpp, unmodified, under a cgroup memory limit smaller than the model
# (weights are used in place from the mapping: --no-repack, since repacking
# copies them into anonymous memory on every variant):
# its own mmap loading vs the same binary with libddram_shim.so preloaded
# (learning the access plan on the first pass, then reusing the saved plan).
# Runs are interleaved and repeated; report medians. Linux, root required.
#
#   bench/llama/run.sh MODEL.gguf [BUDGET] [SLACK]
#
# Environment: LLAMA (path to llama-completion), SHIM (path to
# libddram_shim.so), ROUNDS (3), NTOK (64), THREADS (4), PROMPT, OUT.
set -uo pipefail
MODEL=${1:?model path}
BUDGET=${2:-160M}
SLACK=${3:-192M}
LLAMA=${LLAMA:-llama-completion}
SHIM=${SHIM:-target/release/libddram_shim.so}
ROUNDS=${ROUNDS:-3}
NTOK=${NTOK:-64}
THREADS=${THREADS:-4}
PROMPT=${PROMPT:-"Write a short story about a lighthouse keeper who finds a message in a bottle."}
OUT=${OUT:-bench/llama/results-$(date +%Y%m%d-%H%M%S).txt}
PLANFILE="$MODEL.ddram-plan"
limit=$(( $(numfmt --from=iec "$BUDGET") + $(numfmt --from=iec "$SLACK") ))
# cgroup v1 (memory controller mounted separately) or v2 (unified).
if [ -f /sys/fs/cgroup/memory/memory.limit_in_bytes ]; then
  CGROOT=/sys/fs/cgroup/memory; LIMIT_FILE=memory.limit_in_bytes
else
  CGROOT=/sys/fs/cgroup; LIMIT_FILE=memory.max
fi
CG=$CGROOT/ddram-llama-$$
mkdir -p "$CG" && echo "$limit" > "$CG/$LIMIT_FILE" || { echo "cgroup setup failed"; exit 1; }
cleanup() { echo $$ > "$CGROOT/cgroup.procs" 2>/dev/null; rmdir "$CG" 2>/dev/null; }
trap cleanup EXIT
: > "$OUT"
echo "# model=$MODEL size=$(stat -c %s "$MODEL") budget=$BUDGET slack=$SLACK ntok=$NTOK threads=$THREADS" | tee -a "$OUT"
run() { # name, env...
  local name=$1; shift
  sync; echo 3 > /proc/sys/vm/drop_caches
  echo ">>> $name" | tee -a "$OUT"
  ( echo $BASHPID > "$CG/cgroup.procs"
    env "$@" "$LLAMA" -m "$MODEL" -p "$PROMPT" -n "$NTOK" -t "$THREADS" --temp 0 -no-cnv -c 512 --no-repack 2>&1 \
      | grep -E "prompt eval time|eval time|load time|ddram-shim|error" ) | tee -a "$OUT"
  echo "RESULT $name $(grep -E ' eval time' "$OUT" | tail -1 | sed 's/.*(//; s/).*//')" | tee -a "$OUT"
}
rm -f "$PLANFILE"
for r in $(seq "$ROUNDS"); do
  run "kernel-mmap"          DDRAM_BUDGET=
  run "ddram-no-plan"        DDRAM_BUDGET="$BUDGET" DDRAM_PLAN=0 LD_PRELOAD="$SHIM"
  rm -f "$PLANFILE"
  run "ddram-learn-plan"     DDRAM_BUDGET="$BUDGET" DDRAM_PLAN=1 DDRAM_VERBOSE=1 LD_PRELOAD="$SHIM"
  run "ddram-saved-plan"     DDRAM_BUDGET="$BUDGET" DDRAM_PLAN=1 DDRAM_VERBOSE=1 LD_PRELOAD="$SHIM"
done
echo
grep '^RESULT' "$OUT"
