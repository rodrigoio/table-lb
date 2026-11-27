#!/bin/bash
# Full benchmark matrix, then the report.
#
#   CAMPAIGN=campaign-YYYYMMDD bench/campaign.sh
#
# Environment:
#   SCENARIOS   scenarios to run, in order (default "c1 c2 c3 c5"; c2 includes C4)
#   REPS_C1 REPS_C2 REPS_C3 REPS_C4 REPS_C5 REPS_C5_VARIANTS
#               repetitions (default 3 3 2 2 2 1)
#   LONG_CYCLE_MS  minimum cycle of the long-cycle B labels (default 30000 →
#               labels b-30s and b-1rt-30s); 0 = do not run those labels
#   <SCENARIO>_<VAR>  per-scenario overrides, see bench/run.sh
#   NO_REPORT   1 = do not build the report at the end (start.sh builds it)
#
# The order of A and B alternates between repetitions to spread any drift of
# the host over both projects.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export CAMPAIGN="${CAMPAIGN:-campaign-$(date -u +%Y%m%d)}"
SCENARIOS="${SCENARIOS:-c1 c2 c3 c5}"
LONG_CYCLE_MS="${LONG_CYCLE_MS:-30000}"
mkdir -p "$ROOT/results/$CAMPAIGN"
log="$ROOT/results/$CAMPAIGN/campaign.log"

run() {
  "$ROOT/bench/run.sh" "$@" 2>&1 | grep -v '^\s*Image ' | tee -a "$log"
}
pair() { # pair <scenario> <rep>: A then B on odd repetitions, B then A on even ones
  if (( $2 % 2 == 1 )); then run a "$1" "$2"; run b "$1" "$2"; else run b "$1" "$2"; run a "$1" "$2"; fi
}
# Long-cycle B: the consumer keeps the hot buffer open for at least
# LONG_CYCLE_MS between switches (MIN_CYCLE_MS), with the four-round-trip
# producer (b-<n>s) or the single-round-trip one (b-1rt-<n>s).
suffix="$(( LONG_CYCLE_MS / 1000 ))s"
long_b() { # long_b <scenario> <rep>
  (( LONG_CYCLE_MS > 0 )) || return 0
  LABEL="b-$suffix" MIN_CYCLE_MS="$LONG_CYCLE_MS" run b "$1" "$2"
}
long_b_1rt() { # long_b_1rt <scenario> <rep>
  (( LONG_CYCLE_MS > 0 )) || return 0
  LABEL="b-1rt-$suffix" MIN_CYCLE_MS="$LONG_CYCLE_MS" \
    PRODUCER_SCRIPT=/scripts/project/producer-1rt.sql run b "$1" "$2"
}

echo "campaign $CAMPAIGN started $(date -u +%FT%TZ) at $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown): $SCENARIOS" | tee -a "$log"

for scenario in $SCENARIOS; do
  case "$scenario" in
    c1) # saturation (+ B with the single-round-trip producer, + the same with long cycles)
      # b-<n>s is left out here: at saturation the drain is longer than the
      # minimum cycle, so it would behave like b (campaign-20261126 reached
      # 25–30 s cycles on its own with the four-round-trip producer).
      for (( rep = 1; rep <= ${REPS_C1:-3}; rep++ )); do
        pair c1 "$rep"
        LABEL=b-1rt PRODUCER_SCRIPT=/scripts/project/producer-1rt.sql run b c1 "$rep"
        long_b_1rt c1 "$rep"
      done ;;
    c2) # sustained load (+ A with DELETE cleanup, + long-cycle B, + C4: B with a single controller row)
      for (( rep = 1; rep <= ${REPS_C2:-3}; rep++ )); do
        pair c2 "$rep"
        LABEL=a-delete CLEANUP_MODE=delete run a c2 "$rep"
        long_b c2 "$rep"
        long_b_1rt c2 "$rep"
      done
      for (( rep = 1; rep <= ${REPS_C4:-2}; rep++ )); do
        LABEL=b-slots1 LOCK_SLOTS=1 C2_LOCK_SLOTS=1 run b c2 "$rep"
      done ;;
    c3) # burst (+ long-cycle B)
      for (( rep = 1; rep <= ${REPS_C3:-2}; rep++ )); do
        pair c3 "$rep"
        long_b c3 "$rep"
        long_b_1rt c3 "$rep"
      done ;;
    c5) # massive concurrency through the pooler (+ single-round-trip B, + the same with long
        # cycles, + B with fewer controller rows)
      for (( rep = 1; rep <= ${REPS_C5:-2}; rep++ )); do
        pair c5 "$rep"
        LABEL=b-1rt PRODUCER_SCRIPT=/scripts/project/producer-1rt.sql run b c5 "$rep"
        long_b_1rt c5 "$rep"
      done
      for (( rep = 1; rep <= ${REPS_C5_VARIANTS:-1}; rep++ )); do
        LABEL=b-slots64 C5_LOCK_SLOTS=64 run b c5 "$rep"
      done ;;
    *) # any other scenario file: one A/B pair per repetition
      reps_var="REPS_$(tr '[:lower:]-' '[:upper:]_' <<<"$scenario")"
      for (( rep = 1; rep <= ${!reps_var:-1}; rep++ )); do pair "$scenario" "$rep"; done ;;
  esac
done

echo "campaign finished $(date -u +%FT%TZ)" | tee -a "$log"
[[ "${NO_REPORT:-0}" == "1" ]] && exit 0
docker run --rm -u "$(id -u):$(id -g)" -v "$ROOT/results:/results" tlb-rust:latest \
  /usr/local/bin/report "/results/$CAMPAIGN" 2>&1 | tee -a "$log"
