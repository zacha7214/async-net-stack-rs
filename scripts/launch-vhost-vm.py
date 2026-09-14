#!/usr/bin/env python3
"""Launch an ARM64 guest with shared RAM, management NIC and the synthetic lab NIC.

Pass only firmware/kernel/disk/console boot arguments after --. The wrapper owns
machine, accelerator, RAM and networking. --dry-run probes QEMU and prints argv.
"""
import argparse
import os
import platform
import shlex
import shutil
import stat
import subprocess
import tempfile
from pathlib import Path


def probe(qemu, *args):
    r = subprocess.run([qemu, *args], capture_output=True, text=True, timeout=15)
    return r.stdout + r.stderr


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--qemu", required=True, type=Path)
    p.add_argument("--socket", required=True, type=Path)
    p.add_argument("--ram-dir", required=True, type=Path)
    p.add_argument("--accel", required=True, choices=["hvf", "kvm", "tcg"])
    p.add_argument("--memory-mib", type=int, default=2048)
    p.add_argument("--cpus", type=int, default=4)
    p.add_argument("--ssh-port", type=int, default=2222)
    p.add_argument("--dry-run", action="store_true")
    p.add_argument("boot", nargs=argparse.REMAINDER)
    a = p.parse_args()
    qemu = str(a.qemu.expanduser().resolve())
    sock = a.socket.expanduser().resolve()
    ramdir = a.ram_dir.expanduser().resolve()
    if not os.access(qemu, os.X_OK): p.error("--qemu is not executable")
    if not ramdir.is_dir(): p.error("create --ram-dir first with mkdir -m 700")
    if not 256 <= a.memory_mib <= 65536 or not 1 <= a.cpus <= 64 or not 1024 <= a.ssh_port <= 65535:
        p.error("invalid memory, CPU count or SSH port")
    if a.accel == "hvf" and platform.system() != "Darwin": p.error("HVF requires a native macOS QEMU binary")
    if a.accel == "kvm" and not os.access("/dev/kvm", os.R_OK | os.W_OK):
        p.error("/dev/kvm unavailable; use --accel tcg for a functional nested-VM test")
    if not a.dry_run and (not sock.exists() or not stat.S_ISSOCK(sock.stat().st_mode)):
        p.error("start vhost_user_net on --socket before launching the VM")
    boot = a.boot[1:] if a.boot[:1] == ["--"] else a.boot
    if not boot: p.error("supply existing firmware/kernel/disk boot arguments after --")
    forbidden = ("-machine", "-M", "-m", "-accel", "-cpu", "-smp", "-netdev", "-nic", "-net", "-mem-path", "-numa")
    for word in boot:
        if word.split("=", 1)[0] in forbidden:
            p.error(f"wrapper owns {word}; remove it from the boot arguments")
    # Commas are QEMU option delimiters, even inside a single argv item.
    if any("," in str(v) for v in (sock, ramdir)):
        p.error("socket/RAM paths must not contain commas")
    devices = probe(qemu, "-device", "help")
    backends = probe(qemu, "-machine", "virt", "-netdev", "help")
    accel = probe(qemu, "-accel", "help")
    if "virtio-net-pci" not in devices or "vhost-user" not in backends or "user" not in backends.split():
        p.error("QEMU needs virtio-net-pci, vhost-user and user (management) networking")
    if a.accel not in accel.split(): p.error(f"QEMU does not list accelerator {a.accel}")
    if a.accel == "tcg": print("TCG: functional validation only; not representative of HVF/KVM performance.")
    print(probe(qemu, "--version").splitlines()[0])
    # A unique file prevents collisions with an existing VM's RAM. It is removed
    # after QEMU exits; no live guest RAM is ever reused or truncated here.
    with tempfile.TemporaryDirectory(prefix="vhost-ram-", dir=ramdir) as directory:
        ram = str(Path(directory) / "guest.ram")
        device = ("virtio-net-pci,id=labnic,netdev=lab,mac=02:00:00:00:00:02,"
                  "disable-legacy=on,iommu_platform=on,ats=off,packed=off,"
                  "event_idx=off,indirect_desc=off,queue_reset=on,mrg_rxbuf=off,mq=off,"
                  "csum=off,guest_csum=off,gso=off,guest_tso4=off,guest_tso6=off,"
                  "guest_ecn=off,guest_ufo=off,host_tso4=off,host_tso6=off,host_ecn=off,"
                  "host_ufo=off,rx_queue_size=256,tx_queue_size=256")
        cmd = [qemu, "-machine", "virt,memory-backend=labram", "-accel", a.accel,
               "-cpu", "max" if a.accel == "tcg" else "host", "-smp", str(a.cpus),
               "-m", str(a.memory_mib), "-object",
               f"memory-backend-file,id=labram,size={a.memory_mib}M,mem-path={ram},share=on",
               "-netdev", f"user,id=management,hostfwd=tcp:127.0.0.1:{a.ssh_port}-:22",
               "-device", "virtio-net-pci,netdev=management,mac=02:00:00:00:00:10",
               "-chardev", f"socket,id=vu,path={sock}",
               "-netdev", "vhost-user,id=lab,chardev=vu,queues=1", "-device", device, *boot]
        print(shlex.join(cmd), flush=True)
        if a.dry_run:
            print("Dry run only; its temporary RAM pathname is illustrative. Rerun without --dry-run.")
            return
        # Avoid starting with obviously insufficient storage for file-backed RAM.
        if shutil.disk_usage(ramdir).free < a.memory_mib * 1024 * 1024:
            p.error("insufficient free space in --ram-dir")
        raise SystemExit(subprocess.call(cmd))


if __name__ == "__main__":
    main()
