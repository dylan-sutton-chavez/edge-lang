#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."

# One suite walked a test at a time, what ran kept in a table under miri/ that the Actions cache carries to the next run.
SUITE="${SUITE:?pass SUITE, one of vm snapshot modules parser lexer abi}"
table="miri/$SUITE.tsv"
# Tests start only while MARGIN seconds remain before the job ends.
deadline=$(( ${STARTED:-$(date +%s)} + ${MINUTES:-355} * 60 - ${MARGIN:-600} ))
MODES=("" "-Zmiri-tree-borrows" "-Zmiri-strict-provenance -Zmiri-symbolic-alignment-check")
NAMES=("Stacked Borrows" "Tree Borrows" "Strict provenance")
summary="${GITHUB_STEP_SUMMARY:-/dev/stdout}"
log="$(mktemp)"
listed="$(mktemp)"
trap 'rm -f "$log" "$listed"' EXIT

mkdir -p miri && touch "$table"
make -s miri-list SUITE="$SUITE" > "$listed" || { echo "::error::could not list the $SUITE tests"; exit 1; }

# The table holds a test, the round it last ran in, how it ended and its seconds, a test the suite no longer holds left out.
awk -F'\t' 'FILENAME == ARGV[1] { keep[$0] = 1; next } $1 in keep' "$listed" "$table" > "$table.next" && mv "$table.next" "$table"
round="$(cat "miri/$SUITE.round" 2>/dev/null)"
round="${round:-1}"
pending() { awk -F'\t' -v round="$1" 'FILENAME == ARGV[1] { if ($2 >= round) done[$1] = 1; next } !($0 in done)' "$table" "$listed"; }
# Every test ran in this round, so the next one starts in the next mode under its own seed.
[ -n "$(pending "$round")" ] || round=$(( round + 1 ))
echo "$round" > "miri/$SUITE.round"
mode=$(( (round - 1) % 3 ))
flags="-Zmiri-seed=$round ${MODES[$mode]}"

failed=0
rows=""
for test in $(pending "$round"); do
  left=$(( deadline - $(date +%s) ))
  (( left > 0 )) || break
  began=$(date +%s)
  echo "::group::$test"
  MIRIFLAGS="$flags" timeout --kill-after=30 "$left" make -s miri SUITE="$SUITE" CASE="$test" 2>&1 | tee "$log"
  status=${PIPESTATUS[0]}
  echo "::endgroup::"
  case "$status" in
    0) result=passed ;;
    124 | 137) result=timeout ;;
    *) result=failed; failed=1; echo "::error::$test failed under Miri" ;;
  esac
  seconds=$(( $(date +%s) - began ))
  { awk -F'\t' -v test="$test" '$1 != test' "$table"; printf '%s\t%s\t%s\t%s\n' "$test" "$round" "$result" "$seconds"; } > "$table.next" && mv "$table.next" "$table"
  rows+="| \`$test\` | $result | ${seconds}s |"$'\n'
done

done_now="$(awk -F'\t' -v round="$round" '$2 >= round' "$table" | wc -l | tr -d ' ')"
total="$(wc -l < "$listed" | tr -d ' ')"
{
  echo "### Miri, $SUITE, round $round in ${NAMES[$mode]}"
  echo
  echo "$done_now of $total tests done in this round."
  echo
  echo "| Test | Result | Time |"
  echo "|---|---|---|"
  printf '%s' "$rows"
} >> "$summary"

exit "$failed"
