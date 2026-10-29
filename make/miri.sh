#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

# One suite walked a test per core at a time, what ran kept in a table under miri/ that the Actions cache carries to the next run.
SUITE="${SUITE:?pass SUITE, one of vm snapshot modules parser lexer abi}"
table="miri/$SUITE.tsv"
# Tests start only while MARGIN seconds remain before the job ends.
deadline=$(( ${STARTED:-$(date +%s)} + ${MINUTES:-355} * 60 - ${MARGIN:-600} ))
workers="${JOBS:-$(nproc)}"
MODES=("" "-Zmiri-tree-borrows" "-Zmiri-strict-provenance -Zmiri-symbolic-alignment-check")
NAMES=("Stacked Borrows" "Tree Borrows" "Strict provenance")
# each listed test runs in that many slices sized for 16 cores and 100 minutes
declare -A SLICES=([vm::test::test_cases]=4 [vm::test::strict_cases]=4 [snapshot::test::vm_corpus_preempt_equivalence]=4 [snapshot::test::vm_corpus_restore_at_a_preempt]=4 [snapshot::test::snapshot_cases]=24 [snapshot::test::flipped_bytes_never_panic]=2 [snapshot::test::resealed_forgeries_never_panic]=4 [snapshot::test::a_restore_keeps_addresses]=3)
summary="${GITHUB_STEP_SUMMARY:-/dev/stdout}"
work="$(mktemp -d)"
declare -A unit_of=() log_of=() began=() whole=()
trap 'rm -rf "$work"' EXIT
trap 'kill "${!unit_of[@]}" 2> /dev/null; exit 1' INT TERM

mkdir -p miri && touch "$table"
make -s miri-setup >&2 && make -s miri-list MIRI_SETUP= SUITE="$SUITE" > "$work/tests" && [ -s "$work/tests" ] || { echo "::error::could not list the $SUITE tests"; exit 1; }
while read -r test; do
  n="${SLICES[$test]:-1}"
  if (( n == 1 )); then echo "$test"; else for (( k = 0; k < n; k++ )); do echo "$test@$k/$n"; done; fi
done < "$work/tests" > "$work/listed"

# The table holds the last row of each, the round it ran in, how it ended and its seconds, a test the suite no longer holds left out.
awk -F'\t' 'FILENAME == ARGV[1] { last[$1] = $0; next } $0 in last { print last[$0] }' "$table" "$work/listed" > "$table.next" && mv "$table.next" "$table"
round="$(cat "miri/$SUITE.round" 2>/dev/null)"
round="${round:-1}"
# Passed, ignored or too long for a job is done for the round, a cut test and then a failed one run first.
pending() {
  awk -F'\t' -v round="$round" '
    FILENAME == ARGV[1] { if ($2 >= round) state[$1] = $3; next }
    state[$0] ~ /^(passed|ignored|too long)$/ { next }
    state[$0] == "cut" { print; next }
    state[$0] == "failed" { failed = failed $0 "\n"; next }
    { rest = rest $0 "\n" }
    END { printf "%s%s", failed, rest }' "$table" "$work/listed"
}
# Every test is done in this round, so the next one starts in the next mode under its own seed.
[ -n "$(pending)" ] || round=$(( round + 1 ))
echo "$round" > "miri/$SUITE.round"
mode=$(( (round - 1) % 3 ))
flags="-Zmiri-seed=$round${MODES[$mode]:+ ${MODES[$mode]}}"

failed=0
rows=""

# Waits for a test to end, prints its output and keeps its row, a test cut though it started with the job being too long for one.
reap() {
  local pid status unit seconds result
  wait -n -p pid
  status=$?
  unit="${unit_of[$pid]}"
  seconds=$(( $(date +%s) - ${began[$pid]} ))
  case "$status" in
    0) result="passed"; grep -q ' 1 ignored;' "${log_of[$pid]}" && result="ignored" ;;
    124 | 137) result="cut"; (( ${whole[$pid]} )) && result="too long" ;;
    *) result="failed" ;;
  esac
  echo "::group::$unit, $result in ${seconds}s"
  cat "${log_of[$pid]}"
  echo "::endgroup::"
  case "$result" in
    failed) failed=1; echo "::error::$unit failed under Miri" ;;
    "too long") failed=1; echo "::error::$unit cannot finish in one job, so slice it or shrink it under Miri" ;;
  esac
  printf '%s\t%s\t%s\t%s\n' "$unit" "$round" "$result" "$seconds" >> "$table"
  rows+="| \`$unit\` | $result | ${seconds}s |"$'\n'
  unset "unit_of[$pid]"
}

i=0
for unit in $(pending); do
  (( ${#unit_of[@]} < workers )) || reap
  left=$(( deadline - $(date +%s) ))
  (( left > 0 )) || break
  test="${unit%@*}"
  slice="0/1"
  [ "$unit" = "$test" ] || slice="${unit#*@}"
  i=$(( i + 1 ))
  EDGE_SLICE="$slice" MIRIFLAGS="$flags" timeout --kill-after=30 "$left" make -s miri MIRI_SETUP= SUITE="$SUITE" CASE="$test" > "$work/$i.log" 2>&1 &
  unit_of[$!]="$unit"
  log_of[$!]="$work/$i.log"
  began[$!]=$(date +%s)
  whole[$!]=$(( i <= workers ))
done
while (( ${#unit_of[@]} )); do reap; done

total="$(wc -l < "$work/listed" | tr -d ' ')"
open="$(pending | wc -l | tr -d ' ')"
{
  echo "### Miri, $SUITE, round $round in ${NAMES[$mode]}"
  echo
  echo "$(( total - open )) of $total tests done in this round, \`MIRIFLAGS=\"$flags\" make miri SUITE=$SUITE CASE=<test>\` runs one again and \`EDGE_SLICE=k/n\` picks a slice."
  echo
  echo "| Test | Result | Time |"
  echo "|---|---|---|"
  printf '%s' "$rows"
} >> "$summary"

exit "$failed"
