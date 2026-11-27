#!/bin/bash
# Runs ONE benchmark run: project × scenario × repetition, on a fresh database.
#
#   bench/run.sh <a|b> <scenario> [repetition]
#
# Environment:
#   CAMPAIGN        results/<CAMPAIGN>/ (default: today's date)
#   LABEL           variant label used in the output path (default: project letter)
#   <SCENARIO>_<VAR>  overrides VAR of bench/scenarios/<scenario>.env for this
#                   scenario only, e.g. C5_PRODUCER_CLIENTS=200000, C2_PRODUCER_RATE=8000
#   any knob of the project's docker-compose.yml (CLEANUP_MODE, LOCK_SLOTS,
#   PG_CPUS, PG_MEM, PG_SHARED_BUFFERS, PGBOUNCER_POOL_SIZE, ...)
#
# Scenario variables understood here (besides the compose knobs):
#   DURATION, WARMUP, READERS, BURST, BURST_AT, DRAIN_TIMEOUT
#   BURST_COUNT, BURST_EVERY  repeated bursts: BURST_COUNT bursts of
#                      BURST_DURATION seconds, the i-th at BURST_AT + (i-1) × BURST_EVERY
#   PRODUCER_REPLICAS  number of pgbench producer containers; PRODUCER_CLIENTS
#                      (and PRODUCER_RATE) are split among them. Each replica
#                      has its own source IP (≈60k connections each), its own
#                      order id range and its own controller slots.
#   PRODUCER_VIA       postgres (default) | pgbouncer
#
# Output: results/<CAMPAIGN>/<scenario>/<LABEL>/rep-<n>/
#   meta.env   parameters and timestamps of the run
#   raw/       pgbench logs, collector CSVs, docker stats, service logs (not versioned)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
project="${1:?usage: bench/run.sh <a|b> <scenario> [repetition]}"
scenario="${2:?usage: bench/run.sh <a|b> <scenario> [repetition]}"
rep="${3:-1}"
[[ "$project" == "a" || "$project" == "b" ]] || { echo "project must be a or b" >&2; exit 2; }

scenario_file="$ROOT/bench/scenarios/$scenario.env"
[[ -f "$scenario_file" ]] || { echo "unknown scenario: $scenario_file" >&2; exit 2; }
set -a
# shellcheck disable=SC1090
source "$scenario_file"
set +a

# Per-scenario overrides: <SCENARIO>_<VAR>=value → VAR=value
prefix="$(tr '[:lower:]-' '[:upper:]_' <<<"$scenario")_"
overrides=()
for name in $(compgen -e); do
  [[ "$name" == "$prefix"?* && -n "${!name}" ]] || continue
  var="${name#"$prefix"}"
  printf -v "$var" '%s' "${!name}"
  export "${var?}"
  overrides+=("$var")
done

CAMPAIGN="${CAMPAIGN:-$(date -u +%Y%m%d)}"
LABEL="${LABEL:-$project}"
run_dir="$ROOT/results/$CAMPAIGN/$scenario/$LABEL/rep-$rep"
raw="$run_dir/raw"
rm -rf "$run_dir"
mkdir -p "$raw"

export RESULTS_DIR="$run_dir"
export HOST_UID="$(id -u)" HOST_GID="$(id -g)"
export PGB_LOG=1

# Where the producers connect.
PRODUCER_VIA="${PRODUCER_VIA:-postgres}"
case "$PRODUCER_VIA" in
  postgres)  export PRODUCER_HOST=postgres PRODUCER_PORT=5432 ;;
  pgbouncer) export PRODUCER_HOST=pgbouncer PRODUCER_PORT=6432 ;;
  *) echo "PRODUCER_VIA must be postgres or pgbouncer" >&2; exit 2 ;;
esac

# Server connections needed: producers (or the pooler's pools), readers,
# consumers, collector, psql, plus a margin. Never below the config default.
if [[ -z "${PG_MAX_CONNECTIONS:-}" ]]; then
  if [[ "$PRODUCER_VIA" == "pgbouncer" ]]; then
    producer_conns=$(( ${PGBOUNCER_PROCESSES:-4} * ${PGBOUNCER_POOL_SIZE:-16} ))
  else
    producer_conns=$(( ${PRODUCER_CLIENTS:-16} + ${BURST_CLIENTS:-0} ))
  fi
  needed=$(( producer_conns + ${READER_CLIENTS:-4} + ${WORKERS:-4} + 40 ))
  export PG_MAX_CONNECTIONS=$(( needed > 100 ? needed : 100 ))
fi

compose() {
  docker compose -f "$ROOT/project-$project/docker-compose.yml" -p "tlb-bench-$project" "$@"
}
sql() {
  compose exec -T postgres psql -U tlb -d tlb -qtAX -c "$1"
}
now_ms() { date +%s%3N; }
log() { echo "[$(date -u +%H:%M:%S)] $project/$scenario/$LABEL/rep-$rep: $*"; }

backlog() {
  if [[ "$project" == "a" ]]; then
    sql "SELECT COUNT(*) FROM inbox WHERE state = 'P'"
  else
    sql "SELECT (SELECT COUNT(*) FROM buffer_0) + (SELECT COUNT(*) FROM buffer_1)"
  fi
}

stats_pid=""
cleanup() {
  [[ -n "$stats_pid" ]] && kill "$stats_pid" 2>/dev/null || true
  compose --profile load --profile burst down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

log "preparing (fresh database)"
compose --profile load --profile burst down -v --remove-orphans >/dev/null 2>&1 || true
compose --profile load --profile burst build --quiet >/dev/null
compose up -d >/dev/null 2>&1
# Same starting point for every run: vacuumed/analyzed seed, clean checkpoint.
sql "VACUUM (ANALYZE) orders" >/dev/null
sql "CHECKPOINT" >/dev/null
sleep "${SETTLE_SECONDS:-5}"

# CPU / memory per container while the run is active
(
  echo "ts_ms;name;cpu_perc;mem_usage"
  while true; do
    ts="$(now_ms)"
    docker stats --no-stream --format '{{.Name}};{{.CPUPerc}};{{.MemUsage}}' 2>/dev/null \
      | grep "tlb-bench-$project" | sed "s/^/$ts;/" || true
    sleep 1
  done
) >"$raw/docker_stats.csv" &
stats_pid=$!

# Producer replicas: split clients (and rate) and give each replica its own
# order id range (BASE + i × 10^13: room for 10^5 clients × 10^8 ids each)
# and its own controller slots (SLOT_OFFSET + clients before it).
replicas="${PRODUCER_REPLICAS:-1}"
clients="${PRODUCER_CLIENTS:-16}"
(( replicas >= 1 && replicas <= clients )) || { echo "PRODUCER_REPLICAS must be 1..PRODUCER_CLIENTS" >&2; exit 2; }
start_producers() {
  local i n offset=0 base role rate
  for (( i = 1; i <= replicas; i++ )); do
    n=$(( clients / replicas + (i <= clients % replicas ? 1 : 0) ))
    base=$(( ${BASE:-0} + (i - 1) * 10000000000000 ))
    role=producer
    (( replicas > 1 )) && role="producer-$i"
    rate=""
    [[ -n "${PRODUCER_RATE:-}" ]] && rate="$(awk -v r="$PRODUCER_RATE" -v n="$n" -v c="$clients" 'BEGIN { printf "%.3f", r * n / c }')"
    compose run --rm -T \
      -e PGB_ROLE="$role" -e PGB_CLIENTS="$n" -e PGB_RATE="$rate" \
      -e BASE="$base" -e SLOT_OFFSET=$(( ${SLOT_OFFSET:-0} + offset )) \
      producer >"$raw/$role.out" 2>&1 &
    pids+=($!)
    offset=$(( offset + n ))
  done
}

load_start_ms="$(now_ms)"
log "load started: duration=${DURATION}s clients=$clients replicas=$replicas via=$PRODUCER_VIA rate=${PRODUCER_RATE:-max} readers=${READERS:-0} burst=${BURST:-0}"
pids=()
start_producers
if [[ "${READERS:-0}" == "1" ]]; then
  compose run --rm -T reader >"$raw/reader.out" 2>&1 &
  pids+=($!)
fi
if [[ "${BURST:-0}" == "1" ]]; then
  # BURST_COUNT bursts, BURST_EVERY seconds apart (from load start), each with
  # its own order id range; a single burst keeps the historic file names.
  bursts="${BURST_COUNT:-1}"
  (
    for (( i = 1; i <= bursts; i++ )); do
      at=$(( ${BURST_AT:-120} + (i - 1) * ${BURST_EVERY:-0} ))
      elapsed=$(( ($(now_ms) - load_start_ms) / 1000 ))
      (( at > elapsed )) && sleep $(( at - elapsed ))
      role=burst
      (( bursts > 1 )) && role="burst-$i"
      compose run --rm -T -e PGB_ROLE="$role" \
        -e BASE=$(( ${BURST_BASE:-100000000000000000} + (i - 1) * 10000000000000 )) \
        burst >"$raw/$role.out" 2>&1
    done
  ) &
  pids+=($!)
fi
status=0
for pid in "${pids[@]}"; do
  wait "$pid" || status=1
done
load_end_ms="$(now_ms)"
[[ "$status" == "0" ]] || log "WARNING: a load generator exited with an error (see raw/*.out)"

log "load finished, waiting for the consumer to drain the backlog"
drain_deadline=$(( $(date +%s) + ${DRAIN_TIMEOUT:-600} ))
remaining="$(backlog)"
while [[ "$remaining" != "0" && "$(date +%s)" -lt "$drain_deadline" ]]; do
  sleep 1
  remaining="$(backlog)"
done
drained_ms="$(now_ms)"
log "backlog left: $remaining (drain took $(( (drained_ms - load_end_ms) / 1000 ))s)"
sleep 3   # one more collector sample at rest

kill "$stats_pid" 2>/dev/null || true
stats_pid=""
compose stop -t 60 collector >/dev/null 2>&1
compose stop -t 60 consumer >/dev/null 2>&1
compose logs --no-color consumer >"$raw/consumer.log" 2>&1 || true
compose logs --no-color postgres >"$raw/postgres.log" 2>&1 || true
compose logs --no-color pgbouncer >"$raw/pgbouncer.log" 2>&1 || true

{
  echo "project=$project"
  echo "label=$LABEL"
  echo "scenario=$scenario"
  echo "rep=$rep"
  echo "git_rev=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo "load_start_ms=$load_start_ms"
  echo "load_end_ms=$load_end_ms"
  echo "drained_ms=$drained_ms"
  echo "backlog_left=$remaining"
  echo "load_status=$status"
  grep -vE '^\s*(#|$)' "$scenario_file"
  # overrides and effective settings come last: the report keeps the last value
  for var in "${overrides[@]}" CLEANUP_MODE WORKERS BATCH_SIZE LOCK_SLOTS POLL_MS MIN_CYCLE_MS PRODUCER_SCRIPT \
             PRODUCER_VIA PRODUCER_REPLICAS QUERY_MODE PG_CPUS PG_MEM PG_SHARED_BUFFERS \
             PG_EFFECTIVE_CACHE_SIZE PG_MAX_CONNECTIONS PGBOUNCER_PROCESSES PGBOUNCER_POOL_SIZE; do
    [[ -n "${!var:-}" ]] && echo "$var=${!var}"
  done
} >"$run_dir/meta.env"

log "done → ${run_dir#$ROOT/}"
