#!/usr/bin/env bash
# configure-eni.sh - ENI bind/unbind/status wrapper with idempotency
#
# Wraps the existing bind_eni.sh and unbind_eni.sh scripts with:
#   - Idempotency checks (bind is a no-op if already bound to vfio-pci)
#   - Status reporting (current binding state)
#   - Appropriate exit codes
#
# Usage:
#   ./configure-eni.sh --action bind
#   ./configure-eni.sh --action unbind
#   ./configure-eni.sh --action status
#
# Exit codes:
#   0 - Success (or already in desired state)
#   1 - Failure

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ── Argument parsing ─────────────────────────────────────────────────────────

ACTION=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --action)
            ACTION="$2"
            shift 2
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 --action <bind|unbind|status>" >&2
            exit 1
            ;;
    esac
done

if [[ -z "$ACTION" ]]; then
    echo "Usage: $0 --action <bind|unbind|status>" >&2
    exit 1
fi

# ── Helper functions ─────────────────────────────────────────────────────────

# Find the PCI address of the secondary ENA device (the last one listed).
get_secondary_pci_addr() {
    local pci_addr
    pci_addr=$(lspci -D | grep "Elastic Network Adapter" | tail -1 | cut -d' ' -f1)
    echo "$pci_addr"
}

# Check if the device is currently bound to vfio-pci.
is_bound_to_vfio() {
    local pci_addr="$1"
    if [[ -e "/sys/bus/pci/drivers/vfio-pci/$pci_addr" ]]; then
        return 0
    fi
    return 1
}

# Check if the device is currently bound to the kernel ena driver.
is_bound_to_ena() {
    local pci_addr="$1"
    if [[ -e "/sys/bus/pci/drivers/ena/$pci_addr" ]]; then
        return 0
    fi
    return 1
}

# Kill any DPDK processes that may be holding the vfio-pci device open.
# A running DPDK process keeps an fd on /dev/vfio/*, which prevents the
# kernel from unbinding or rebinding the device.
kill_dpdk_processes() {
    local killed=false
    for name in echo test-client tokio-echo; do
        if pkill -f "target/release/$name" 2>/dev/null; then
            echo "Killed lingering $name process"
            killed=true
        fi
    done
    if [[ "$killed" == "true" ]]; then
        # Give processes time to release vfio-pci file descriptors
        sleep 2
    fi
    # Clean DPDK runtime state so the next process can re-init EAL
    rm -rf /var/run/dpdk/ 2>/dev/null || true
}

# ── Actions ──────────────────────────────────────────────────────────────────

do_status() {
    local pci_addr
    pci_addr=$(get_secondary_pci_addr)
    if [[ -z "$pci_addr" ]]; then
        echo "STATUS: no_secondary_eni"
        echo "No secondary ENA device found"
        return 1
    fi

    echo "PCI_ADDR=$pci_addr"

    if is_bound_to_vfio "$pci_addr"; then
        echo "STATUS: bound_to_vfio"
        echo "Secondary ENI ($pci_addr) is bound to vfio-pci (DPDK ready)"
        return 0
    elif is_bound_to_ena "$pci_addr"; then
        echo "STATUS: bound_to_ena"
        echo "Secondary ENI ($pci_addr) is bound to kernel ena driver"
        return 0
    else
        echo "STATUS: unbound"
        echo "Secondary ENI ($pci_addr) is not bound to any known driver"
        return 0
    fi
}

do_bind() {
    local pci_addr
    pci_addr=$(get_secondary_pci_addr)
    if [[ -z "$pci_addr" ]]; then
        echo "ERROR: No secondary ENA device found" >&2
        return 1
    fi

    # Idempotency: already bound to vfio-pci
    if is_bound_to_vfio "$pci_addr"; then
        echo "Already bound to vfio-pci ($pci_addr) - no action needed"
        return 0
    fi

    echo "Binding $pci_addr to vfio-pci..."

    # Kill any DPDK processes that might hold the vfio-pci device open
    kill_dpdk_processes

    # Load required kernel modules
    modprobe uio 2>/dev/null || true
    modprobe vfio-pci 2>/dev/null || true

    # Enable noiommu mode — required on EC2 Nitro instances which don't
    # expose hardware IOMMU to the guest.  Without this, vfio-pci refuses
    # to bind with "No such device".
    if [[ -f /sys/module/vfio/parameters/enable_unsafe_noiommu_mode ]]; then
        echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode
        echo "Enabled vfio noiommu mode"
    fi

    # Unbind from current driver (may already be unbound — that's fine)
    if [[ -e "/sys/bus/pci/devices/$pci_addr/driver" ]]; then
        echo "$pci_addr" > "/sys/bus/pci/devices/$pci_addr/driver/unbind" 2>/dev/null || true
        sleep 1
    fi

    # Use driver_override to tell the kernel which driver to use for this device.
    # This is more reliable than new_id because it doesn't depend on matching
    # vendor/device IDs (ENA uses 1d0f:ec20 on c5n instances).
    echo "vfio-pci" > "/sys/bus/pci/devices/$pci_addr/driver_override"

    # Bind to vfio-pci (retry up to 3 times if device is transiently busy)
    local bind_attempts=0
    local max_bind_attempts=3
    while [[ $bind_attempts -lt $max_bind_attempts ]]; do
        if echo "$pci_addr" > /sys/bus/pci/drivers/vfio-pci/bind 2>/dev/null; then
            break
        fi
        bind_attempts=$((bind_attempts + 1))
        echo "Bind attempt $bind_attempts failed, retrying in 2s..."
        sleep 2
    done

    # Poll until ENI is fully bound to vfio-pci
    local retries=0
    local max_retries=10
    while [[ $retries -lt $max_retries ]]; do
        if is_bound_to_vfio "$pci_addr"; then
            echo "Successfully bound $pci_addr to vfio-pci (after ${retries}s)"
            return 0
        fi
        retries=$((retries + 1))
        echo "Waiting for ENI bind to vfio-pci... (${retries}/${max_retries})"
        sleep 1
    done

    echo "ERROR: Failed to bind $pci_addr to vfio-pci after ${max_retries}s" >&2
    return 1
}

do_unbind() {
    local pci_addr
    pci_addr=$(get_secondary_pci_addr)
    if [[ -z "$pci_addr" ]]; then
        echo "ERROR: No secondary ENA device found" >&2
        return 1
    fi

    # Idempotency: already bound to ena
    if is_bound_to_ena "$pci_addr"; then
        echo "Already bound to ena driver ($pci_addr) - no action needed"
        return 0
    fi

    echo "Unbinding $pci_addr from vfio-pci and returning to ena driver..."

    # Kill any DPDK processes that hold the vfio-pci device open.
    # A running DPDK app keeps /dev/vfio/* open, preventing driver unbind.
    kill_dpdk_processes

    # Unbind from current driver (vfio-pci or whatever is loaded)
    if [[ -e "/sys/bus/pci/devices/$pci_addr/driver" ]]; then
        echo "$pci_addr" > "/sys/bus/pci/devices/$pci_addr/driver/unbind" 2>/dev/null || true
        sleep 1
    fi

    # Clear driver_override so the kernel uses the default (ena) driver
    echo "" > "/sys/bus/pci/devices/$pci_addr/driver_override" 2>/dev/null || true

    # Trigger kernel re-scan so ena driver picks up the device
    echo "$pci_addr" > /sys/bus/pci/drivers/ena/bind 2>/dev/null || true

    # Poll until ENI is fully transitioned to ena driver.
    # The kernel driver re-probe is asynchronous; without polling,
    # the next tier may attempt to bind before the transition completes.
    local retries=0
    local max_retries=15
    while [[ $retries -lt $max_retries ]]; do
        if is_bound_to_ena "$pci_addr"; then
            echo "Successfully bound $pci_addr back to ena driver (after ${retries}s)"

            # Bring up the network interface and configure its IP.
            # After rebinding to ena, the kernel creates the interface but
            # it may not have an IP assigned. We use multiple strategies:
            #   1. NetworkManager (AL2023 default)
            #   2. dhclient (older systems)
            #   3. EC2 IMDS manual fallback
            local iface
            iface=$(ls "/sys/bus/pci/devices/$pci_addr/net/" 2>/dev/null | head -1)
            if [[ -n "$iface" ]]; then
                echo "Bringing up interface $iface..."
                ip link set "$iface" up 2>/dev/null || true

                # Try NetworkManager first (AL2023)
                if command -v nmcli >/dev/null 2>&1; then
                    nmcli device connect "$iface" 2>/dev/null || true
                    sleep 3
                fi

                # Try dhclient as fallback
                if ! ip -4 addr show "$iface" 2>/dev/null | grep -q 'inet '; then
                    dhclient "$iface" 2>/dev/null || true
                    sleep 3
                fi

                # Final fallback: configure IP from EC2 IMDS (IMDSv2)
                if ! ip -4 addr show "$iface" 2>/dev/null | grep -q 'inet '; then
                    echo "DHCP didn't assign IP, trying IMDS (IMDSv2)..."
                    local mac
                    mac=$(cat "/sys/class/net/$iface/address" 2>/dev/null || true)
                    if [[ -n "$mac" ]]; then
                        # IMDSv2 requires a session token
                        local imds_token
                        imds_token=$(curl -sf --max-time 5 -X PUT \
                            -H "X-aws-ec2-metadata-token-ttl-seconds: 60" \
                            "http://169.254.169.254/latest/api/token" 2>/dev/null || true)
                        local imds_header=""
                        if [[ -n "$imds_token" ]]; then
                            imds_header="X-aws-ec2-metadata-token: $imds_token"
                        fi
                        local imds_ip
                        imds_ip=$(curl -sf --max-time 5 ${imds_header:+-H "$imds_header"} \
                            "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${mac}/local-ipv4s" 2>/dev/null | head -1 || true)
                        local imds_cidr
                        imds_cidr=$(curl -sf --max-time 5 ${imds_header:+-H "$imds_header"} \
                            "http://169.254.169.254/latest/meta-data/network/interfaces/macs/${mac}/subnet-ipv4-cidr-block" 2>/dev/null || true)
                        if [[ -n "$imds_ip" && -n "$imds_cidr" ]]; then
                            local prefix_len="${imds_cidr##*/}"
                            ip addr add "${imds_ip}/${prefix_len}" dev "$iface" 2>/dev/null || true
                            echo "Configured $iface with $imds_ip/$prefix_len from IMDS"
                        else
                            echo "IMDS lookup failed (token=${imds_token:+ok}${imds_token:-empty}, ip=${imds_ip:-empty}, cidr=${imds_cidr:-empty})"
                        fi
                    fi
                fi

                # Poll for IP with short timeout
                local ip_retries=0
                while [[ $ip_retries -lt 5 ]]; do
                    local iface_ip
                    iface_ip=$(ip -4 addr show "$iface" 2>/dev/null | grep -oP 'inet \K[\d.]+' | head -1 || true)
                    if [[ -n "$iface_ip" ]]; then
                        echo "Interface $iface configured with IP $iface_ip"
                        break
                    fi
                    ip_retries=$((ip_retries + 1))
                    sleep 1
                done
            fi
            return 0
        fi
        retries=$((retries + 1))
        echo "Waiting for ENI transition to ena driver... (${retries}/${max_retries})"
        sleep 1
    done

    echo "ERROR: Failed to bind $pci_addr to ena driver after ${max_retries}s" >&2
    return 1
}

# ── Main dispatch ────────────────────────────────────────────────────────────

case "$ACTION" in
    bind)
        do_bind
        ;;
    unbind)
        do_unbind
        ;;
    status)
        do_status
        ;;
    *)
        echo "Unknown action: $ACTION" >&2
        echo "Usage: $0 --action <bind|unbind|status>" >&2
        exit 1
        ;;
esac
