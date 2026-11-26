#!/bin/bash
# Correctness test 1 (project B) — switch race.
#
# 32 producers hold the controller lock (FOR SHARE) for a random 0-5 ms before
# inserting, while the consumer switches as often as it can (POLL_MS=1).
# Every produced event must be consumed exactly once.
#
# Usage: tests/switch-race.sh            (result in results/tests/switch-race/)
source "$(dirname "$0")/lib.sh"

NAME="${NAME:-switch-race}"
SCRIPT="${SCRIPT:-/scripts/tests/producer-b-delayed.sql}"
EXPECT_LOSS="${EXPECT_LOSS:-0}"

export PRODUCER_SCRIPT="$SCRIPT"
export PRODUCER_CLIENTS="${PRODUCER_CLIENTS:-32}"
# One pgbench thread per client. pgbench prepares statements synchronously and
# multiplexes the clients of a thread: without the lock (no-lock test) a stale
# producer preparing an INSERT into the buffer being truncated blocks its whole
# thread, including the client whose open transaction the TRUNCATE waits for —
# a client-side deadlock that the server cannot detect. Separate threads model
# independent producers.
export PRODUCER_THREADS="${PRODUCER_THREADS:-$PRODUCER_CLIENTS}"
export TRANSACTIONS="${TRANSACTIONS:-3000}" WINDOW="${WINDOW:-100}"
export SLEEP_MAX_MS="${SLEEP_MAX_MS:-5}" POLL_MS="${POLL_MS:-1}"
export LOCK_SLOTS="${LOCK_SLOTS:-64}"

out="$TEST_RESULTS/$NAME"
mkdir -p "$out/raw"
export RESULTS_DIR="$out"
expected=$((PRODUCER_CLIENTS * TRANSACTIONS))

{
  echo "# $NAME"
  echo "date=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "project=b script=$SCRIPT clients=$PRODUCER_CLIENTS transactions=$TRANSACTIONS window=$WINDOW"
  echo "sleep_max_ms=$SLEEP_MAX_MS poll_ms=$POLL_MS lock_slots=$LOCK_SLOTS expect_loss=$EXPECT_LOSS"
  echo
} >"$out/summary.txt"

stack_up b
echo "## pgbench" >>"$out/summary.txt"
produce b | grep -E 'processed|failed|latency average|tps' >>"$out/summary.txt"
wait_consumed b "$expected" 20

{
  echo
  echo "## consumer"
  echo "switches=$(sql b 'SELECT MAX(generation) FROM buffer_control')"
  sql b "SELECT 'cycles=' || COUNT(*) || ' rows_read=' || SUM(rows_read)
              || ' switch_wait_ms_avg=' || ROUND(CAST(AVG(switch_wait_ms) AS NUMERIC), 2)
              || ' switch_wait_ms_max=' || ROUND(CAST(MAX(switch_wait_ms) AS NUMERIC), 2)
         FROM batch_log"
  echo "rows_left_in_buffers=$(sql b 'SELECT (SELECT COUNT(*) FROM buffer_0) + (SELECT COUNT(*) FROM buffer_1)')"
  echo
  echo "## verify (consumed)"
} >>"$out/summary.txt"

extra=()
[[ "$EXPECT_LOSS" == "1" ]] && extra=(-e EXPECT_LOSS=1)
output="$(verify b consumed "${extra[@]}")"
echo "$output" >>"$out/summary.txt"
stack_down b

cat "$out/summary.txt"
result_of "$NAME" "$output"
