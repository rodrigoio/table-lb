#!/bin/bash
# Turns environment variables into a pgbench command line.
#
#   PGB_ROLE          producer | reader | burst | ... (prefix of output files)
#   PGB_SCRIPTS       space separated list of scripts (script@weight allowed)
#   PGB_CLIENTS       number of client connections
#   PGB_THREADS       pgbench worker threads
#   PGB_DURATION      run time in seconds (ignored when PGB_TRANSACTIONS is set)
#   PGB_TRANSACTIONS  fixed number of transactions per client (-t)
#   PGB_RATE          target total rate in tps (empty = as fast as possible)
#   PGB_LOG           1 = write per-transaction logs (sampled)
#   PGB_SAMPLING      sampling rate for per-transaction logs
#   PGB_MODE          query protocol: prepared (default) | extended | simple.
#                     Behind a transaction pooler use extended: pgbench
#                     prepares statements synchronously, blocking every client
#                     of the thread while the pooler waits for a free server
#                     connection, and those connections are held by clients of
#                     the same blocked thread in the middle of a transaction.
#   WINDOW, BASE, LOCK_SLOTS, SLOT_OFFSET, SEED_ORDERS, SLEEP_MAX_MS,
#   PAYLOAD_SIZE      variables passed to the scripts with -D
set -euo pipefail

ROLE="${PGB_ROLE:-producer}"
OUT="/results/raw"
mkdir -p "$OUT"

PAYLOAD_SIZE="${PAYLOAD_SIZE:-256}"
PAYLOAD="$(head -c "$PAYLOAD_SIZE" /dev/zero | tr '\0' 'x')"

args=(
  -h "${PGHOST:-postgres}" -U "${PGUSER:-tlb}" -n -M "${PGB_MODE:-prepared}"
  -c "${PGB_CLIENTS:-8}" -j "${PGB_THREADS:-2}" -P "${PGB_PROGRESS:-5}"
  -D k=0
  -D window="${WINDOW:-50000}"
  -D base="${BASE:-0}"
  -D lock_slots="${LOCK_SLOTS:-64}"
  -D slot_offset="${SLOT_OFFSET:-0}"
  -D seed_orders="${SEED_ORDERS:-1000000}"
  -D sleep_max_ms="${SLEEP_MAX_MS:-0}"
  -D hot=0
  -D payload="$PAYLOAD"
)

if [[ -n "${PGB_TRANSACTIONS:-}" ]]; then
  args+=(-t "$PGB_TRANSACTIONS")
else
  args+=(-T "${PGB_DURATION:-60}")
fi

if [[ -n "${PGB_RATE:-}" ]]; then
  args+=(-R "$PGB_RATE")
fi

if [[ "${PGB_LOG:-1}" == "1" ]]; then
  args+=(-l --log-prefix="$OUT/$ROLE" --sampling-rate="${PGB_SAMPLING:-0.05}")
fi

for script in ${PGB_SCRIPTS:?PGB_SCRIPTS is required}; do
  args+=(-f "$script")
done

echo "pgbench ${args[*]//$PAYLOAD/<payload:$PAYLOAD_SIZE>} ${PGDATABASE:-tlb}" > "$OUT/$ROLE.cmd"
pgbench "${args[@]}" "${PGDATABASE:-tlb}" 2>&1 | tee "$OUT/$ROLE.summary.txt"
