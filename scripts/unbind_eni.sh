#!/usr/bin/env bash
set -euo pipefail

ENI_ID=${1:-}

if [[ -z "$ENI_ID" ]]; then
    echo "Usage: $0 <ENI_ID>"
    echo "Example: $0 eni-1234567890abcdef0"
    exit 1
fi

echo "Unbinding ENI $ENI_ID from DPDK..."

PCI_ADDR=$(lspci -D | grep "Elastic Network Adapter" | tail -1 | cut -d' ' -f1)

if [[ -z "$PCI_ADDR" ]]; then
    echo "Error: Could not find ENA device PCI address"
    exit 1
fi

echo "Found ENA device at PCI address: $PCI_ADDR"

echo "Unbinding $PCI_ADDR from vfio-pci..."
echo "$PCI_ADDR" > /sys/bus/pci/devices/$PCI_ADDR/driver/unbind 2>/dev/null || true

echo "Binding $PCI_ADDR back to ena driver..."
echo "$PCI_ADDR" > /sys/bus/pci/drivers/ena/bind 2>/dev/null || true

if [[ -e "/sys/bus/pci/drivers/ena/$PCI_ADDR" ]]; then
    echo "Successfully bound $PCI_ADDR back to ena driver"
    echo "ENI $ENI_ID is back to kernel networking"
else
    echo "Error: Failed to bind device back to ena driver"
    exit 1
fi
