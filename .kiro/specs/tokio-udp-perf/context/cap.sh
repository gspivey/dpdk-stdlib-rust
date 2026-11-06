#!/bin/bash
# Usage on DUT: cap.sh <tag>. Waits for tokio-echo, samples until it exits.
T=$1; O=/root/cap-$T.log
while ! pgrep -f target/release/tokio-echo >/dev/null; do sleep 1; done
P=$(pgrep -f target/release/tokio-echo | head -1)
for i in $(seq 1 90); do
  echo "=== $(date +%s) $(date -u +%T)"
  mpstat 4 1 | tail -1
  perf stat -e context-switches,cpu-migrations,task-clock -p $P -- sleep 4 2>&1 | grep -E "context|migr|task-clock"
  pgrep -f target/release/tokio-echo >/dev/null || break
done > $O 2>&1
