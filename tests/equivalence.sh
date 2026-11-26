#!/bin/bash
# Correctness test 3 — A × B equivalence.
#
# The same deterministic event set (pgbench lifecycle generator with a fixed
# transaction count per client) is ingested by project A and by project B.
# For each project: every event must be consumed exactly once and every order
# must end in its highest produced state. Both projects must end with the same
# state hash.
source "$(dirname "$0")/lib.sh"

NAME=equivalence
export PRODUCER_CLIENTS="${PRODUCER_CLIENTS:-8}" PRODUCER_THREADS=2
export TRANSACTIONS="${TRANSACTIONS:-4000}" WINDOW="${WINDOW:-300}"
export POLL_MS=100

out="$TEST_RESULTS/$NAME"
mkdir -p "$out/raw"
export RESULTS_DIR="$out"
expected=$((PRODUCER_CLIENTS * TRANSACTIONS))

{
  echo "# $NAME"
  echo "date=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "clients=$PRODUCER_CLIENTS transactions=$TRANSACTIONS window=$WINDOW"
} >"$out/summary.txt"

status=0
declare -A hashes
for project in a b; do
  stack_up "$project"
  {
    echo
    echo "## project $project — pgbench"
  } >>"$out/summary.txt"
  produce "$project" | grep -E 'processed|failed|latency average|tps' >>"$out/summary.txt"
  wait_consumed "$project" "$expected" 20

  consumed="$(verify "$project" consumed)"
  state="$(verify "$project" state)"
  {
    echo "## project $project — verify (consumed)"
    echo "$consumed"
    echo "## project $project — verify (state)"
    echo "$state"
  } >>"$out/summary.txt"
  result_of "project $project consumed" "$consumed" || status=1
  result_of "project $project state" "$state" || status=1
  hashes[$project]="$(grep '^state_hash=' <<<"$state" | cut -d= -f2)"
  stack_down "$project"
done

{
  echo
  echo "## comparison"
  echo "state_hash_a=${hashes[a]}"
  echo "state_hash_b=${hashes[b]}"
} >>"$out/summary.txt"

if [[ -n "${hashes[a]}" && "${hashes[a]}" == "${hashes[b]}" ]]; then
  echo "result=PASS" >>"$out/summary.txt"
  echo "$NAME (same final state in A and B): PASS"
else
  echo "result=FAIL" >>"$out/summary.txt"
  echo "$NAME (same final state in A and B): FAIL"
  status=1
fi
exit "$status"
