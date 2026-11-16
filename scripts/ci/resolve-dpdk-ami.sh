#!/usr/bin/env bash
# resolve-dpdk-ami.sh <dpdk-line> <arch>
#
# Prints the pre-built DPDK AMI for one supported LTS line and architecture,
# read from the per-line SSM parameters that build-dpdk-ami*.yml write:
#
#   x86_64: /dpdk-stdlib-rust/ami/dpdk-<line>/{latest,dpdk-version}
#   arm64:  /dpdk-stdlib-rust/ami/graviton/dpdk-<line>/{latest,dpdk-version}
#
# Fails loudly (non-zero exit) when the parameters are missing, when the
# recorded DPDK release is not on the requested line, or when the AMI is not
# available or its DpdkVersion tag disagrees. There is deliberately no
# fallback to the legacy /dpdk-stdlib-rust/ami/latest parameter or to a stock
# AMI: a test run must never silently use a different DPDK release than the
# line it reports on.
#
# Output (stdout, also appended to $GITHUB_OUTPUT when set):
#   ami_id=<ami-...>
#   dpdk_version=<X.Y.Z>
#
# Example:
#   scripts/ci/resolve-dpdk-ami.sh 25.11 x86_64
set -euo pipefail

LINE="${1:?Usage: resolve-dpdk-ami.sh <dpdk-line e.g. 25.11> <x86_64|arm64>}"
ARCH="${2:?Usage: resolve-dpdk-ami.sh <dpdk-line e.g. 25.11> <x86_64|arm64>}"

if [[ ! "$LINE" =~ ^[0-9]{2}\.[0-9]{2}$ ]]; then
  echo "::error::DPDK line must look like 25.11, got '${LINE}'" >&2
  exit 1
fi

case "$ARCH" in
  x86_64) PREFIX="/dpdk-stdlib-rust/ami/dpdk-${LINE}" ;;
  arm64)  PREFIX="/dpdk-stdlib-rust/ami/graviton/dpdk-${LINE}" ;;
  *) echo "::error::arch must be x86_64 or arm64, got '${ARCH}'" >&2; exit 1 ;;
esac

get_param() {
  aws ssm get-parameter --name "$1" --query Parameter.Value --output text
}

if ! AMI_ID=$(get_param "${PREFIX}/latest") || ! DPDK_VERSION=$(get_param "${PREFIX}/dpdk-version"); then
  echo "::error::No ${ARCH} AMI recorded for DPDK ${LINE} at ${PREFIX}. Run the 'Build DPDK AMI$([[ $ARCH == arm64 ]] && echo ' (Graviton)')' workflow for this line first." >&2
  exit 1
fi

if [[ ! "$AMI_ID" =~ ^ami-[0-9a-f]+$ ]]; then
  echo "::error::${PREFIX}/latest holds '${AMI_ID}', not an AMI ID" >&2
  exit 1
fi
if [[ "$DPDK_VERSION" != "$LINE" && "$DPDK_VERSION" != "$LINE".* ]]; then
  echo "::error::${PREFIX}/dpdk-version is '${DPDK_VERSION}', which is not on the ${LINE} line" >&2
  exit 1
fi

# Cross-check the AMI itself: it must exist, be available, match the arch,
# and carry the same DpdkVersion tag the build wrote.
if ! read -r STATE IMG_ARCH TAG_VERSION < <(aws ec2 describe-images --image-ids "$AMI_ID" \
       --query 'Images[0].[State, Architecture, Tags[?Key==`DpdkVersion`]|[0].Value]' --output text); then
  echo "::error::Could not describe ${AMI_ID} (from ${PREFIX}/latest)" >&2
  exit 1
fi
if [[ "$STATE" != "available" || "$IMG_ARCH" != "$ARCH" || "$TAG_VERSION" != "$DPDK_VERSION" ]]; then
  echo "::error::${AMI_ID} is state=${STATE} arch=${IMG_ARCH} DpdkVersion=${TAG_VERSION}; expected available/${ARCH}/${DPDK_VERSION}" >&2
  exit 1
fi

echo "ami_id=${AMI_ID}"
echo "dpdk_version=${DPDK_VERSION}"
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "ami_id=${AMI_ID}"
    echo "dpdk_version=${DPDK_VERSION}"
  } >> "$GITHUB_OUTPUT"
fi
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  echo "Tested on DPDK **${DPDK_VERSION}** (${ARCH}), AMI \`${AMI_ID}\` from \`${PREFIX}/latest\`" >> "$GITHUB_STEP_SUMMARY"
fi
echo "Using DPDK ${DPDK_VERSION} ${ARCH} AMI ${AMI_ID} (${PREFIX}/latest)" >&2
