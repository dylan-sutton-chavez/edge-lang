#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

TIMEOUT_MS="${TIMEOUT_MS:-5000}"
CMIN_SECS="${CMIN_SECS:-600}" # afl-cmin gets this long, then the new-coverage inputs are carried unminimized.
bin=target/release/afl-pipeline

mkdir -p corpus logs
shopt -s nullglob
inputs=(corpus/* out/*/queue/id:*)
(( ${#inputs[@]} )) || { echo "nothing to carry"; exit 0; }
[ -x "$bin" ] || cargo afl build --release

# afl-cmin reads every directory itself and drops duplicates by hash.
dirs=(-i corpus)
for q in out/*/queue; do dirs+=(-i "$q"); done
# The bound needs timeout, which macOS has only as gtimeout from coreutils.
bounded() {
  local t
  t="$(command -v timeout || command -v gtimeout || true)"
  if [ -n "$t" ]; then "$t" -k 30 "$CMIN_SECS" "$@"; else "$@"; fi
}

rm -rf corpus.next
if ! bounded cargo afl cmin -T all -t "$TIMEOUT_MS" "${dirs[@]}" -o corpus.next -- "$bin" >logs/cmin.log 2>&1; then
  tail -5 logs/cmin.log
  echo "afl-cmin did not finish, carrying the old corpus and the new coverage instead"
  rm -rf corpus.next && mkdir corpus.next
  cp corpus/* corpus.next/ 2>/dev/null || true
  for q in out/*/queue; do
    n="$(basename "$(dirname "$q")")"
    for f in "$q"/id:*+cov*; do cp "$f" "corpus.next/$n-$(basename "$f")"; done
  done
fi
# An empty result saw no coverage, so the old corpus stays.
[ -n "$(ls -A corpus.next)" ] || { echo "nothing carried, keeping the old corpus"; rm -rf corpus.next; exit 0; }
rm -rf corpus && mv corpus.next corpus
echo "corpus: $(find corpus -type f | wc -l | tr -d ' ') inputs"
