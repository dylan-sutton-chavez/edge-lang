#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

# The testing api body from every fuzzer_stats, the corpus from the largest queue.
shopt -s nullglob
awk -F' *: *' '
  $1 == "execs_done" { execs += $2 }
  $1 == "corpus_count" && $2 > corpus { corpus = $2 }
  $1 == "saved_crashes" { crashes += $2 }
  $1 == "saved_hangs" { hangs += $2 }
  END { printf "%d %d %d %d\n", execs, corpus, crashes, hangs }
' out/*/fuzzer_stats /dev/null | {
  read -r execs corpus crashes hangs
  jq -n --arg strategy "${STRATEGY:-local}" --arg state "${1:-running}" --argjson execs "$execs" --argjson corpus "$corpus" --argjson crashes "$crashes" --argjson hangs "$hangs" \
    '{strategy: $strategy, state: $state, execs: $execs, corpus: $corpus, crashes: $crashes, hangs: $hangs}'
}
