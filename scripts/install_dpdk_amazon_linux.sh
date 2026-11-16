#!/usr/bin/env bash
set -euo pipefail

# Usage: install_dpdk_amazon_linux.sh [WORK_DIR] [DPDK_VERSION]
# WORK_DIR defaults to current directory. DPDK source is downloaded and built here.
# DPDK_VERSION is a full release such as 25.11.4. It can also come from the
# DPDK_VERSION environment variable. There is no default: the supported
# versions are the upstream-maintained LTS lines listed in the README
# ("Supported DPDK versions"), and the caller (Packer / CI) picks one.

WORK_DIR="${1:-$(pwd)}"
DPDK_VERSION="${2:-${DPDK_VERSION:-}}"
if [[ ! "$DPDK_VERSION" =~ ^[0-9]{2}\.[0-9]{2}(\.[0-9]+)?$ ]]; then
    echo "ERROR: DPDK version required (e.g. 25.11.4), got '${DPDK_VERSION}'" >&2
    echo "Usage: $0 [WORK_DIR] DPDK_VERSION" >&2
    exit 1
fi
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
    libbsd-devel \
    libpcap-devel \
    numactl-devel

# Install kernel headers matching the running kernel.
# AL2023 moved to kernel 6.12 where the new kernel6.12-headers package
# conflicts with the legacy kernel-headers (6.1) package. Since DPDK is
# built with -Denable_kmods=false (no kernel modules), the pre-installed
# headers from the running kernel's package are sufficient.
sudo dnf install -y kernel-devel kernel-headers 2>&1 || \
    echo "Note: kernel-headers conflict (expected on AL2023 kernel 6.12); using pre-installed headers"

# pyelftools is required by the DPDK build system (meson).
# python3-pyelftools RPM is not available in AL2023 default repos,
# so install via pip. Use --break-system-packages for AL2023 PEP 668.
pip3 install pyelftools || pip3 install --break-system-packages pyelftools

INSTALL_PREFIX="/usr/local"
DPDK_TARBALL="dpdk-${DPDK_VERSION}.tar.xz"

echo "Downloading DPDK ${DPDK_VERSION}..."
if [[ ! -f "$DPDK_TARBALL" ]]; then
    curl -fL "https://fast.dpdk.org/rel/${DPDK_TARBALL}" -o "$DPDK_TARBALL"
fi
# Point releases unpack to dpdk-stable-X.Y.Z, the .0 release to dpdk-X.Y.
# Read the top-level directory from the tarball instead of guessing.
# sed reads the whole listing: `| head -1` would close the pipe early, tar
# would die of SIGPIPE, and pipefail would fail the script (exit 141).
DPDK_DIR=$(tar -tf "$DPDK_TARBALL" | sed -n '1s#/.*##p')
if [[ -z "$DPDK_DIR" ]]; then
    echo "ERROR: could not read top-level directory from $DPDK_TARBALL" >&2
    exit 1
fi
[[ -d "$DPDK_DIR" ]] || tar -xf "$DPDK_TARBALL"

cd "$DPDK_DIR"

echo "Configuring DPDK build..."
# --libdir=lib: Force libraries into $PREFIX/lib/ instead of lib64/.
# AL2023 (Fedora-based) has a real /usr/lib64 directory, so meson
# auto-detects lib64 as the default libdir. We pin it to lib/ so that
# ldconfig, pkg-config, and downstream consumers use a single known path.
#
# -Denable_kmods=false: igb_uio frequently fails to compile against newer
# AL2023 kernels and is not needed — vfio-pci (shipped with the kernel) is
# the preferred driver.
#
# -Ddisable_drivers=net/gve,net/ionic: These drivers have known compilation
# failures on AL2023 due to kernel header conflicts (__le64 type issues,
# gve header conflicts). Not needed on AWS — only ENA is required.
meson setup build \
    --prefix="$INSTALL_PREFIX" \
    --libdir=lib \
    --buildtype=release \
    -Denable_kmods=false \
    -Ddisable_drivers=net/gve,net/ionic

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

INSTALLED_VERSION=$(PKG_CONFIG_PATH=/usr/local/lib/pkgconfig pkg-config --modversion libdpdk 2>/dev/null || echo "unknown")
# pkg-config reports 25.11.0 for the 25.11 release; compare on X.Y.Z.
EXPECTED_VERSION="$DPDK_VERSION"
[[ "$EXPECTED_VERSION" =~ ^[0-9]+\.[0-9]+$ ]] && EXPECTED_VERSION="${EXPECTED_VERSION}.0"
if [[ "$INSTALLED_VERSION" != "$EXPECTED_VERSION" ]]; then
    echo "ERROR: requested DPDK ${DPDK_VERSION} but pkg-config reports ${INSTALLED_VERSION}" >&2
    exit 1
fi

echo "DPDK installation complete!"
echo "  Libraries installed: ${DPDK_LIB_COUNT}"
echo "  DPDK version: ${INSTALLED_VERSION}"
echo "  Hugepages configured: $(cat /proc/sys/vm/nr_hugepages) x 2MB"
