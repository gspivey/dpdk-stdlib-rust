#!/bin/bash
# DUT-side E5 capture. Usage: flame.sh <tag> <min_rx_pps>
# Waits for tokio-echo, then for a [PERF] line with rx_pps >= min_rx_pps
# (a saturated 64 B step), records 8 s of perf (299 Hz; DUT disk is small) with DWARF call graphs (release builds have no frame pointers), and folds
# the stacks with inferno so only a small text file has to leave the box.
T=$1; MIN=$2; L=/var/log/echo-tokio-dpdk.log; O=/root/flame-$T
exec > $O.log 2>&1
while ! pgrep -f target/release/tokio-echo >/dev/null; do sleep 1; done
P=$(pgrep -f target/release/tokio-echo | head -1)
echo "pid $P"
for i in $(seq 1 900); do
  last=$(grep -o 'rx_pps=[0-9]*' $L 2>/dev/null | tail -1 | cut -d= -f2)
  if [[ -n "$last" && "$last" -ge "$MIN" ]]; then break; fi
  sleep 1
done
echo "triggered at rx_pps=$last $(date -u +%T)"
perf record -F 299 --call-graph dwarf,8192 -p $P -o $O.data -- sleep 8
perf script -i $O.data > $O.script 2>/dev/null
/root/.cargo/bin/inferno-collapse-perf $O.script > $O.folded
rm -f $O.script $O.data; wc -l $O.folded; df -h / | tail -1
echo DONE
