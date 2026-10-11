#!/usr/bin/env bash
set -euo pipefail

# The campaign of the UTC day, as lines for GITHUB_ENV.
weekday="$(date -u +%u)"
hour="$(date -u +%H)"
day="$(date -u +%j)"

# The first block of a Sunday starts clean from the seeds.
if [ "$weekday" = 7 ] && (( 10#$hour < 6 )); then
  printf 'STRATEGY=clean\nSCHEDULES=explore fast rare coe\nSEEDS=in\n'
  exit 0
fi

case $(( 10#$day % 4 )) in
  0) printf 'STRATEGY=explore\nSCHEDULES=explore fast rare coe\n' ;;
  1) printf 'STRATEGY=rare\nSCHEDULES=rare explore exploit fast\n' ;;
  2) printf 'STRATEGY=mopt\nSCHEDULES=fast coe explore rare\nMUTATE=-L 0\n' ;;
  3) printf 'STRATEGY=seek\nSCHEDULES=seek mmopt lin quad\n' ;;
esac
