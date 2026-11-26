#!/bin/bash
# Full benchmark matrix (≈3.5 h on the reference 2 vCPU VM), then the report.
#
#   CAMPAIGN=campaign-YYYYMMDD bench/campaign.sh
#
# The order of A and B alternates between repetitions to spread any drift of
# the host over both projects.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export CAMPAIGN="${CAMPAIGN:-campaign-$(date -u +%Y%m%d)}"
mkdir -p "$ROOT/results/$CAMPAIGN"
log="$ROOT/results/$CAMPAIGN/campaign.log"

run() {
  "$ROOT/bench/run.sh" "$@" 2>&1 | grep -v '^\s*Image ' | tee -a "$log"
}
pair() { # pair <scenario> <rep>: A then B on odd repetitions, B then A on even ones
  if (( $2 % 2 == 1 )); then run a "$1" "$2"; run b "$1" "$2"; else run b "$1" "$2"; run a "$1" "$2"; fi
}

echo "campaign $CAMPAIGN started $(date -u +%FT%TZ) at $(git -C "$ROOT" rev-parse --short HEAD)" | tee -a "$log"

# C1 — saturation
for rep in 1 2 3; do pair c1 "$rep"; done

# C2 — sustained load (+ A with DELETE cleanup, + C4: B with a single controller row)
for rep in 1 2 3; do
  pair c2 "$rep"
  LABEL=a-delete CLEANUP_MODE=delete run a c2 "$rep"
done
for rep in 1 2; do
  LABEL=b-slots1 LOCK_SLOTS=1 run b c2 "$rep"
done

# C3 — burst
for rep in 1 2; do pair c3 "$rep"; done

echo "campaign finished $(date -u +%FT%TZ), building the report" | tee -a "$log"
docker run --rm -u "$(id -u):$(id -g)" -v "$ROOT/results:/results" tlb-rust:latest \
  /usr/local/bin/report "/results/$CAMPAIGN" 2>&1 | tee -a "$log"
