#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

TIMEOUT_MS="${TIMEOUT_MS:-5000}"
bin=target/release/afl-pipeline

# The old corpus and every queue, deduplicated by hash.
rm -rf union && mkdir -p union corpus logs
shopt -s nullglob
for input in corpus/* out/*/queue/id:*; do
  [ -f "$input" ] && cp "$input" "union/$(shasum "$input" | cut -c1-16)"
done
[ -n "$(ls -A union)" ] || { echo "nothing to carry"; rm -rf union; exit 0; }
[ -x "$bin" ] || cargo afl build --release

rm -rf corpus.next
cargo afl cmin -t "$TIMEOUT_MS" -i union -o corpus.next -- "$bin" >logs/cmin.log 2>&1 || { tail -20 logs/cmin.log; rm -rf union corpus.next; exit 1; }
# An empty result saw no coverage, so the union is kept whole.
[ -n "$(ls -A corpus.next)" ] || { echo "afl-cmin kept nothing, carrying every input instead"; rm -rf corpus.next; mv union corpus.next; }
rm -rf corpus union && mv corpus.next corpus
echo "corpus: $(ls corpus | wc -l | tr -d ' ') inputs"
