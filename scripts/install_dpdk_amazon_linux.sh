#!/usr/bin/env bash
set -euo pipefail

echo "Installing DPDK on Amazon Linux 3..."

if [[ ! -f /etc/os-release ]] || ! grep -q "Amazon Linux" /etc/os-release; then
    echo "Warning: This script is designed for Amazon Linux 3"
fi

echo "Installing dependencies..."
sudo dnf update -y
sudo dnf groupinstall -y "Development Tools"
sudo dnf install -y \
    meson \
    ninja-build \
    python3-pip \
    python3-pyelftools \
    libbsd-devel \
    libpcap-devel \
    numactl-devel \
    kernel-devel \
    kernel-headers

pip3 install --user pyelftools

DPDK_VERSION="22.11.6"
DPDK_DIR="dpdk-${DPDK_VERSION}"
INSTALL_PREFIX="/usr/local"

echo "Downloading DPDK ${DPDK_VERSION}..."
if [[ ! -d "$DPDK_DIR" ]]; then
    curl -L "https://fast.dpdk.org/rel/dpdk-${DPDK_VERSION}.tar.xz" -o "dpdk-${DPDK_VERSION}.tar.xz"
    tar -xf "dpdk-${DPDK_VERSION}.tar.xz"
fi

cd "$DPDK_DIR"

echo "Configuring DPDK build..."
meson setup build \
    --prefix="$INSTALL_PREFIX" \
    --buildtype=release \
    -Denable_kmods=true

echo "Building DPDK..."
ninja -C build

echo "Installing DPDK (requires sudo)..."
sudo ninja -C build install

echo "Updating library path..."
echo "$INSTALL_PREFIX/lib" | sudo tee /etc/ld.so.conf.d/dpdk.conf
sudo ldconfig

echo "Loading DPDK kernel modules..."
sudo modprobe uio
sudo modprobe vfio-pci

echo "Setting up hugepages..."
echo 1024 | sudo tee /proc/sys/vm/nr_hugepages
sudo mkdir -p /mnt/huge
sudo mount -t hugetlbfs nodev /mnt/huge

echo "DPDK installation complete!"
echo "Hugepages configured: $(cat /proc/sys/vm/nr_hugepages) x 2MB"
