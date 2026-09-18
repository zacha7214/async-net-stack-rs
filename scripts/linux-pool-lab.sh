#!/usr/bin/env bash
# Run only in a fresh network namespace; no host-interface changes.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
if [[ $(uname -s) != Linux ]]; then
    echo 'This lab requires Linux.' >&2
    exit 1
fi
# The public invocation always creates a fresh namespace. The inner program is
# passed directly to unshare, so no environment flag can skip isolation.
exec unshare --net bash -s -- "$ROOT" "$@" <<'INNER'
set -euo pipefail
ROOT=$1
shift
ip link set lo up
"${TUN_POOL_BIN:-$ROOT/target/release/examples/tun_pool}" --seconds 3600 &
worker=$!
trap 'kill "$worker" 2>/dev/null || true; wait "$worker" 2>/dev/null || true' EXIT
for ((i=0; i<100; i++)); do
    if ip link show labtun >/dev/null 2>&1; then break; fi
    if ! kill -0 "$worker" 2>/dev/null; then echo 'Worker failed to start' >&2; exit 1; fi
    sleep 0.02
done
ip addr add 10.77.0.1/24 dev labtun
ip link set labtun up
ip route add 255.255.255.255/32 dev labtun
# Optional kernel egress impairment, applied to requests toward the stack.
# Examples: NETEM='delay 2ms 1ms loss 1%' or NETEM='rate 10mbit limit 64'
if [[ -n ${NETEM:-} ]]; then
    read -r -a netem_args <<< "$NETEM"
    tc qdisc add dev labtun root netem "${netem_args[@]}"
fi
python3 "$ROOT/scripts/pool-client.py" "$@"
ip -s link show labtun
if command -v tc >/dev/null; then tc -s qdisc show dev labtun; fi
INNER
