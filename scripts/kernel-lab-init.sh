#!/bin/sh
# PID 1 in the disposable lab initramfs. No disks are mounted or modified.
export PATH=/bin:/sbin
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /proc /sys /run /tmp
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devpts devpts /dev/pts
mount -t tmpfs tmpfs /tmp
exec </dev/console >/dev/console 2>&1
ulimit -l unlimited
echo "KERNEL_LAB_READY $(uname -r)"
mode=shell
packets=10000
for arg in $(cat /proc/cmdline); do
    case "$arg" in
        lab.mode=*) mode=${arg#lab.mode=} ;;
        lab.packets=*) packets=${arg#lab.packets=} ;;
    esac
done
case "$packets" in ''|*[!0-9]*) echo 'Invalid lab.packets'; mode=shell ;; esac
lab_iface=
for address in /sys/class/net/*/address; do
    if [ "$(cat "$address")" = 02:00:00:00:00:02 ]; then
        lab_iface=$(basename "$(dirname "$address")")
    fi
done
if [ -n "$lab_iface" ]; then
    ip link set "$lab_iface" mtu 1500 up
    echo "Lab NIC: $lab_iface"
fi
case "$mode" in
    zero-copy|copy)
        echo KERNEL_LAB_RECEIVER_BEGIN
        xdp_vm_rx --iface "$lab_iface" --mode "$mode" --packets "$packets" --samples 8
        result=$?
        echo "KERNEL_LAB_RESULT=$result"
        sync
        poweroff -f
        ;;
    shell) echo 'Run xdp_vm_rx --iface NAME --mode zero-copy --packets 10000' ;;
    *) echo "Unknown lab.mode: $mode" ;;
esac
while :; do setsid cttyhack sh; done
