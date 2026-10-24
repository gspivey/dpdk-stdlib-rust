# TCP Performance Test DUT Configurations

Each configuration specifies how the DUT (Device Under Test) TCP echo server is
launched for TRex ASTF traffic generation.

## Configurations

### `plain-rust-tcp`

Kernel TCP baseline using `std::net::TcpStream`.

- **Binary**: `target/release/plain-tcp-echo`
- **Args**: `--ip <DUT_DATA_ENI_IP> --port 9000`
- **NIC binding**: kernel (ena driver)
- **Description**: Minimal TCP echo server using the standard library.
  Represents the kernel network stack baseline for comparison against DPDK.

### `rust-dpdk-tcp`

DPDK-accelerated TCP using `dpdk-stdlib-tcp`.

- **Binary**: `target/release/tcp-echo`
- **Args**: `--ip <DUT_DATA_ENI_IP> --port 9000`
- **NIC binding**: vfio-pci (DPDK)
- **Description**: TCP echo server using the dpdk-stdlib-tcp crate with
  full DPDK kernel-bypass. Requires hugepages and vfio-pci bound NIC.

### `tokio-dpdk-tcp`

Async DPDK-accelerated TCP using `dpdk-tokio` compat layer.

- **Binary**: `target/release/tokio-tcp-echo`
- **Args**: `--ip <DUT_DATA_ENI_IP> --port 9000`
- **NIC binding**: vfio-pci (DPDK)
- **Description**: Async TCP echo server using the tokio-compatible
  dpdk-stdlib-tcp async layer. Tests async overhead on top of DPDK.

## Benchmark Parameters

The TCP benchmark runner (`run_tcp_benchmark.py`) uses TRex ASTF mode:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `payload-sizes` | 64,512,1400,65536 | TCP payload sizes in bytes |
| `cps-rates` | 100,500,1000,5000 | Target connections per second |
| `duration` | 30 | Seconds per rate step |
| `dst-port` | 9000 | DUT TCP listen port |

## Metrics Collected

For each (payload_size, cps_rate) combination:

- **CPS** — actual connections per second established
- **Throughput** — aggregate TCP throughput in Mbps
- **Latency** — P50, P90, P99, max (microseconds)
- **Retransmits** — TCP segment retransmissions
- **Timeouts** — RTO-triggered retransmit timeouts
- **Connection drops** — failed connection attempts
