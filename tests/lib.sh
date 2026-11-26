#!/bin/bash
# Shared helpers for the correctness tests. Each test runs the project's own
# docker-compose file under a separate compose project name (tlb-test-<p>),
# so it never touches a benchmark stack.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_RESULTS="${TEST_RESULTS:-$ROOT/results/tests}"
export HOST_UID="$(id -u)" HOST_GID="$(id -g)"

# Small seed: the tests are about correctness, not volume.
export SEED_ORDERS="${SEED_ORDERS:-10000}"
export AUDIT=1
export PGB_LOG=0

compose() {
  local project="$1"
  shift
  docker compose -f "$ROOT/project-$project/docker-compose.yml" -p "tlb-test-$project" "$@"
}

sql() {
  local project="$1"
  shift
  compose "$project" exec -T postgres psql -U tlb -d tlb -qtAX -c "$1"
}

stack_up() {
  local project="$1"
  compose "$project" --profile load --profile burst down -v --remove-orphans >/dev/null 2>&1 || true
  compose "$project" build --quiet >/dev/null
  compose "$project" up -d consumer >/dev/null 2>&1
}

stack_down() {
  local project="$1"
  compose "$project" --profile load --profile burst down -v --remove-orphans >/dev/null 2>&1 || true
}

# Runs the producer service with the current environment and prints the
# pgbench summary (without per-interval progress lines).
produce() {
  local project="$1"
  compose "$project" run --rm -T producer 2>&1 | grep -vE '^(progress|\s*Container)' || true
}

# Waits until audit_consumed holds `expected` rows or stops changing.
wait_consumed() {
  local project="$1" expected="$2" stable_for="${3:-20}"
  local last=-1 still=0 now
  while true; do
    now="$(sql "$project" 'SELECT COUNT(*) FROM audit_consumed')"
    if [[ "$now" -ge "$expected" ]]; then
      break
    fi
    if [[ "$now" == "$last" ]]; then
      still=$((still + 1))
      [[ "$still" -ge "$stable_for" ]] && break
    else
      still=0
      last="$now"
    fi
    sleep 1
  done
  # let an in-flight drain finish before verifying
  sleep 2
}

# verify <project> <mode> [extra docker run args...]
verify() {
  local project="$1" mode="$2"
  shift 2
  compose "$project" run --rm --no-deps -T --entrypoint /usr/local/bin/verify \
    -e VERIFY_MODE="$mode" -e CLIENTS="$PRODUCER_CLIENTS" -e TRANSACTIONS="$TRANSACTIONS" \
    -e WINDOW="$WINDOW" -e BASE="${BASE:-0}" "$@" seed 2>&1 | grep -v '^\s*Container' || true
}

# Prints a line "<label>: PASS|FAIL" from a verify output and returns its status.
result_of() {
  local label="$1" output="$2"
  if grep -q '^result=PASS' <<<"$output"; then
    echo "$label: PASS"
    return 0
  fi
  echo "$label: FAIL"
  return 1
}
