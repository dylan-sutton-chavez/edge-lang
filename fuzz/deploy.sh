#!/usr/bin/env bash
# make fuzz-status watches a running campaign from another shell.
set -euo pipefail
cd "$(dirname "$0")"

JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN)}" # One instance per logical core.
DURATION="${DURATION:-0}"
FRESH="${FRESH:-0}"
TIMEOUT_MS="${TIMEOUT_MS:-5000}" # > max bounded run, so slow-but-terminating inputs aren't false hangs
SCHEDULES="${SCHEDULES:-}" # Power schedules handed to the instances in turn.
MUTATE="${MUTATE:-}" # Flags every instance takes, -L 0 for MOpt.

export AFL_SKIP_CPUFREQ=1
export AFL_I_DONT_CARE_ABOUT_MISSING_CRASHES=1
export AFL_NO_UI=1
export AFL_AUTORESUME=1 # resume existing out/ on restart instead of aborting; FRESH=1 wipes it first

(( JOBS < 1 )) && JOBS=1
echo "logical cores: $(getconf _NPROCESSORS_ONLN), instances: $JOBS"

# Regenerate the seed corpus / dictionary if absent, then build the instrumented target.
[ -d in ] && [ -n "$(ls -A in 2>/dev/null)" ] || bash ./seeds.sh
cargo afl build --release
bin=target/release/afl-pipeline

# Seeds come from corpus/ once it holds inputs, or from in/.
SEEDS="${SEEDS:-}"
[ -z "$SEEDS" ] && { [ -n "$(ls -A corpus 2>/dev/null)" ] && SEEDS=corpus || SEEDS=in; }
echo "seeds: $SEEDS"

# `out` is a mounted volume in the container; clear its contents, not the mount point itself.
[ "$FRESH" = "1" ] && { find out -mindepth 1 -delete 2>/dev/null || true; }
mkdir -p out logs

# -V time-box only when DURATION > 0.
vflag=()
(( DURATION > 0 )) && vflag=(-V "$DURATION")

pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; }
trap cleanup INT TERM EXIT

read -ra schedules <<< "$SCHEDULES"
read -ra mutate <<< "$MUTATE"
# The flags of instance $1.
pick() {
  picked=()
  (( ${#schedules[@]} )) && picked+=(-p "${schedules[$(( $1 % ${#schedules[@]} ))]}")
  (( ${#mutate[@]} )) && picked+=("${mutate[@]}")
  true
}

# One main (deterministic) instance, the rest secondaries (havoc); all share -o out and sync.
pick 0
cargo afl fuzz "${vflag[@]}" "${picked[@]}" -t "$TIMEOUT_MS" -i "$SEEDS" -o out -x edge.dict -M m0 "$bin" >logs/m0.log 2>&1 &
pids+=($!)
for i in $(seq 1 $(( JOBS - 1 ))); do
  pick "$i"
  cargo afl fuzz "${vflag[@]}" "${picked[@]}" -t "$TIMEOUT_MS" -i "$SEEDS" -o out -x edge.dict -S "s$i" "$bin" >"logs/s$i.log" 2>&1 &
  pids+=($!)
done

echo "running $JOBS instance(s), logs in ./logs, make fuzz-status shows them"
# An instance that aborts fails the campaign instead of leaving it silently short.
for pid in "${pids[@]}"; do wait "$pid"; done
