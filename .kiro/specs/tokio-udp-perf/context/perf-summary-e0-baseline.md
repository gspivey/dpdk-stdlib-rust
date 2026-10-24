## Performance Test Results — c6in.xlarge

Commit: `unknown`
Timestamp: 2026-10-24T03:01:42.722768+00:00

### 64B packets

| Config | Target PPS | TX pps | RX pps | Drop % | NIC imissed | NIC ierrors | NIC nombuf | App Drops | Lat Avg (us) | Lat Max (us) | TX Mbps | RX Mbps |
|--------|-----------|--------|--------|--------|-------------|-------------|-----------|-----------|-------------|-------------|---------|---------|
| rust-dpdk | 70,000 | 70,000 | 69,000 | 1.43% | 0 | 39,232 | 0 | 0 | 0.0 | 0.0 | 35.8 | 35.3 |
| rust-dpdk | 140,000 | 140,000 | 139,000 | 0.71% | 0 | 35,203 | 0 | 0 | 0.0 | 0.0 | 71.7 | 71.2 |
| rust-dpdk | 350,000 | 350,000 | 349,000 | 0.29% | 0 | 31,912 | 0 | 0 | 0.0 | 0.0 | 179.2 | 178.7 |
| rust-dpdk | 700,000 | 700,000 | 698,948 | 0.15% | 0 | 41,903 | 0 | 0 | 0.0 | 0.0 | 358.4 | 357.9 |
| tokio-dpdk | 70,000 | 70,000 | 69,000 | 1.43% | 0 | 39,177 | 0 | 0 | 0.0 | 0.0 | 35.8 | 38.6 |
| tokio-dpdk | 140,000 | 140,000 | 139,000 | 0.71% | 0 | 33,033 | 0 | 0 | 0.0 | 0.0 | 71.7 | 77.8 |
| tokio-dpdk | 350,000 | 350,000 | 302,484 | 13.58% | 0 | 18,083 | 0 | 0 | 0.0 | 0.0 | 179.2 | 169.4 |
| tokio-dpdk | 700,000 | 700,000 | 301,774 | 56.89% | 0 | 21,737 | 0 | 0 | 0.0 | 0.0 | 358.4 | 169.0 |

### 512B packets

| Config | Target PPS | TX pps | RX pps | Drop % | NIC imissed | NIC ierrors | NIC nombuf | App Drops | Lat Avg (us) | Lat Max (us) | TX Mbps | RX Mbps |
|--------|-----------|--------|--------|--------|-------------|-------------|-----------|-----------|-------------|-------------|---------|---------|
| rust-dpdk | 70,000 | 70,000 | 69,000 | 1.43% | 0 | 33,054 | 0 | 0 | 0.0 | 0.0 | 286.7 | 282.6 |
| rust-dpdk | 140,000 | 140,000 | 139,000 | 0.71% | 0 | 41,885 | 0 | 0 | 0.0 | 0.0 | 573.4 | 569.3 |
| rust-dpdk | 350,000 | 350,000 | 349,000 | 0.29% | 0 | 34,936 | 0 | 0 | 0.0 | 0.0 | 1433.6 | 1429.5 |
| rust-dpdk | 700,000 | 700,000 | 697,552 | 0.35% | 0 | 31,874 | 0 | 0 | 288.0 | 0.0 | 2867.2 | 2857.2 |
| tokio-dpdk | 70,000 | 70,000 | 69,000 | 1.43% | 0 | 32,965 | 0 | 0 | 0.0 | 0.0 | 286.7 | 285.9 |
| tokio-dpdk | 140,000 | 140,000 | 139,000 | 0.71% | 0 | 35,997 | 0 | 0 | 0.0 | 0.0 | 573.4 | 576.0 |
| tokio-dpdk | 350,000 | 350,000 | 232,460 | 33.58% | 0 | 11,501 | 0 | 0 | 0.0 | 0.0 | 1433.6 | 963.3 |
| tokio-dpdk | 700,000 | 700,000 | 232,262 | 66.82% | 0 | 10,608 | 0 | 0 | 0.0 | 0.0 | 2867.2 | 962.5 |

### 1400B packets

| Config | Target PPS | TX pps | RX pps | Drop % | NIC imissed | NIC ierrors | NIC nombuf | App Drops | Lat Avg (us) | Lat Max (us) | TX Mbps | RX Mbps |
|--------|-----------|--------|--------|--------|-------------|-------------|-----------|-----------|-------------|-------------|---------|---------|
| rust-dpdk | 70,000 | 70,000 | 69,000 | 1.43% | 0 | 41,878 | 0 | 0 | 0.0 | 0.0 | 784.0 | 772.8 |
| rust-dpdk | 140,000 | 140,000 | 139,000 | 0.71% | 0 | 32,761 | 0 | 0 | 0.0 | 0.0 | 1568.0 | 1556.8 |
| rust-dpdk | 350,000 | 350,000 | 349,000 | 0.29% | 0 | 38,737 | 0 | 0 | 0.0 | 0.0 | 3920.0 | 3908.8 |
| rust-dpdk | 700,000 | 476,694 | 475,898 | 0.17% | 0 | 20,380 | 0 | 0 | 0.0 | 0.0 | 5339.0 | 5330.1 |
| tokio-dpdk | 70,000 | 70,000 | 69,000 | 1.43% | 0 | 38,395 | 0 | 0 | 0.0 | 0.0 | 784.0 | 776.1 |
| tokio-dpdk | 140,000 | 140,000 | 139,000 | 0.71% | 0 | 30,540 | 0 | 0 | 0.0 | 0.0 | 1568.0 | 1563.5 |
| tokio-dpdk | 350,000 | 350,000 | 148,946 | 57.44% | 0 | 11,282 | 0 | 0 | 0.0 | 0.0 | 3920.0 | 1675.3 |
| tokio-dpdk | 700,000 | 476,707 | 159,043 | 66.64% | 0 | 6,858 | 0 | 0 | 0.0 | 0.0 | 5339.1 | 1788.9 |

### NIC Drops Instrumentation Self-Check

Compares `(NIC-FINAL − NIC-BASELINE)` one-shot snapshots against the sum of per-tick `[PERF]` deltas over the reporter's lifetime. These MUST match exactly — a mismatch means per-tick delta bookkeeping is losing data.

| Config | Status | imissed (expected / actual / Δ) | ierrors (expected / actual / Δ) | rx_nombuf (expected / actual / Δ) |
|--------|--------|--------------------------------|----------------------------------|-----------------------------------|
| native-dpdk | no data | — | — | — |
| rust-dpdk | **OK** | 0 / 0 / 0 | 350,371 / 350,371 / 0 | 0 / 0 / 0 |
| tokio-dpdk | **OK** | 0 / 0 / 0 | 241,430 / 241,430 / 0 | 0 / 0 / 0 |
| plain-rust | no data | — | — | — |
| native-dpdk-v6 | no data | — | — | — |
| rust-dpdk-v6 | no data | — | — | — |

*`expected = FINAL − BASELINE` (raw NIC counter delta across reporter lifetime). `actual = sum of per-tick [PERF] delta fields`. Any Δ ≠ 0 is a bug in the tick loop's bookkeeping.*
