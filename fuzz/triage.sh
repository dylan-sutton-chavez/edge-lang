#!/usr/bin/env bash
# Counts saved crashes by panic site, since one bug saves many inputs, and replays the saved hangs.
set -euo pipefail
cd "$(dirname "$0")"

bin=target/release/afl-pipeline
[ -d out ] || { echo "no findings yet, run make fuzz first"; exit 0; }
find out -type f -path '*crashes*' ! -name README.txt | while IFS= read -r f; do
    "$bin" < "$f" 2>&1 | grep -oE 'panicked at [^:]+:[0-9]+' || true
done | sort | uniq -c | sort -rn
# A saved hang counts only if it still runs past the timeout alone, since persistent mode on macOS also saves stalls of AFL itself.
limit=$(( ${TIMEOUT_MS:-5000} / 1000 + 1 ))
saved=0 slow=0
while IFS= read -r f; do
    saved=$((saved + 1))
    rc=0; timeout "$limit" "$bin" < "$f" > /dev/null 2>&1 || rc=$?
    if (( rc == 124 )); then slow=$((slow + 1)); echo "slow: $f"; fi
done < <(find out -type f -path '*hangs*' ! -name README.txt)
echo "hangs: $saved saved, $slow still slow alone"
