#!/usr/bin/env python3
"""
TRex benchmark orchestrator for dpdk-stdlib-rust performance testing.

Connects to TRex server, runs UDP echo benchmarks at multiple rate levels,
and outputs structured JSON results.

Usage:
    python3 run_benchmark.py \
        --server localhost \
        --config-name rust-dpdk \
        --src-ip 10.0.1.100 --dst-ip 10.0.1.200 \
        --dst-mac aa:bb:cc:dd:ee:ff \
        --packet-sizes 64,512,1400 \
        --duration 30 \
        --output /tmp/perf-results/rust-dpdk.json
"""

import argparse
import json
import os
import sys
import time
from datetime import datetime, timezone

# TRex stateless client library (available on TRex instance at /opt/trex)
sys.path.insert(0, '/opt/trex/automation/trex_control_plane/interactive')
from trex.stl.api import STLClient, STLStream, STLTXCont, STLPktBuilder, STLFlowLatencyStats

# Also need scapy from TRex's bundled copy
sys.path.insert(0, '/opt/trex/external_libs')
from scapy.layers.l2 import Ether
from scapy.layers.inet import IP, UDP
from scapy.packet import Raw


def build_streams(packet_size, src_ip, dst_ip, src_mac, dst_mac, src_port=12000, dst_port=9000):
    """Build main traffic + latency measurement streams."""
    min_payload = max(0, packet_size - 14 - 20 - 8 - 4)
    payload = b'P' * min_payload

    base_pkt = Ether(src=src_mac, dst=dst_mac) / IP(src=src_ip, dst=dst_ip) / \
               UDP(sport=src_port, dport=dst_port) / Raw(payload)

    pad = max(0, packet_size - len(base_pkt) - 4)
    if pad > 0:
        base_pkt = base_pkt / Raw(b'\x00' * pad)

    main_stream = STLStream(
        packet=STLPktBuilder(pkt=base_pkt),
        mode=STLTXCont(),
        name='udp_main'
    )

    latency_stream = STLStream(
        packet=STLPktBuilder(pkt=base_pkt),
        mode=STLTXCont(pps=1000),
        flow_stats=STLFlowLatencyStats(pg_id=0),
        name='udp_latency'
    )

    return [main_stream, latency_stream]


def run_single_benchmark(client, port, streams, target_pps, duration_sec):
    """Run a single benchmark at a given target PPS and return stats."""
    client.reset(ports=[port])
    client.add_streams(streams, ports=[port])

    # Clear stats before starting
    client.clear_stats()

    # Start traffic with duration — TRex will stop TX after duration_sec.
    # Use TRex 'pps' multiplier format for deterministic rate control.
    client.start(ports=[port], mult=f'{target_pps}pps', duration=duration_sec)

    # Sleep for the duration + drain time, then explicitly stop.
    # We avoid wait_on_traffic() because it can timeout on some setups
    # when the internal state machine doesn't transition cleanly.
    time.sleep(duration_sec + 2)
    client.stop(ports=[port])
    time.sleep(2)

    # Collect stats
    stats = client.get_stats()
    port_stats = stats.get(port, {})
    latency_stats = stats.get('latency', {}).get(0, {}).get('latency', {})

    tx_pkts = port_stats.get('opackets', 0)
    rx_pkts = port_stats.get('ipackets', 0)
    tx_bytes = port_stats.get('obytes', 0)
    rx_bytes = port_stats.get('ibytes', 0)
    tx_pps = tx_pkts / max(duration_sec, 1)
    rx_pps = rx_pkts / max(duration_sec, 1)

    drop_pkts = max(0, tx_pkts - rx_pkts)
    drop_pct = (drop_pkts / max(tx_pkts, 1)) * 100.0

    result = {
        'target_pps': target_pps,
        'duration_sec': duration_sec,
        'tx_pkts': tx_pkts,
        'rx_pkts': rx_pkts,
        'tx_pps': round(tx_pps),
        'rx_pps': round(rx_pps),
        'tx_mbps': round((tx_bytes * 8) / (max(duration_sec, 1) * 1e6), 2),
        'rx_mbps': round((rx_bytes * 8) / (max(duration_sec, 1) * 1e6), 2),
        'drop_pkts': drop_pkts,
        'drop_pct': round(drop_pct, 4),
    }

    # Latency stats (may not be available if no packets returned)
    if latency_stats:
        result['lat_avg_us'] = latency_stats.get('average', 0)
        result['lat_p99_us'] = latency_stats.get('total_max', 0)  # TRex reports max, not p99
        result['lat_max_us'] = latency_stats.get('total_max', 0)
        jitter = latency_stats.get('jitter', 0)
        result['lat_jitter_us'] = jitter
    else:
        result['lat_avg_us'] = -1
        result['lat_p99_us'] = -1
        result['lat_max_us'] = -1
        result['lat_jitter_us'] = -1

    return result


def main():
    parser = argparse.ArgumentParser(description='TRex UDP echo benchmark')
    parser.add_argument('--server', default='localhost', help='TRex server address')
    parser.add_argument('--port', type=int, default=0, help='TRex port index (default: 0)')
    parser.add_argument('--config-name', required=True, help='DUT config name (e.g. rust-dpdk)')
    parser.add_argument('--src-ip', required=True, help='Source IP (TRex data ENI)')
    parser.add_argument('--dst-ip', required=True, help='Destination IP (DUT data ENI)')
    parser.add_argument('--src-mac', default=None, help='Source MAC (auto-detected if omitted)')
    parser.add_argument('--dst-mac', required=True, help='Destination MAC (gateway MAC for AWS VPC)')
    parser.add_argument('--dst-port', type=int, default=9000, help='UDP destination port')
    parser.add_argument('--packet-sizes', default='64,512,1400,8500', help='Comma-separated packet sizes')
    parser.add_argument('--rate-steps', default='70000,140000,350000,700000', help='Comma-separated target PPS values')
    parser.add_argument('--duration', type=int, default=30, help='Seconds per rate step')
    parser.add_argument('--output', required=True, help='Output JSON file path')
    args = parser.parse_args()

    packet_sizes = [int(s) for s in args.packet_sizes.split(',')]
    rate_steps = [int(r) for r in args.rate_steps.split(',')]

    print(f"=== TRex Benchmark: {args.config_name} ===")
    print(f"Server: {args.server}, Port: {args.port}")
    print(f"Src: {args.src_ip} -> Dst: {args.dst_ip} (MAC: {args.dst_mac})")
    print(f"Packet sizes: {packet_sizes}")
    print(f"Rate steps (target PPS): {rate_steps}")
    print(f"Duration per step: {args.duration}s")

    # Connect to TRex
    client = STLClient(server=args.server)
    try:
        client.connect()
        client.acquire(ports=[args.port])

        # Get source MAC from port if not specified
        src_mac = args.src_mac
        if not src_mac:
            port_info = client.get_port_info(ports=[args.port])[0]
            src_mac = port_info.get('hw_mac', '00:00:00:00:00:00')
        print(f"Source MAC: {src_mac}")

        results = {}

        errors = []
        for pkt_size in packet_sizes:
            print(f"\n--- Packet size: {pkt_size}B ---")
            try:
                streams = build_streams(
                    packet_size=pkt_size,
                    src_ip=args.src_ip,
                    dst_ip=args.dst_ip,
                    src_mac=src_mac,
                    dst_mac=args.dst_mac,
                    dst_port=args.dst_port,
                )
            except Exception as e:
                print(f"\n  ERROR: {pkt_size}B stream build failed: {e}", flush=True)
                errors.append(f'{pkt_size}B: {e}')
                continue

            size_results = []
            for target_pps in rate_steps:
                try:
                    print(f"  Target: {target_pps:,} pps ... ", end='', flush=True)
                    result = run_single_benchmark(
                        client=client,
                        port=args.port,
                        streams=streams,
                        target_pps=target_pps,
                        duration_sec=args.duration,
                    )
                    size_results.append(result)
                    print(f"TX: {result['tx_pps']:,} pps, RX: {result['rx_pps']:,} pps, "
                          f"Drop: {result['drop_pct']}%, Lat avg: {result['lat_avg_us']} us")
                except Exception as e:
                    print(f"\n  ERROR: {pkt_size}B @ {target_pps} pps failed: {e}", flush=True)
                    errors.append(f'{pkt_size}B@{target_pps}pps: {e}')

            if size_results:
                results[f'{pkt_size}B'] = size_results

    finally:
        client.disconnect()

    # Write output (including partial results if some packet sizes failed)
    output = {
        'config_name': args.config_name,
        'timestamp': datetime.now(timezone.utc).isoformat(),
        'packet_sizes': packet_sizes,
        'rate_steps': rate_steps,
        'duration_per_step': args.duration,
        'src_ip': args.src_ip,
        'dst_ip': args.dst_ip,
        'results': results,
    }
    if errors:
        output['errors'] = errors

    os.makedirs(os.path.dirname(args.output), exist_ok=True)
    with open(args.output, 'w') as f:
        json.dump(output, f, indent=2)

    print(f"\nResults written to {args.output}")
    if errors:
        print(f"WARNING: {len(errors)} packet size(s) failed: {errors}")
        sys.exit(0)  # Still exit 0 — partial results are valid


if __name__ == '__main__':
    main()
