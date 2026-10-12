#!/usr/bin/env bash
# make fuzz-status watches a running campaign from another shell.
set -euo pipefail
cd "$(dirname "$0")"

JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN)}" # One instance per logical core.
DURATION="${DURATION:-0}"
UNTIL="${UNTIL:-0}" # A Unix time to stop at, which wins over DURATION once the build is done.
GRACE="${GRACE:-60}" # Seconds AFL gets to write its stats after SIGINT, before SIGKILL.
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

# The stop is wall-clock time, since -V counts only fuzzing and overran by half an hour.
deadline=0
(( DURATION > 0 )) && deadline=$(( $(date +%s) + DURATION ))
if (( UNTIL > 0 )); then
  deadline=$UNTIL
  (( deadline > $(date +%s) )) || { echo "no time left to fuzz before $UNTIL"; exit 1; }
fi

pids=()
names=()
# SIGINT goes to afl-fuzz alone, so cargo-afl waits while it writes its stats, and SIGKILL takes the whole group after GRACE.
stop() {
  trap - EXIT INT TERM
  (( ${#pids[@]} )) || return 0
  for pid in "${pids[@]}"; do pkill -INT -g "$pid" -x afl-fuzz || true; done
  local end=$(( $(date +%s) + GRACE ))
  for pid in "${pids[@]}"; do
    while kill -0 "$pid" 2>/dev/null && (( $(date +%s) < end )); do sleep 1; done
    kill -KILL -- "-$pid" 2>/dev/null || true
  done
  echo "stopped $JOBS instance(s)"
}
trap stop EXIT
trap 'exit 130' INT TERM

read -ra schedules <<< "$SCHEDULES"
read -ra mutate <<< "$MUTATE"
# The flags of instance $1.
pick() {
  picked=()
  (( ${#schedules[@]} )) && picked+=(-p "${schedules[$(( $1 % ${#schedules[@]} ))]}")
  (( ${#mutate[@]} )) && picked+=("${mutate[@]}")
  true
}

# One main (deterministic) instance and the rest secondaries (havoc) share -o out, each in a process group of its own since cargo-afl passes no signal on to afl-fuzz.
set -m
for i in $(seq 0 $(( JOBS - 1 ))); do
  pick "$i"
  if (( i == 0 )); then role=(-M m0); name=m0; else role=(-S "s$i"); name="s$i"; fi
  cargo afl fuzz "${picked[@]}" -t "$TIMEOUT_MS" -i "$SEEDS" -o out -x edge.dict "${role[@]}" "$bin" >"logs/$name.log" 2>&1 &
  pids+=($!)
  names+=("$name")
done
set +m

echo "running $JOBS instance(s), logs in ./logs, make fuzz-status shows them"
# An instance that ends before the stop time fails the campaign instead of leaving it silently short.
while (( deadline == 0 || $(date +%s) < deadline )); do
  for i in "${!pids[@]}"; do
    kill -0 "${pids[$i]}" 2>/dev/null || { echo "${names[$i]} ended early"; grep -A1 'PROGRAM ABORT' "logs/${names[$i]}.log" || tail -5 "logs/${names[$i]}.log"; exit 1; }
  done
  sleep 5
done
