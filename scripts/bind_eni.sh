#!/usr/bin/env bash
set -euo pipefail

ENI_ID=${1:-}

if [[ -z "$ENI_ID" ]]; then
    echo "Usage: $0 <ENI_ID>"
    echo "Example: $0 eni-1234567890abcdef0"
    exit 1
fi

echo "Binding ENI $ENI_ID to DPDK..."

PCI_ADDR=$(lspci -D | grep "Elastic Network Adapter" | tail -1 | cut -d' ' -f1)

if [[ -z "$PCI_ADDR" ]]; then
    echo "Error: Could not find ENA device PCI address"
    exit 1
fi

echo "Found ENA device at PCI address: $PCI_ADDR"

echo "Loading kernel modules..."
modprobe uio
modprobe vfio-pci

echo "Binding $PCI_ADDR to vfio-pci..."
echo "$PCI_ADDR" > /sys/bus/pci/devices/$PCI_ADDR/driver/unbind 2>/dev/null || true
echo "1d0f 0ec2" > /sys/bus/pci/drivers/vfio-pci/new_id 2>/dev/null || true
echo "$PCI_ADDR" > /sys/bus/pci/drivers/vfio-pci/bind

if [[ -e "/sys/bus/pci/drivers/vfio-pci/$PCI_ADDR" ]]; then
    echo "Successfully bound $PCI_ADDR to vfio-pci"
    echo "ENI $ENI_ID is ready for DPDK"
else
    echo "Error: Failed to bind device to vfio-pci"
    exit 1
fi

if command -v dpdk-devbind.py &> /dev/null; then
    echo "DPDK device status:"
    dpdk-devbind.py -s | grep -A5 -B5 "$PCI_ADDR" || true
fi
