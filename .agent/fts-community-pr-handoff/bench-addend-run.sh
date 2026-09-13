#!/usr/bin/env bash
# Reproducible full-set SBG run for one engine.
# usage: bench-addend-run.sh <engine> <target-dir> <out-root> <tag> <reps> <core>
#
# This box also hosts other agent sessions that drive `do_query`, and one of
# their harnesses runs `pkill -x do_query`. We therefore NEVER kill by process
# name: we only reap our own binary, and we report foreign do_query processes
# instead of silently fighting them (mutual SIGTERM made earlier runs die as
# `make: *** [serve] Terminated` with an empty client response).
set -uo pipefail
SBG=/data/arrow/code/search-benchmark-game
QUERIES_FILE="${QUERIES_FILE:-$SBG/queries.txt}"
ENGINE="${1:?engine}"; TDIR="${2:?target dir}"; OUT="${3:?out root}"
TAG="${4:?tag}"; REPS="${5:?reps}"; CORE="${6:?core}"
mkdir -p "$OUT"
MY_BIN="$TDIR/release/do_query"

reap_own() { pkill -f "$MY_BIN" 2>/dev/null || true; }
busy() { pgrep -x cargo >/dev/null || pgrep -x rustc >/dev/null; }
wait_quiet() {
  while busy; do sleep 10; done
  sleep 10
  while busy; do sleep 10; done
}

wait_quiet
echo "quiet (no cargo/rustc) at $(date +%H:%M:%S), core=$CORE, engine=$ENGINE"
foreign=$(pgrep -x do_query 2>/dev/null | wc -l)
if [ "$foreign" != "0" ]; then
  echo "WARNING: $foreign foreign do_query process(es) running - results will be contended"
  echo "         and the other harness runs 'pkill -x do_query', which will SIGTERM ours:"
  pgrep -af do_query | sed 's/^/    /'
fi

unset LANCE_HACK_IU_MUST_DRIVE LANCE_HACK_IU_NO_CLIP LANCE_HACK_IU_FORCE_INTERSECT
unset LANCE_HACK_IU_MAXSCORE LANCE_HACK_IU_SIMPLE LANCE_HACK_IU_TIGHT
unset LANCE_HACK_UNION_BUF LANCE_HACK_FORCE_QUANTIZED_NORMS
unset LANCE_DIAG_IU LANCE_DIAG_WORK LANCE_DIAG_AND LANCE_FTS_AND_SCORE_FIRST
unset LANCE_FTS_BULK_AND
export LANCE_FTS_NUM_SHARDS=1 LANCE_FTS_PARTITION_SIZE=65536 LANCE_FTS_SEARCH_CHUNK=1
export LANCE_BENCH_RUNTIME=current_thread
export JAVA_HOME=/tmp/jdk-21.0.8+9 PATH="/tmp/jdk-21.0.8+9/bin:$PATH"
export CORPUS=/dev/shm/sbg/corpus.json SBG_ROOT=/dev/shm/sbg
export COMMANDS='TOP_10' NUM_ITER=10 WARMUP_TIME=20
export CARGO_TARGET_DIR="$TDIR"

echo "binary sha256: $(sha256sum "$MY_BIN" | awk '{print $1}')"
for r in $(seq 1 "$REPS"); do
  reap_own
  sleep 1
  d="$OUT/${TAG}-r${r}"; mkdir -p "$d"; rm -f "$d/results.json" "$d/client.log"
  ( cd "$d" && taskset -c "$CORE" python3 "$SBG/src/client.py" "$QUERIES_FILE" "$ENGINE" >client.log 2>&1 )
  rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "rep $r FAILED rc=$rc - see $d/client.log"
  fi
  echo "done $TAG r$r $(date +%H:%M:%S)"
done
echo RUN_DONE
