#!/usr/bin/env bash
# diagnose-networking.sh — Dump networking state for CI failure diagnosis
#
# Runs ON an EC2 instance via SSM. Produces structured, section-delimited
# output that can be parsed by the agent or included in PR comments.
#
# Usage: bash scripts/integration-tests/diagnose-networking.sh
#
# Output format: sections delimited by "=== SECTION NAME ===" headers.
# Each section is self-contained and can be grep'd independently.

set -uo pipefail
# Intentionally NOT set -e — we want all sections to run even if some commands fail.

echo "=== NETWORKING DIAGNOSTICS ==="
echo "timestamp: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
echo "hostname: $(hostname)"
echo "kernel: $(uname -r)"

# ── DPDK Port Status ────────────────────────────────────────────────────────
echo ""
echo "=== DPDK PORT STATUS ==="
if command -v dpdk-devbind.py >/dev/null 2>&1; then
    dpdk-devbind.py --status 2>&1
elif [[ -f /usr/local/bin/dpdk-devbind.py ]]; then
    /usr/local/bin/dpdk-devbind.py --status 2>&1
else
    echo "dpdk-devbind.py not found"
    # Fallback: check driver bindings directly
    echo ""
    echo "PCI devices with vfio-pci driver:"
    ls /sys/bus/pci/drivers/vfio-pci/ 2>/dev/null | grep -E '^[0-9]' || echo "  (none)"
    echo ""
    echo "PCI devices with ena driver:"
    ls /sys/bus/pci/drivers/ena/ 2>/dev/null | grep -E '^[0-9]' || echo "  (none)"
fi

# ── IP Addresses ────────────────────────────────────────────────────────────
echo ""
echo "=== IP ADDRESSES ==="
ip addr show 2>/dev/null || ifconfig 2>/dev/null || echo "no ip/ifconfig command"

# ── ARP Table ───────────────────────────────────────────────────────────────
echo ""
echo "=== ARP TABLE ==="
ip neigh show 2>/dev/null || arp -a 2>/dev/null || echo "no arp command"

# ── Route Table ─────────────────────────────────────────────────────────────
echo ""
echo "=== ROUTE TABLE ==="
ip route show 2>/dev/null || route -n 2>/dev/null || echo "no route command"

# ── IMDS: ENI Information ───────────────────────────────────────────────────
echo ""
echo "=== IMDS: ENI INFORMATION ==="
TOKEN=$(curl -s -X PUT "http://169.254.169.254/latest/api/token" \
    -H "X-aws-ec2-metadata-token-ttl-seconds: 21600" 2>/dev/null || echo "")

if [[ -n "$TOKEN" ]]; then
    MACS=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
        http://169.254.169.254/latest/meta-data/network/interfaces/macs/ 2>/dev/null || echo "")

    if [[ -n "$MACS" ]]; then
        echo "ENI MACs found: $(echo "$MACS" | tr '\n' ' ')"
        echo ""
        for mac in $MACS; do
            echo "--- ENI: $mac ---"
            echo "  device-number: $(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
                "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${mac}device-number" 2>/dev/null || echo 'N/A')"
            echo "  local-ipv4s: $(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
                "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${mac}local-ipv4s" 2>/dev/null || echo 'N/A')"
            echo "  subnet-id: $(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
                "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${mac}subnet-id" 2>/dev/null || echo 'N/A')"
            echo "  subnet-cidr: $(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
                "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${mac}subnet-ipv4-cidr-block" 2>/dev/null || echo 'N/A')"
            echo ""
        done
    else
        echo "No ENI MACs returned from IMDS"
    fi
else
    echo "IMDS token acquisition failed"
fi

# ── Gateway ARP Test ────────────────────────────────────────────────────────
echo ""
echo "=== GATEWAY ARP TEST ==="

# Determine gateway IP
GATEWAY_IP=$(ip route show default 2>/dev/null | awk '/default via/ {print $3}' | head -1)

if [[ -z "$GATEWAY_IP" && -n "$TOKEN" ]]; then
    # Derive from IMDS subnet
    PRIMARY_MAC=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
        http://169.254.169.254/latest/meta-data/mac 2>/dev/null || echo "")
    if [[ -n "$PRIMARY_MAC" ]]; then
        SUBNET_CIDR=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" \
            "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${PRIMARY_MAC}/subnet-ipv4-cidr-block" 2>/dev/null || echo "")
        if [[ -n "$SUBNET_CIDR" ]]; then
            GATEWAY_IP=$(echo "$SUBNET_CIDR" | sed 's|\.[0-9]*/.*|.1|')
        fi
    fi
fi

echo "Gateway IP: ${GATEWAY_IP:-unknown}"

if [[ -n "$GATEWAY_IP" ]]; then
    # Ping gateway to populate ARP table
    ping -c 1 -W 2 "$GATEWAY_IP" >/dev/null 2>&1 || true

    # Show ARP entry for gateway
    echo "Gateway ARP entry:"
    ip neigh show "$GATEWAY_IP" 2>/dev/null || echo "  (no entry)"

    # Try arping for explicit MAC resolution
    if command -v arping >/dev/null 2>&1; then
        echo ""
        echo "arping result:"
        arping -c 1 -I ens5 "$GATEWAY_IP" 2>&1 || echo "  arping failed"
    else
        echo ""
        echo "arping not installed (install with: dnf install -y iputils)"
    fi
fi

# ── Hugepage Status ─────────────────────────────────────────────────────────
echo ""
echo "=== HUGEPAGE STATUS ==="
grep -i huge /proc/meminfo 2>/dev/null || echo "hugepage info not available"

# ── VFIO Status ─────────────────────────────────────────────────────────────
echo ""
echo "=== VFIO STATUS ==="
ls -la /dev/vfio/ 2>/dev/null || echo "no /dev/vfio directory"
echo ""
echo "noiommu mode:"
cat /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null || echo "  vfio module not loaded"

# ── DPDK Shared Memory ──────────────────────────────────────────────────────
echo ""
echo "=== DPDK SHARED MEMORY ==="
ls -la /var/run/dpdk/ 2>/dev/null || echo "no /var/run/dpdk/ directory (clean state)"

# ── DPDK-Related dmesg ──────────────────────────────────────────────────────
echo ""
echo "=== DPDK-RELATED DMESG (last 30 lines) ==="
dmesg 2>/dev/null | grep -iE 'vfio|dpdk|ena|hugepage|noiommu|uio' | tail -30 || echo "no relevant dmesg entries"

# ── Running DPDK Processes ──────────────────────────────────────────────────
echo ""
echo "=== DPDK-RELATED PROCESSES ==="
ps aux 2>/dev/null | grep -E 'echo|test-client|dpdk' | grep -v grep || echo "no DPDK processes running"

echo ""
echo "=== END DIAGNOSTICS ==="
