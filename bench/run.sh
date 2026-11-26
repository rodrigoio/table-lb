#!/bin/bash
# Runs ONE benchmark run: project × scenario × repetition, on a fresh database.
#
#   bench/run.sh <a|b> <scenario> [repetition]
#
# Environment:
#   CAMPAIGN   results/<CAMPAIGN>/ (default: today's date)
#   LABEL      variant label used in the output path (default: project letter)
#   any knob of the project's docker-compose.yml (CLEANUP_MODE, LOCK_SLOTS, ...)
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

CAMPAIGN="${CAMPAIGN:-$(date -u +%Y%m%d)}"
LABEL="${LABEL:-$project}"
run_dir="$ROOT/results/$CAMPAIGN/$scenario/$LABEL/rep-$rep"
raw="$run_dir/raw"
rm -rf "$run_dir"
mkdir -p "$raw"

export RESULTS_DIR="$run_dir"
export HOST_UID="$(id -u)" HOST_GID="$(id -g)"
export PGB_LOG=1

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

load_start_ms="$(now_ms)"
log "load started: duration=${DURATION}s rate=${PRODUCER_RATE:-max} readers=${READERS:-0} burst=${BURST:-0}"
pids=()
compose run --rm -T producer >"$raw/producer.out" 2>&1 &
pids+=($!)
if [[ "${READERS:-0}" == "1" ]]; then
  compose run --rm -T reader >"$raw/reader.out" 2>&1 &
  pids+=($!)
fi
if [[ "${BURST:-0}" == "1" ]]; then
  (sleep "${BURST_AT:-120}"; compose run --rm -T burst >"$raw/burst.out" 2>&1) &
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
  for var in CLEANUP_MODE WORKERS BATCH_SIZE LOCK_SLOTS POLL_MS; do
    [[ -n "${!var:-}" ]] && echo "$var=${!var}"
  done
} >"$run_dir/meta.env"

log "done → ${run_dir#$ROOT/}"
