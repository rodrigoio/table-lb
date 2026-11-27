#!/bin/bash
# Correctness test 4 (project B) — switch race with the single-round-trip
# producer (project-b/pgbench/producer-1rt.sql): same load and checks as
# switch-race.sh. The consumer switches as often as it can, so producers
# constantly miss the hot buffer and take the fallback path.
NAME=switch-race-1rt \
TRANSACTIONS="${TRANSACTIONS:-10000}" \
SCRIPT=/scripts/project/producer-1rt.sql \
  exec "$(dirname "$0")/switch-race.sh"
