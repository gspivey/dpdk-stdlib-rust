#!/usr/bin/env python3
"""
TRex ASTF TCP benchmark orchestrator for dpdk-stdlib-rust performance testing.

Connects to TRex ASTF server, runs TCP echo benchmarks at multiple payload
sizes, and outputs structured JSON results with latency percentiles and CPS.

Usage:
    python3 run_tcp_benchmark.py \
        --server localhost \
        --config-name rust-dpdk-tcp \
        --src-ip 10.0.1.100 --dst-ip 10.0.1.200 \
        --dst-mac aa:bb:cc:dd:ee:ff \
        --payload-sizes 64,512,1400,65536 \
        --duration 30 \
        --output /tmp/perf-results/rust-dpdk-tcp.json
"""

import argparse
import json
import os
import sys
import time
from datetime import datetime, timezone

# TRex ASTF client library
sys.path.insert(0, '/opt/trex/automation/trex_control_plane/interactive')
from trex.astf.api import ASTFClient, ASTFProfile
from trex.astf.trex_astf_profile import (
    ASTFIPGenDist,
    ASTFIPGen,
    ASTFTCPClientTemplate,
    ASTFTCPServerTemplate,
    ASTFProgram,
    ASTFTemplate,
    ASTFAssociation,
)


def build_tcp_profile(payload_size, src_ip, dst_ip, dst_port):
    """Build an ASTF TCP echo profile programmatically."""
    payload = b'T' * payload_size

    prog_c = ASTFProgram()
    prog_c.send(payload)
    prog_c.recv(payload_size)

    prog_s = ASTFProgram()
    prog_s.recv(payload_size)
    prog_s.send(payload)

    ip_gen_c = ASTFIPGenDist(ip_range=src_ip, distribution="seq")
    ip_gen_s = ASTFIPGenDist(ip_range=dst_ip, distribution="seq")
    ip_gen = ASTFIPGen(
        glob=ASTFIPGenDist(ip_range="0.0.0.0", distribution="seq"),
        dist_client=ip_gen_c,
        dist_server=ip_gen_s,
    )

    template = ASTFTemplate(
        client_template=ASTFTCPClientTemplate(
            program=prog_c,
            port=dst_port,
        ),
        server_template=ASTFTCPServerTemplate(
            program=prog_s,
            assoc=ASTFAssociation(port=dst_port),
        ),
    )

    return ASTFProfile(
        default_ip_gen=ip_gen,
        templates=[template],
    )


def run_single_benchmark(client, payload_size, src_ip, dst_ip, dst_port, target_cps, duration_sec):
    """Run a single TCP benchmark at a given target CPS and return stats."""
    profile = build_tcp_profile(payload_size, src_ip, dst_ip, dst_port)

    client.reset()
    client.load_profile(profile)

    client.clear_stats()

    ts_start = time.time()
    client.start(mult=target_cps, duration=duration_sec)

    # Wait for traffic to finish
    time.sleep(duration_sec + 5)
    client.stop()
    time.sleep(2)
    ts_end = time.time()

    stats = client.get_stats()

    # Extract TCP-specific stats from ASTF counters
    total = stats.get('total', {})
    client_stats = total.get('client', {})
    server_stats = total.get('server', {})

    # Connection metrics
    tcp_connects = client_stats.get('m_active_flows', 0)
    tcp_connect_attempts = client_stats.get('tcps_connattempt', 0)
    tcp_connected = client_stats.get('tcps_connects', 0)
    tcp_closed = client_stats.get('tcps_closed', 0)

    # Data transfer
    tx_bytes = client_stats.get('tcps_sndbyte', 0)
    rx_bytes = client_stats.get('tcps_rcvbyte', 0)
    tx_pkts = client_stats.get('tcps_sndtotal', 0)
    rx_pkts = client_stats.get('tcps_rcvpack', 0)

    # Error counters
    tcp_timeouts = client_stats.get('tcps_rexmttimeo', 0)
    tcp_drops = client_stats.get('tcps_drops', 0)
    tcp_conndrops = client_stats.get('tcps_conndrops', 0)
    tcp_retransmits = client_stats.get('tcps_sndrexmitpack', 0)

    # CPS calculation
    actual_duration = max(ts_end - ts_start - 5, 1)  # subtract drain time
    cps = tcp_connected / actual_duration if tcp_connected > 0 else 0
    throughput_mbps = (tx_bytes * 8) / (actual_duration * 1e6) if tx_bytes > 0 else 0

    # Latency — TRex ASTF exposes latency in stats.get('latency', {})
    latency = stats.get('latency', {})
    lat_p50 = latency.get('percentile_50', -1)
    lat_p90 = latency.get('percentile_90', -1)
    lat_p99 = latency.get('percentile_99', -1)
    lat_avg = latency.get('average', -1)
    lat_max = latency.get('max', -1)

    # If structured percentiles unavailable, try histogram
    if lat_p50 == -1:
        hist = latency.get('histogram', [])
        if hist:
            lat_p50 = _percentile_from_hist(hist, 50)
            lat_p90 = _percentile_from_hist(hist, 90)
            lat_p99 = _percentile_from_hist(hist, 99)

    result = {
        'payload_size': payload_size,
        'target_cps': target_cps,
        'duration_sec': duration_sec,
        'ts_start_unix': round(ts_start, 3),
        'ts_end_unix': round(ts_end, 3),
        'tcp_connect_attempts': tcp_connect_attempts,
        'tcp_connected': tcp_connected,
        'tcp_closed': tcp_closed,
        'cps': round(cps, 1),
        'tx_bytes': tx_bytes,
        'rx_bytes': rx_bytes,
        'tx_pkts': tx_pkts,
        'rx_pkts': rx_pkts,
        'throughput_mbps': round(throughput_mbps, 2),
        'tcp_timeouts': tcp_timeouts,
        'tcp_drops': tcp_drops,
        'tcp_conndrops': tcp_conndrops,
        'tcp_retransmits': tcp_retransmits,
        'lat_avg_us': lat_avg if lat_avg != -1 else -1,
        'lat_p50_us': lat_p50 if lat_p50 != -1 else -1,
        'lat_p90_us': lat_p90 if lat_p90 != -1 else -1,
        'lat_p99_us': lat_p99 if lat_p99 != -1 else -1,
        'lat_max_us': lat_max if lat_max != -1 else -1,
    }

    return result


def _percentile_from_hist(histogram, percentile):
    """Extract a percentile from a TRex latency histogram."""
    if not histogram:
        return -1
    total = sum(count for _, count in histogram)
    if total == 0:
        return -1
    target = total * percentile / 100.0
    cumulative = 0
    for latency_us, count in histogram:
        cumulative += count
        if cumulative >= target:
            return latency_us
    return histogram[-1][0] if histogram else -1


def results_to_structured_json(config_name, all_results):
    """Convert results to structured JSON with test_name/backend/metric_name/metric_value/unit."""
    structured = []
    for r in all_results:
        payload = r['payload_size']
        base = {
            'test_name': f'tcp_echo_{payload}B',
            'backend': config_name,
            'payload_size': payload,
            'target_cps': r['target_cps'],
        }

        metrics = [
            ('cps', r['cps'], 'connections/sec'),
            ('throughput', r['throughput_mbps'], 'Mbps'),
            ('tcp_connected', r['tcp_connected'], 'connections'),
            ('tcp_retransmits', r['tcp_retransmits'], 'packets'),
            ('tcp_timeouts', r['tcp_timeouts'], 'count'),
            ('tcp_drops', r['tcp_drops'], 'connections'),
            ('lat_avg', r['lat_avg_us'], 'us'),
            ('lat_p50', r['lat_p50_us'], 'us'),
            ('lat_p90', r['lat_p90_us'], 'us'),
            ('lat_p99', r['lat_p99_us'], 'us'),
            ('lat_max', r['lat_max_us'], 'us'),
        ]

        for metric_name, metric_value, unit in metrics:
            structured.append({
                **base,
                'metric_name': metric_name,
                'metric_value': metric_value,
                'unit': unit,
            })

    return structured


def main():
    parser = argparse.ArgumentParser(description='TRex TCP echo benchmark (ASTF)')
    parser.add_argument('--server', default='localhost', help='TRex server address')
    parser.add_argument('--config-name', required=True, help='DUT config name (e.g. rust-dpdk-tcp)')
    parser.add_argument('--src-ip', required=True, help='Source IP (TRex data ENI)')
    parser.add_argument('--dst-ip', required=True, help='Destination IP (DUT data ENI)')
    parser.add_argument('--dst-mac', required=True, help='Destination MAC (gateway MAC for AWS VPC)')
    parser.add_argument('--dst-port', type=int, default=9000, help='TCP destination port')
    parser.add_argument('--payload-sizes', default='64,512,1400,65536',
                        help='Comma-separated payload sizes in bytes')
    parser.add_argument('--cps-rates', default='100,500,1000,5000',
                        help='Comma-separated target CPS (connections per second) values')
    parser.add_argument('--duration', type=int, default=30, help='Seconds per rate step')
    parser.add_argument('--output', required=True, help='Output JSON file path')
    args = parser.parse_args()

    payload_sizes = [int(s) for s in args.payload_sizes.split(',')]
    cps_rates = [int(r) for r in args.cps_rates.split(',')]

    print(f"=== TRex TCP Benchmark: {args.config_name} ===")
    print(f"Server: {args.server}")
    print(f"Src: {args.src_ip} -> Dst: {args.dst_ip}:{args.dst_port}")
    print(f"Payload sizes: {payload_sizes}")
    print(f"CPS rates: {cps_rates}")
    print(f"Duration per step: {args.duration}s")

    client = ASTFClient(server=args.server)
    all_results = []
    errors = []

    try:
        client.connect()

        for payload_size in payload_sizes:
            print(f"\n--- Payload: {payload_size}B ---")

            for target_cps in cps_rates:
                try:
                    print(f"  CPS: {target_cps:,} ... ", end='', flush=True)
                    result = run_single_benchmark(
                        client=client,
                        payload_size=payload_size,
                        src_ip=args.src_ip,
                        dst_ip=args.dst_ip,
                        dst_port=args.dst_port,
                        target_cps=target_cps,
                        duration_sec=args.duration,
                    )
                    all_results.append(result)
                    print(f"CPS: {result['cps']:,.0f}, "
                          f"Throughput: {result['throughput_mbps']:.1f} Mbps, "
                          f"Retransmits: {result['tcp_retransmits']}, "
                          f"P50: {result['lat_p50_us']} us, "
                          f"P99: {result['lat_p99_us']} us")
                except Exception as e:
                    print(f"\n  ERROR: {payload_size}B @ {target_cps} CPS: {e}", flush=True)
                    errors.append(f'{payload_size}B@{target_cps}cps: {e}')

    finally:
        client.disconnect()

    # Write raw results
    output = {
        'config_name': args.config_name,
        'timestamp': datetime.now(timezone.utc).isoformat(),
        'protocol': 'tcp',
        'payload_sizes': payload_sizes,
        'cps_rates': cps_rates,
        'duration_per_step': args.duration,
        'src_ip': args.src_ip,
        'dst_ip': args.dst_ip,
        'dst_port': args.dst_port,
        'results': all_results,
        'structured': results_to_structured_json(args.config_name, all_results),
    }
    if errors:
        output['errors'] = errors

    os.makedirs(os.path.dirname(args.output), exist_ok=True)
    with open(args.output, 'w') as f:
        json.dump(output, f, indent=2)

    print(f"\nResults written to {args.output}")
    if errors:
        print(f"WARNING: {len(errors)} step(s) failed: {errors}")
        sys.exit(0)  # Partial results are valid


if __name__ == '__main__':
    main()
