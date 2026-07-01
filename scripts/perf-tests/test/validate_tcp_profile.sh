#!/usr/bin/env bash
#
# Offline validation of the TRex ASTF profile built by run_tcp_benchmark.py.
#
# Builds the profile against the REAL TRex control-plane (its ArgVerify) with NO
# NIC and NO TRex server — catching ASTF-API errors (wrong arg names/types, bad
# IP ranges, etc.) in <1s locally instead of a ~30-min EC2 run. This exists
# because a chain of such errors (#104 LRO, #105 ip_range, #106 glob,
# ASTFAssociation) was found one-at-a-time on real hardware; every one of them
# is caught here instantly.
#
# Usage:  bash scripts/perf-tests/test/validate_tcp_profile.sh
# Exit 0 = profile builds for all payload sizes; 1 = ArgVerify/API error; 2 = setup error.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
CACHE="${TREX_CP_CACHE:-/tmp/trex-cp}"
CPI="$CACHE/scripts/automation/trex_control_plane/interactive"

# 1. Fetch the TRex control-plane profile module (sparse, shallow) — once, cached.
if [ ! -d "$CPI/trex/astf" ]; then
    echo "Fetching TRex control-plane into $CACHE ..."
    rm -rf "$CACHE"
    git clone --depth 1 --filter=blob:none --sparse \
        https://github.com/cisco-system-traffic-generator/trex-core "$CACHE" >/dev/null 2>&1 \
        || { echo "SKIP: could not clone trex-core (no network?)"; exit 2; }
    git -C "$CACHE" sparse-checkout set \
        scripts/automation/trex_control_plane/interactive/trex \
        scripts/external_libs >/dev/null 2>&1 \
        || { echo "ERROR: sparse-checkout failed"; exit 2; }
fi

# 2. Shim: Python 3.12+ removed stdlib `cgi`, which the bundled scapy 2.4.3 imports.
SHIM="$CACHE/.pyshim"; mkdir -p "$SHIM"
cat > "$SHIM/cgi.py" <<'PY'
def escape(s, quote=False):
    s = s.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;')
    return s.replace('"', '&quot;') if quote else s
PY

# 3. Build the profile for every payload size. A zmq stub is pre-loaded so TRex
#    never touches the arch-specific bundled pyzmq (only needed to CONNECT a
#    client, not to BUILD a profile).
export TREX_EXT_LIBS="$CACHE/scripts/external_libs"
export PYTHONPATH="$SHIM:$CPI"
python3 -W ignore - "$REPO_ROOT" <<'PY'
import sys, types
_z = types.ModuleType('zmq')
def _g(name):
    if name.startswith('__') and name.endswith('__'):
        raise AttributeError(name)   # no __path__ etc. -> TRex's ext-lib loader skips it
    return object
_z.__getattr__ = _g
sys.modules['zmq'] = _z

root = sys.argv[1]
sys.path.insert(0, root + '/scripts/perf-tests/trex')
import run_tcp_benchmark as m

ok = True
for ps in (64, 512, 1400, 65536):
    try:
        m.build_tcp_profile(ps, '10.0.1.5', '10.0.1.6', 9000)
        print(f'  ok   - build_tcp_profile(payload={ps})')
    except Exception as e:
        ok = False
        print(f'  FAIL - build_tcp_profile(payload={ps}): {type(e).__name__}: {e}')
print('== ASTF profile builds cleanly ==' if ok else '== ASTF profile has API errors ==')
sys.exit(0 if ok else 1)
PY
