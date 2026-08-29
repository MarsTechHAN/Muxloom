#!/usr/bin/env bash
#
# Run one gate and, when it fails, say why somewhere that can be read without
# admin rights on this repository.
#
# A red job says only "Process completed with exit code 1" to anybody who is
# not an administrator here: the job log is behind `actions:read`, and the
# annotation the runner leaves by itself carries no reason. Whoever has to fix
# a break — often an agent, on another machine, with no token at all — is left
# guessing which of three platforms failed on which test. Annotations are
# public, so the gate repeats its own failure as one.
#
# Usage: bash .github/gate.sh <title> <command> [args...]
set -uo pipefail

title=$1
shift

log=$(mktemp)
status=0
"$@" 2>&1 | tee "$log" || status=$?
if [ "$status" -eq 0 ]; then
  rm -f "$log"
  exit 0
fi

# The interesting part of a red run is the error and the few lines under it —
# a clippy lint with its file and line, a test panic with its assertion. When
# nothing matches, the tail is still better than nothing.
report=$(grep -E -A 4 '^error|panicked at|FAILED' "$log" | head -n 80)
if [ -z "$report" ]; then
  report=$(tail -n 60 "$log")
fi

# Workflow commands are one line: newlines, carriage returns and percent signs
# travel encoded. Plain bash so this reads the same on the Windows runner.
encoded=""
while IFS= read -r line; do
  line=${line//%/%25}
  line=${line//$'\r'/}
  encoded="${encoded}${line}%0A"
done <<<"$report"

printf '::error title=%s::%s\n' "$title" "$encoded"
rm -f "$log"
exit "$status"
