#!/usr/bin/env bash
set -uo pipefail

# Posts stdin to the testing api, exiting 2 on a 409 and 1 on any other failure.
api="${TESTING_API:-https://edgepython.com}"
path="$1"
body="$(cat)"

warn() { echo "::warning::testing api $path $1" >&2; exit 1; }

# An OIDC token lives for minutes, so every call asks for a fresh one.
[ -n "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ] || warn "skipped, no id-token permission or not on Actions"
token="$(curl -sSf --retry 3 -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=edgepython.com" | jq -r .value)" || warn "could not get the OIDC token"

answer="$(mktemp)"
trap 'rm -f "$answer"' EXIT
status="$(curl -sS --retry 3 -o "$answer" -w '%{http_code}' -X POST -H "Authorization: Bearer $token" -H 'Content-Type: application/json' --data-binary "$body" "$api$path")" || warn "unreachable"

case "$status" in
  2??) cat "$answer" ;;
  409) exit 2 ;;
  *) warn "answered $status $(head -c 300 "$answer")" ;;
esac
