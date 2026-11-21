#!/usr/bin/env bash
# load.sh refuses a RATE or TOTAL that is not a whole number before it does anything:
# both reach shell arithmetic, which would evaluate an expression (and run a command
# substitution in an array subscript). Each probe must exit 2 with the reason, and the
# command inside it must never run.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
[ $# -eq 0 ] || { echo "test-load-args.sh: takes no arguments (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/.."

work="$(mktemp -d "${TMPDIR:-/tmp}/load-args.XXXXXX")"
trap 'rm -rf "$work"' EXIT
marker="$work/ran"
fail=0
probe() { # name value variable expected-message
  local status=0 out
  out="$(env -u BOOKING_TEST_DATABASE_URL "$3=$2" SCENARIO=hold-contention scripts/load.sh 2>&1 </dev/null)" || status=$?
  if [ "$status" != 2 ] || ! grep -q -- "$4" <<<"$out"; then
    echo "error: load.sh with $1 exited $status: $out" >&2
    fail=1
  fi
  if [ -e "$marker" ]; then
    echo "error: load.sh with $1 ran the command inside it" >&2
    fail=1
  fi
}
probe "an injected RATE" "SECONDS[\$(touch $marker)]" RATE "RATE must be a positive whole number"
probe "an expression RATE" "1+1" RATE "RATE must be a positive whole number"
probe "a zero RATE" "0" RATE "RATE must be a positive whole number"
probe "an injected TOTAL" "SECONDS[\$(touch $marker)]" TOTAL "TOTAL must be a whole number"
probe "a negative TOTAL" "-5" TOTAL "TOTAL must be a whole number"
(( fail )) && exit 1
echo "load args ok: RATE and TOTAL are digits only, refused before any work"
