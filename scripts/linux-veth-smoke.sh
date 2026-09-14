#!/usr/bin/env bash
# Isolated test: generic AF_XDP -> ARP, ICMP, UDP -> TX completion.
# Build first: cargo build --release --features xdp --examples
set -euo pipefail
if [[ $(uname -s) != Linux || $EUID != 0 ]]; then
  echo 'Run as root inside a Linux test VM.' >&2
  exit 1
fi
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bench="$root/target/release/examples/device_bench"
load="$root/target/release/examples/udp_load"
[[ -x $bench && -x $load ]] || { echo 'Build release examples with --features xdp first.' >&2; exit 1; }
dut="ans-dut-$$"
peer="ans-peer-$$"
left="anl$$"
right="anr$$"
server=''
logdir=$(mktemp -d /tmp/async-net-smoke.XXXXXX)
cleanup() {
  if [[ -n $server ]]; then kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true; fi
  ip netns del "$dut" 2>/dev/null || true
  ip netns del "$peer" 2>/dev/null || true
  ip link del "$left" 2>/dev/null || true
  ip link del "$right" 2>/dev/null || true
}
trap cleanup EXIT INT TERM
ip netns add "$dut"
ip netns add "$peer"
ip link add "$left" type veth peer name "$right"
ip link set "$left" netns "$dut"
ip link set "$right" netns "$peer"
ip -n "$dut" link set "$left" name xdp0
ip -n "$peer" link set "$right" name xdp1
ip -n "$dut" link set xdp0 address 02:00:00:00:00:02 up
ip -n "$peer" link set xdp1 address 02:00:00:00:00:01 up
ip -n "$dut" link set lo up
ip -n "$peer" link set lo up
ip -n "$peer" addr add 10.9.0.1/24 dev xdp1
# TUN/XDP expect software-complete packets. Keep offload state explicit.
ip netns exec "$peer" ethtool -K xdp1 tx off rx off gro off gso off tso off
ip netns exec "$dut" "$bench" --backend xdp --iface xdp0 --generic --mode copy \
  --action reply --ip 10.9.0.2 --batch 64 --warmup 1 --seconds 8 \
  >"$logdir/device.json" 2>"$logdir/device.log" &
server=$!
# Poll readiness using real ICMP, which also exercises ARP resolution.
ready=0
for attempt in {1..5}; do
  kill -0 "$server" 2>/dev/null || { cat "$logdir/device.log" >&2; exit 1; }
  if ip netns exec "$peer" ping -c 1 -W 1 10.9.0.2; then ready=1; break; fi
done
[[ $ready == 1 ]] || { cat "$logdir/device.log" >&2; exit 1; }
ip netns exec "$peer" "$load" 10.9.0.2:9000 --count 1000 --payload 64 --window 8 --seconds 3 \
  >"$logdir/peer.json"
wait "$server"
server=''
python3 - "$logdir" <<'PY'
import json, pathlib, sys
p = pathlib.Path(sys.argv[1])
peer = json.loads((p / 'peer.json').read_text())
dev = json.loads((p / 'device.json').read_text())
assert peer['received'] > 0, peer
assert peer['invalid'] == 0, peer
assert peer['unsent'] == 0, peer
assert not dev['after']['zero_copy'], dev
assert dev['after']['tx_completed'] > 0, dev
assert dev['after']['invalid_descriptors'] == 0, dev
assert dev['after']['kernel']['tx_invalid'] == 0, dev
assert dev['pending_tx_after_drain'] == 0, dev
print(json.dumps({'peer': peer, 'device': dev}, indent=2))
PY
echo "Logs: $logdir"
