#!/usr/bin/env bash
# Read-only inventory; does not change offloads, queue count, affinity or XDP.
set -eu
iface=${1:?usage: vm-net-info.sh INTERFACE}
[[ $iface != *'/'* && -d /sys/class/net/$iface ]] || { echo 'No such Linux interface' >&2; exit 1; }
uname -a
ip -details link show dev "$iface"
for flag in -i -l -k -g -S; do
  ethtool "$flag" "$iface" 2>&1 || true
done
readlink "/sys/class/net/$iface/device/driver" || true
lscpu 2>/dev/null || true
printf '\nRX queues:\n'
ls -d /sys/class/net/"$iface"/queues/rx-* 2>/dev/null || true
printf '\nIRQs:\n'
if command -v rg >/dev/null; then rg "virtio|$iface" /proc/interrupts || true; fi
printf '\nXDP attachments:\n'
if command -v bpftool >/dev/null; then bpftool net show || true; fi
