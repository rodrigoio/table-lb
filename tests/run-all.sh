#!/bin/bash
# Runs the three correctness tests in sequence and prints a short report.
# Results: results/tests/<test>/summary.txt
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"

status=0
for test in switch-race switch-race-1rt no-lock equivalence; do
  echo "=== $test"
  if ! "$dir/$test.sh" | tail -1; then
    status=1
  fi
done
exit "$status"
