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

    # Unbind from current driver
    echo "$pci_addr" > "/sys/bus/pci/devices/$pci_addr/driver/unbind" 2>/dev/null || true

    # Use driver_override to tell the kernel which driver to use for this device.
    # This is more reliable than new_id because it doesn't depend on matching
    # vendor/device IDs (ENA uses 1d0f:ec20 on c5n instances).
    echo "vfio-pci" > "/sys/bus/pci/devices/$pci_addr/driver_override"

    # Bind to vfio-pci
    echo "$pci_addr" > /sys/bus/pci/drivers/vfio-pci/bind

    if is_bound_to_vfio "$pci_addr"; then
        echo "Successfully bound $pci_addr to vfio-pci"
        return 0
    else
        echo "ERROR: Failed to bind $pci_addr to vfio-pci" >&2
        return 1
    fi
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

    # Unbind from vfio-pci
    echo "$pci_addr" > "/sys/bus/pci/devices/$pci_addr/driver/unbind" 2>/dev/null || true

    # Clear driver_override so the kernel uses the default (ena) driver
    echo "" > "/sys/bus/pci/devices/$pci_addr/driver_override" 2>/dev/null || true

    # Bind to kernel ena driver
    echo "$pci_addr" > /sys/bus/pci/drivers/ena/bind 2>/dev/null || true

    if is_bound_to_ena "$pci_addr"; then
        echo "Successfully bound $pci_addr back to ena driver"
        return 0
    else
        echo "ERROR: Failed to bind $pci_addr to ena driver" >&2
        return 1
    fi
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
