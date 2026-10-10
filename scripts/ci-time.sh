#!/usr/bin/env bash
# CI diagnostics only: run the command unchanged and preserve its exit status.
set -euo pipefail
if [[ $# -lt 2 ]]; then
  printf '%s\n' 'Usage: bash scripts/ci-time.sh LABEL COMMAND [ARGUMENTS...]' >&2
  exit 2
fi
label=$1
shift
metrics=$(mktemp)
trap 'rm -f "$metrics"' EXIT
status=0
TIMEFORMAT=$'Wall seconds: %R\nUser CPU seconds: %U\nSystem CPU seconds: %S'
# Keep the command's stderr in the job log; capture only Bash's timing output.
exec 3>&2
{ time "$@" 2>&3; } 2>"$metrics" || status=$?
printf 'Exit status: %s\n' "$status" >> "$metrics"
cat "$metrics"
if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
  {
    printf '\n### %s\n\n```text\n' "$label"
    cat "$metrics"
    printf '```\n'
  } >> "$GITHUB_STEP_SUMMARY"
fi
exit "$status"
