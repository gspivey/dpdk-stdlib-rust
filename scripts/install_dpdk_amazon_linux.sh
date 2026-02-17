#!/usr/bin/env bash
set -euo pipefail

# Usage: install_dpdk_amazon_linux.sh [WORK_DIR]
# WORK_DIR defaults to current directory. DPDK source is downloaded and built here.

WORK_DIR="${1:-$(pwd)}"
cd "$WORK_DIR"

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
DPDK_DIR="dpdk-stable-${DPDK_VERSION}"
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
sudo modprobe uio || echo "Warning: could not load uio module (may not be available in this kernel)"
sudo modprobe vfio-pci || echo "Warning: could not load vfio-pci module (may not be available in this kernel)"

echo "Setting up hugepages..."
echo 1024 | sudo tee /proc/sys/vm/nr_hugepages
sudo mkdir -p /mnt/huge
if ! mountpoint -q /mnt/huge 2>/dev/null; then
    sudo mount -t hugetlbfs nodev /mnt/huge || echo "Warning: could not mount hugetlbfs (will be configured at boot via fstab)"
fi

echo "Verifying DPDK installation..."
DPDK_LIB_COUNT=$(ls /usr/local/lib/librte_* 2>/dev/null | wc -l)
if [[ "$DPDK_LIB_COUNT" -eq 0 ]]; then
    echo "ERROR: No DPDK libraries found in /usr/local/lib/"
    exit 1
fi

DPDK_VERSION=$(PKG_CONFIG_PATH=/usr/local/lib/pkgconfig pkg-config --modversion libdpdk 2>/dev/null || echo "unknown")

echo "DPDK installation complete!"
echo "  Libraries installed: ${DPDK_LIB_COUNT}"
echo "  DPDK version: ${DPDK_VERSION}"
echo "  Hugepages configured: $(cat /proc/sys/vm/nr_hugepages) x 2MB"
