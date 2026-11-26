#!/bin/bash
# Correctness test 2 (project B) — negative control.
#
# Same as switch-race.sh, but producers read the controller WITHOUT FOR SHARE.
# The switch no longer waits for in-flight inserts, so some inserts land in a
# buffer that is being drained and get truncated. The test PASSES when events
# are lost: it proves that the pessimistic lock is what makes B correct (and
# that the switch-race test is able to detect a broken protocol).
NAME=no-lock \
SCRIPT=/scripts/tests/producer-b-nolock.sql \
EXPECT_LOSS=1 \
  exec "$(dirname "$0")/switch-race.sh"
