#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."

WORKER="${WORKER:-local}"
LONG="${LONG:-false}"
report=make/report.sh
# Cases are claimed only while MARGIN seconds remain before the job ends.
deadline=$(( ${STARTED:-$(date +%s)} + ${MINUTES:-355} * 60 - ${MARGIN:-600} ))
# How long a worker with nothing to take waits before it asks again.
WAIT="${WAIT:-300}"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT
# A cancelled job lets go of what this run held, so another run takes its cases and the page stops counting it as going.
trap 'echo "{}" | bash "$report" /api/testing/miri/stop >/dev/null; exit 130' INT TERM

# Every test of every suite, so new tests join the queue and removed ones leave.
list() { for suite in vm snapshot modules parser lexer abi; do make -s miri-list SUITE="$suite" | jq -R --arg suite "$suite" '{suite: $suite, name: .}' || return 1; done; }
listed="$(list | jq -s '{cases: .}')" || { echo "::error::could not list the Miri tests"; exit 1; }
round="$(bash "$report" /api/testing/miri/cases <<< "$listed")" || { echo "::error::the testing api took no cases, so this worker has nothing to claim"; exit 1; }
echo "round $(jq -r '"\(.round), \(.mode), \(.done) of \(.total) cases done"' <<< "$round")"

failed=0
while :; do
  budget=$(( (deadline - $(date +%s)) * 1000 ))
  (( budget > 0 )) || break
  claim="$(jq -n --arg worker "$WORKER" --argjson long "$LONG" --argjson budget "$budget" '{worker: $worker, long: $long, budget_ms: $budget}' | bash "$report" /api/testing/miri/claim)" || { echo "::error::the testing api answered no claim"; failed=1; break; }
  suite="$(jq -r '.case.suite // empty' <<< "$claim")"
  # Nothing fits now, but a long case or a new round may still come before the deadline.
  if [ -z "$suite" ]; then
    (( budget > WAIT * 1000 )) || break
    sleep "$WAIT"
    continue
  fi
  name="$(jq -r .case.name <<< "$claim")"
  flags="$(jq -r .flags <<< "$claim")"
  limit=$(( $(jq -r .limit_ms <<< "$claim") / 1000 ))
  (( limit > 0 )) || break

  # One case with its round's flags, cut at its limit.
  echo "::group::$name, round $(jq -r .round <<< "$claim") in $(jq -r .mode <<< "$claim"), up to ${limit}s"
  began=$(date +%s)
  MIRIFLAGS="$flags" timeout --kill-after=30 "$limit" make -s miri SUITE="$suite" CASE="$name" 2>&1 | tee "$log"
  status=${PIPESTATUS[0]}
  ms=$(( ($(date +%s) - began) * 1000 ))
  echo "::endgroup::"

  case "$status" in
    0) outcome=passed ;;
    124 | 137) outcome=timeout ;;
    *) outcome=failed; failed=1; echo "::error::$name failed under Miri" ;;
  esac
  [ "$outcome" = passed ] && message="" || message="$(tail -c 2000 "$log")"
  jq -n --arg worker "$WORKER" --arg suite "$suite" --arg name "$name" --arg outcome "$outcome" --argjson ms "$ms" --arg message "$message" \
    '{worker: $worker, suite: $suite, name: $name, outcome: $outcome, ms: $ms} + (if $message == "" then {} else {message: $message} end)' \
    | bash "$report" /api/testing/miri/result >/dev/null
  # A 409 means the case is no longer ours.
  [ $? -eq 1 ] && echo "::warning::the result of $name did not reach the testing api"
done

exit "$failed"
