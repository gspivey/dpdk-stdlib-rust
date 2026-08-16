# AWS VPC Networking for DPDK

This document is the **authoritative reference** for how DPDK interacts with AWS VPC networking.
Read this BEFORE debugging any networking issue or modifying packet send/receive paths.

## Critical Rule

> **All DPDK outbound frames must use the VPC gateway MAC as the Ethernet destination.**
> Do NOT ARP for the peer's direct MAC address. AWS VPC is L3-routed, not L2-switched.

## AWS VPC Networking Model

### L3-Routed, Not L2-Switched

AWS VPC has **no real Layer 2 broadcast domain**, even within the same subnet. Every packet
transits through a **virtual router** managed by the VPC. This is fundamentally different from
traditional Ethernet:

| Traditional Ethernet | AWS VPC |
|---------------------|---------|
| ARP resolves to target host MAC | ARP resolves to virtual router (gateway) MAC |
| Same-subnet traffic is L2-switched | Same-subnet traffic goes through virtual router |
| Broadcast frames reach all hosts | Broadcast frames are dropped or limited |
| dst_mac = target host MAC | dst_mac = gateway MAC (always) |

### Gateway Address

The default gateway is always at `subnet_base + 1`:
- Subnet `10.0.1.0/24` → gateway is `10.0.1.1`
- Subnet `10.0.2.0/24` → gateway is `10.0.2.1`

### Proxy ARP

When an instance sends an ARP request for ANY IP in the VPC (same subnet or different):
- The VPC virtual router responds with **its own MAC address** (the gateway MAC)
- The response is NOT the target host's actual ENI MAC
- This is by design: all traffic routes through the virtual router for L3 forwarding

### What This Means for DPDK

When constructing raw Ethernet frames in DPDK (bypassing the kernel), you must:
1. Set **dst_mac = gateway MAC** for ALL outbound traffic (even to hosts on the same subnet)
2. Set **src_mac = your DPDK ENI's MAC** (read from DPDK port at init)
3. Set IP-layer addresses normally (src_ip = your DPDK ENI IP, dst_ip = actual target)

The VPC virtual router does L3 forwarding based on `dst_ip`. It doesn't care that `dst_mac` is
the gateway's MAC — that's expected. The router rewrites the Ethernet header before delivering
to the destination ENI.

### What Happens When You Get dst_mac Wrong

| dst_mac used | Result |
|-------------|--------|
| Gateway MAC | Packet delivered correctly via L3 forwarding |
| Broadcast (ff:ff:ff:ff:ff:ff) | **Dropped by VPC** — broadcast with unicast dst_ip is invalid |
| Target ENI's actual MAC | **Dropped or misrouted** — VPC doesn't do L2 switching |
| Random/zero MAC | **Dropped** — no matching route |

## How to Get the Gateway MAC

### Method 1: arping from kernel interface (recommended for test harness)

The kernel interface (ens5/eth0) is always available and can do standard ARP:

```bash
# Get the gateway IP from the route table
GATEWAY_IP=$(ip route show default | awk '/default via/ {print $3}' | head -1)

# Or derive from IMDS
TOKEN=$(curl -s -X PUT "http://169.254.169.254/latest/api/token" \
    -H "X-aws-ec2-metadata-token-ttl-seconds: 21600")
PRIMARY_MAC=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
    http://169.254.169.254/latest/meta-data/mac)
SUBNET_CIDR=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
    "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${PRIMARY_MAC}/subnet-ipv4-cidr-block")
GATEWAY_IP=$(echo "$SUBNET_CIDR" | sed 's|\.[0-9]*/.*|.1|')

# ARP for gateway MAC
GATEWAY_MAC=$(arping -c 1 -I ens5 "$GATEWAY_IP" 2>/dev/null \
    | grep -oE '([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}' | head -1)

# Fallback: read from ARP table after a ping
ping -c 1 -W 1 "$GATEWAY_IP" >/dev/null 2>&1 || true
GATEWAY_MAC=$(ip neigh show "$GATEWAY_IP" | awk '{print $5}' | head -1)
```

### Method 2: IMDS (for ENI metadata, not gateway MAC directly)

IMDS provides ENI-level metadata but not the gateway MAC. Useful for discovering your own MACs and subnet:

```bash
TOKEN=$(curl -s -X PUT "http://169.254.169.254/latest/api/token" \
    -H "X-aws-ec2-metadata-token-ttl-seconds: 21600")

# List all ENI MACs
curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
    http://169.254.169.254/latest/meta-data/network/interfaces/macs/

# For each MAC, get details
MAC="02:xx:xx:xx:xx:xx/"
curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
    "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${MAC}device-number"
curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
    "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${MAC}local-ipv4s"
curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
    "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${MAC}subnet-ipv4-cidr-block"
```

## ENI Configuration in Our Test Stack

| ENI | Interface | Driver | Subnet | Purpose |
|-----|-----------|--------|--------|---------|
| Primary (device 0) | ens5/eth0 | kernel ena | Private (10.0.1.0/24) | Management, SSM, kernel networking |
| Secondary (device 1) | ens6/eth1 | vfio-pci (when bound) | Private (10.0.1.0/24) | DPDK traffic |

Both ENIs are in the **same private subnet**. The CDK stack exports:
- `SenderDpdkEniId`, `ReceiverDpdkEniId` — ENI resource IDs
- `SenderDpdkEniPrivateIp`, `ReceiverDpdkEniPrivateIp` — ENI private IPs

**Note**: Source/destination check should be disabled on DPDK ENIs to allow the virtual router
to forward packets correctly. If not already set in CDK, this needs to be added.

## The ARP Cache Pre-population Strategy

Since DPDK bypasses the kernel, the kernel's ARP table is not available to DPDK applications.
Our approach:

1. **Discover gateway MAC** from the kernel interface (ens5) at test startup
2. **Pass `--gateway-mac`** to both echo server and test-client as a CLI argument
3. **Pre-populate the ARP cache** with: `target_ip → gateway_mac`
   - This maps the peer's IP to the gateway's MAC
   - It's a deliberate "lie" at the ARP layer, but produces correct L2 behavior
   - The VPC router does L3 forwarding based on dst_ip, so the packet reaches the right host

This avoids the need for DPDK-level ARP entirely in AWS VPC.

## Preferred Approach: NetworkConfig with Gateway

As of the subnet-aware routing implementation, the preferred way to handle AWS VPC
networking is to configure a `NetworkConfig` with the gateway IP. This replaces the
ARP cache pre-population hack with proper routing semantics:

```rust
use dpdk_udp::{UdpSocket, NetworkConfig};
use std::net::Ipv4Addr;

// Configure the socket with subnet and gateway knowledge
let socket = UdpSocket::builder()
    .network(
        NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 100), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1))
    )
    .bind("10.0.1.100:9000")?;

// Cross-subnet traffic automatically ARPs for the gateway IP
// Same-subnet traffic ARPs for the peer IP directly
// Both resolve to the gateway MAC in AWS VPC (via proxy ARP)
```

**Auto-detection**: On Linux, `UdpSocket::bind()` automatically reads `/proc/net/route`
and `/proc/net/arp` to discover the subnet, gateway, and seed the ARP cache. In AWS VPC
this means routing "just works" without any manual configuration — the gateway MAC from
the kernel's ARP table is pre-loaded into the DPDK ARP cache at bind time.

The ARP cache pre-population strategy (below) still works and is still used by the test
harness for explicit control, but new code should prefer `NetworkConfig` or rely on
auto-detection.

## Known Failure Patterns

| Symptom | Root Cause | Fix |
|---------|-----------|-----|
| ARP resolution timeout, falls back to broadcast MAC | DPDK port can't do ARP in VPC (no L2 broadcast) | Pre-populate ARP cache with gateway MAC via `--gateway-mac` |
| Packets sent but never received | dst_mac is broadcast `ff:ff:ff:ff:ff:ff` — VPC drops it | Use gateway MAC as dst_mac |
| Tier 1 fails, Tier 2 passes | Tier 2 uses kernel networking (handles ARP automatically) | Fix Tier 1 sender to use gateway MAC |
| Tier 3 fails same as Tier 1 | Same DPDK send path, same ARP issue | Same fix: gateway MAC |
| ARP reply contains unexpected MAC | VPC proxy ARP returns gateway MAC, not peer MAC | This is correct behavior — use the returned MAC |
| "Socket not connected" errors | `connect()` was `&mut self`, incompatible with `Arc<Mutex<>>` | Fixed: now uses interior mutability (Mutex/RwLock) |

## References

- [AWS VPC Networking Fundamentals](https://docs.aws.amazon.com/vpc/latest/userguide/how-it-works.html)
- [ENA Driver and DPDK on EC2](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/enhanced-networking-ena.html)
- [DPDK ENA PMD Documentation](https://doc.dpdk.org/guides/nics/ena.html)
