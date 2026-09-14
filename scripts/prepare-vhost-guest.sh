#!/usr/bin/env bash
# Run INSIDE the guest. Deliberately refuses any NIC except the dedicated lab MAC.
set -euo pipefail
[[ $(uname -s) == Linux ]] || { echo 'Run this script inside Linux.' >&2; exit 1; }
[[ $EUID == 0 ]] || { echo 'Run with sudo inside the guest.' >&2; exit 1; }
for tool in ip ethtool; do command -v "$tool" >/dev/null || { echo "Install $tool first." >&2; exit 1; }; done
lab_iface=${1:-}
if [[ -z $lab_iface ]]; then
    for address in /sys/class/net/*/address; do
        if [[ $(cat "$address") == 02:00:00:00:00:02 ]]; then
            [[ -z $lab_iface ]] || { echo 'Multiple lab MACs: pass an interface name.' >&2; exit 1; }
            lab_iface=$(basename "$(dirname "$address")")
        fi
    done
fi
[[ -n $lab_iface && -f /sys/class/net/$lab_iface/address ]] || { echo 'Dedicated lab NIC not found.' >&2; exit 1; }
[[ $(cat "/sys/class/net/$lab_iface/address") == 02:00:00:00:00:02 ]] || { echo 'Refusing to change a NIC without the lab MAC.' >&2; exit 1; }
driver=$(basename "$(readlink -f "/sys/class/net/$lab_iface/device/driver")")
[[ $driver == virtio_net ]] || { echo "Expected virtio_net; found $driver" >&2; exit 1; }
if [[ -n $(ip -4 -o addr show dev "$lab_iface") ]]; then
    echo 'Lab NIC has an IPv4 address. Remove its network-manager/DHCP configuration first.' >&2
    exit 1
fi
ip link set dev "$lab_iface" mtu 1500
ethtool -K "$lab_iface" gro off gso off tso off
ip link set dev "$lab_iface" up
echo "Prepared $lab_iface. No IP address is needed for these Ethernet test frames."
ethtool -i "$lab_iface"
echo 'virtio features (ASCII bit positions, starting at bit zero):'
cat "/sys/class/net/$lab_iface/device/features"
printf '\nRun: sudo ./target/release/examples/xdp_vm_rx --iface %q --mode zero-copy --packets 10000\n' "$lab_iface"
