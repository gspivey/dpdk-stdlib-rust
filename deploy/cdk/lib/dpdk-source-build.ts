/**
 * User-data commands that build DPDK from source on a stock AL2023 instance.
 *
 * This is the fallback path, used only when no pre-built DPDK AMI is passed
 * in CDK context. CI always passes the per-line AMI
 * (scripts/ci/resolve-dpdk-ami.sh), so this path is for local experiments.
 *
 * There is no default DPDK version: pass one of the supported LTS releases
 * (README "Supported DPDK versions") with `-c dpdkVersion=25.11.4`. Without
 * it the commands fail at boot with a clear message, which the user-data EXIT
 * trap reports to CloudFormation. The check is not thrown at synth time
 * because synthesis builds every stack in the app, including stacks that are
 * not being deployed.
 *
 * Must match scripts/install_dpdk_amazon_linux.sh:
 *   --libdir=lib           forces libs into $PREFIX/lib/ (not lib64/)
 *   -Denable_kmods=false   avoids igb_uio build failures on AL2023 kernels
 */
export const DPDK_VERSION_CONTEXT_KEY = 'dpdkVersion';

const DPDK_VERSION_PATTERN = /^[0-9]{2}\.[0-9]{2}(\.[0-9]+)?$/;

export interface DpdkSourceBuild {
  /** Put first in the fallback bootstrap: exits at once when the version is missing. */
  guard: string[];
  /** Download and build commands; run after the build dependencies are installed. */
  build: string[];
}

export function dpdkSourceBuild(dpdkVersion: unknown): DpdkSourceBuild {
  if (typeof dpdkVersion !== 'string' || !DPDK_VERSION_PATTERN.test(dpdkVersion)) {
    const shown = String(dpdkVersion ?? '').replace(/[^0-9A-Za-z._-]/g, '');
    return {
      guard: [
        `echo "FATAL: no pre-built DPDK AMI and no valid -c ${DPDK_VERSION_CONTEXT_KEY}=<release> (e.g. 25.11.4); got '${shown}'"`,
        'exit 1',
      ],
      build: [],
    };
  }
  return { guard: [], build: [
    'echo "=== Downloading DPDK ==="',
    'cd /opt',
    `DPDK_VERSION="${dpdkVersion}"`,
    'curl -fL "https://fast.dpdk.org/rel/dpdk-${DPDK_VERSION}.tar.xz" -o "dpdk-${DPDK_VERSION}.tar.xz"',
    // Point releases unpack to dpdk-stable-X.Y.Z, the .0 release to dpdk-X.Y.
    'DPDK_DIR=$(tar -tf "dpdk-${DPDK_VERSION}.tar.xz" | head -1 | cut -d/ -f1)',
    'tar -xf "dpdk-${DPDK_VERSION}.tar.xz"',
    'cd "$DPDK_DIR"',
    'echo "=== Building DPDK ${DPDK_VERSION} ==="',
    'meson setup build --prefix=/usr/local --libdir=lib --buildtype=release -Denable_kmods=false -Ddisable_drivers=net/gve,net/ionic',
    'ninja -C build',
    'ninja -C build install',
    'echo "/usr/local/lib" > /etc/ld.so.conf.d/dpdk.conf',
    'ldconfig',
  ] };
}
