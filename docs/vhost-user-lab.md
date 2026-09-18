# Run the guest AF_XDP / shared-RAM generator path

This implements the previously proposed path with two new examples:

* `vhost_user_net` runs beside QEMU on **macOS or Linux**. It maps QEMU's guest
  RAM and generates Ethernet test packets directly into posted virtio RX buffers.
* `xdp_vm_rx` runs **inside the Linux guest**. It installs the XDP redirect,
  binds an AF_XDP socket, starts the generator, validates packets, compares sampled
  guest physical addresses, and recycles the frames to FILL.

No vmnet, TAP, physical NIC, IP configuration or io_uring is involved on the lab
data path. There is a separate ordinary management NIC for SSH. The backend is
a single-queue, trusted-VM experiment, not a general-purpose virtual switch.

## 1. Decide which operating system is actually running QEMU

The supplied `--enable-linux-io-uring` configure flag and backend list indicate
a **Linux-host build**. `aarch64-softmmu` specifies the emulated machine, not the
host operating system. Run these where the QEMU binary lives:

```sh
uname -s
file ./qemu-system-aarch64
./qemu-system-aarch64 --version
./qemu-system-aarch64 -accel help
```

For the requested **Mac backend -> Linux guest** topology, both QEMU and
`vhost_user_net` must be native Mac executables. Use ARM64 Linux under HVF. A
Linux executable built inside a VM cannot be moved to macOS and run natively.

Alternatively, keep the existing Linux QEMU build and run the Rust backend in
that same Linux host environment. This validates the protocol and guest driver
first. If that environment is itself a VM, another guest is nested: use KVM only
if `/dev/kvm` is available, otherwise use TCG for functional validation.

**Do not put QEMU in Linux and the backend on Mac for this protocol.** Vhost-user
passes RAM and notification file descriptors through a local Unix socket. TCP,
SSH port forwarding, or a shared filesystem does not preserve those descriptors.
See the [vhost-user transport requirements](https://www.qemu.org/docs/master/interop/vhost-user.html#support-for-platforms-other-than-linux).

## 2. Apply the QEMU lab patches and rebuild

`scripts/qemu-lab.py` does everything in this section — clone, patch, configure,
build and code-sign — and refuses to go on if the result lacks vhost-user or
`queue_reset`. See [kernel-lab.md](kernel-lab.md) section 0. Read on for what it
is doing and why, and for the cases where you want to drive it yourself.

There are two independent fixes. **Native Mac builds need the header fix first:**
`hw/net/vhost_net.c` includes `linux-headers/linux/vhost.h` solely for
`VHOST_FILE_UNBIND`. That Linux ioctl header pulls in `<linux/vhost_types.h>` and
other Linux-only headers, causing the reported missing-header error. The patch
removes that include and uses the equivalent `-1` unbind value already used in
the same source file. QEMU's portable `standard-headers/linux/vhost_types.h`
include remains. This is a [known non-Linux build issue](https://gitlab.com/qemu-project/qemu/-/issues/1918).

Adding Linux headers to macOS's include path or disabling vhost-user is not the
fix for this lab. The native Mac configuration can keep `--enable-vhost-user`.

The [QEMU 11.0.1 virtio-net callbacks](https://github.com/qemu/qemu/blob/v11.0.1/hw/net/virtio-net.c)
forward individual queue resets/restarts only for TAP peers. Linux's
`virtnet_xsk_pool_enable` rebinds RX and TX through queue resets. The supplied
patch adds vhost-user peers to those two callbacks; simply advertising the
RING_RESET feature without forwarding the operations would leave stale rings.

On the OS that will **run QEMU**, set paths to your actual checkouts:

```sh
export STACK_ROOT=/absolute/path/to/async-net-stack-rs
export LAB_QEMU_SRC=/absolute/path/to/qemu
export LAB_QEMU_BUILD=/absolute/path/to/qemu/build

git -C "$LAB_QEMU_SRC" apply --check \
  "$STACK_ROOT/patches/qemu-vhost-net-macos-headers.patch"
git -C "$LAB_QEMU_SRC" apply \
  "$STACK_ROOT/patches/qemu-vhost-net-macos-headers.patch"
git -C "$LAB_QEMU_SRC" apply --check \
  "$STACK_ROOT/patches/qemu-11.0.1-vhost-user-queue-reset.patch"
git -C "$LAB_QEMU_SRC" apply \
  "$STACK_ROOT/patches/qemu-11.0.1-vhost-user-queue-reset.patch"
ninja -C "$LAB_QEMU_BUILD" qemu-system-aarch64
```

Both patches were checked against upstream v11.0.1 and the Mac checkout at
`d43c2d5f89` (`v11.1.0-1451-gd43c2d5f89`). Check your actual version with
`git -C "$LAB_QEMU_SRC" describe --always --dirty`; ARM64 binaries built on
different hosts may come from different revisions. If an apply check fails,
inspect the affected file: the patch may already be applied or your source may
differ. `git apply --reverse --check PATCH` detects an already-applied patch;
skip that patch instead of applying it twice. If you already applied the
queue-reset patch, apply only the new header patch and rerun Ninja.

These are local lab patches, not upstream release fixes. The queue-reset patch does not
implement arbitrary multi-queue resets or migration. Use the supplied one-pair
configuration. The Rust backend stops synchronously at GET_VRING_BASE, keeps the
call registration until replaced, and reloads the rings when restarted.

For a **new native Mac build**, create a fresh build directory inside the source
tree instead of reusing Linux build artifacts:

```sh
mkdir -p "$LAB_QEMU_SRC/build-mac-vhost"
cd "$LAB_QEMU_SRC/build-mac-vhost"
../configure --target-list=aarch64-softmmu --enable-hvf --enable-vhost-user
ninja qemu-system-aarch64
export LAB_QEMU_BUILD="$LAB_QEMU_SRC/build-mac-vhost"
```

Keep your normal build dependencies and HVF code signing setup. For the existing
Linux build, your io_uring option can remain; it does not activate this network
path. Check the rebuilt binary, not another installed `qemu-system-aarch64`:

```sh
"$LAB_QEMU_BUILD/qemu-system-aarch64" -machine virt -netdev help
"$LAB_QEMU_BUILD/qemu-system-aarch64" -device virtio-net-pci,help
"$LAB_QEMU_BUILD/qemu-system-aarch64" -accel help
```

## 3. Check the guest kernel configuration

Use the current ARM64 kernel you built. Confirm it contains
`virtnet_xsk_pool_enable`, `XDP_SETUP_XSK_POOL`, and the queue-reset implementation:

```sh
# On the Linux kernel build machine, set your existing source/build paths.
export LAB_KERNEL_SRC=/absolute/path/to/linux
export LAB_KERNEL_BUILD=/absolute/path/to/linux-build
rg -n 'virtnet_xsk_pool_enable|XDP_SETUP_XSK_POOL|virtqueue_reset' \
  "$LAB_KERNEL_SRC/drivers/net/virtio_net.c"
```

Required guest features are listed in `scripts/vhost-guest.config`, including
AF_XDP, BPF syscall/JIT, virtio PCI/net, and `/proc/self/pagemap` support. If any
are missing, merge that fragment into your existing configuration:

```sh
# STACK_ROOT is the project checkout on this Linux build machine.
cp -n "$LAB_KERNEL_BUILD/.config" "$LAB_KERNEL_BUILD/.config.before-vhost-lab"
"$LAB_KERNEL_SRC/scripts/kconfig/merge_config.sh" -m -O "$LAB_KERNEL_BUILD" \
  "$LAB_KERNEL_BUILD/.config" "$STACK_ROOT/scripts/vhost-guest.config"
make -C "$LAB_KERNEL_SRC" O="$LAB_KERNEL_BUILD" ARCH=arm64 olddefconfig
make -C "$LAB_KERNEL_SRC" O="$LAB_KERNEL_BUILD" ARCH=arm64 -j"$(nproc)" Image modules
make -s -C "$LAB_KERNEL_SRC" O="$LAB_KERNEL_BUILD" ARCH=arm64 kernelrelease
```

Use your existing installation/initramfs process if booting from disk. With a
direct `-kernel` boot, point the launcher at the resulting `arch/arm64/boot/Image`.
When building inside Linux for a Mac-hosted guest, copy the Image/initramfs to the
Mac as ordinary files. After boot, compare `uname -r` with `make kernelrelease`.

The launch configuration enables **ACCESS_PLATFORM without a virtual SMMU**.
This makes virtio use the guest DMA API while keeping identity DMA for the first
lab. The backend still implements vhost-user IOTLB misses, updates, permissions,
and invalidation; it never assumes an IOVA is a GPA. In the
[current driver](https://github.com/torvalds/linux/blob/master/drivers/net/virtio_net.c),
XSK enable requires compatible non-null DMA devices and a receive pool. The
[virtio ring DMA path](https://github.com/torvalds/linux/blob/master/drivers/virtio/virtio_ring.c)
explains why omitting ACCESS_PLATFORM can fail that check.

Do not add `iommu=smmuv3` or `ats=on` yet. The example handles adjacent IOTLB pages
when they map contiguously in shared RAM, but rejects one descriptor crossing
noncontiguous mappings. General SMMU scatter mappings need further work.

## 4. Build and start the host generator

On the **same OS as QEMU**:

```sh
cd "$STACK_ROOT"
cargo build --locked --release --example vhost_user_net
python3 scripts/vhost-user-smoke.py

export LAB_RUN=$(mktemp -d /tmp/async-net-vhost.XXXXXX)
chmod 700 "$LAB_RUN"
printf 'Use this directory in the other host terminal: %s\n' "$LAB_RUN"
./target/release/examples/vhost_user_net --socket "$LAB_RUN/net.sock" \
  --size 1500 --batch 64 --pps 10000 --samples 8 --trace-control \
  > "$LAB_RUN/host.jsonl" 2> "$LAB_RUN/host.log"
```

Leave that command running. It waits for QEMU and then for a START Ethernet frame
from the guest receiver. No packets are generated during boot or XSK setup. The
socket must not already exist. If you restart the backend after a disconnect,
restart the lab VM as well; automatic reconnection is not implemented.

The smoke test needs a C compiler to build its tiny atomic-index helper. It uses
real local sockets, SCM_RIGHTS and shared RAM, but does not substitute for the
guest-driver test below.

## 5. Launch the guest with shared RAM and a dedicated virtio NIC

In another **host** terminal, set `LAB_RUN` to the printed directory and use the
launcher. It adds the machine, accelerator, RAM object and two network devices.
Pass only your existing firmware/kernel/disk/console boot arguments after `--`.
Remove the old `-machine`, `-m`, `-smp`, `-cpu`, `-accel`, `-netdev`, and `-nic`
options; the wrapper supplies those. Shut down the old VM before opening the same
writable disk in the new instance.

For a direct-kernel VM, using your **existing** paths and kernel command line:

```sh
cd "$STACK_ROOT"
export LAB_RUN=/tmp/async-net-vhost.YOUR_PRINTED_SUFFIX
export LAB_KERNEL_IMAGE=/absolute/path/to/Image
export LAB_INITRD=/absolute/path/to/your-initramfs
export LAB_DISK=/absolute/path/to/your-guest.qcow2
export LAB_KERNEL_CMDLINE='YOUR EXISTING root=... console=ttyAMA0 ...'

python3 scripts/launch-vhost-vm.py \
  --qemu "$LAB_QEMU_BUILD/qemu-system-aarch64" --accel hvf \
  --socket "$LAB_RUN/net.sock" --ram-dir "$LAB_RUN" \
  --memory-mib 2048 --cpus 4 --ssh-port 2222 -- \
  -kernel "$LAB_KERNEL_IMAGE" -initrd "$LAB_INITRD" \
  -append "$LAB_KERNEL_CMDLINE" \
  -drive "file=$LAB_DISK,format=qcow2,if=virtio" -nographic
```

Omit `-initrd` if your existing configuration does not use one. For UEFI boot,
replace the `-kernel/-initrd/-append` arguments with your existing `-bios` or
pflash arguments. Keep the actual root device/partition from your bootable VM;
the launcher deliberately does not invent one. Add `--dry-run` before `--` to
inspect the complete generated command without booting.

For the Linux-host route, change only `--accel hvf` to `--accel kvm`, or to
`--accel tcg` when nested hardware acceleration is unavailable. TCG is for
correctness checks, not representative performance measurements.

The wrapper supplies these material properties:

| Setting | Purpose |
| --- | --- |
| `memory-backend-file,share=on` + machine `memory-backend=labram` | Back the actual guest RAM with a shareable file |
| `vhost-user,...,queues=1` | One RX/TX queue pair served by the Rust process |
| `disable-legacy=on,iommu_platform=on,queue_reset=on` | Modern virtio, DMA API and pool rebinding |
| `packed=off,event_idx=off,indirect_desc=off` | The supported split-ring protocol |
| `mrg_rxbuf=off`, checksum/GSO/TSO off | Small RX buffers, 12-byte virtio header, no offload interpretation |
| RX/TX queue size 256 | Bounded starting point; XSK rings are independently sized |
| Lab MAC `02:00:00:00:00:02` | Identify the interface without guessing its Linux name |
| Management MAC `02:00:00:00:00:10`, SSH `127.0.0.1:2222` | Keep management traffic off the test queue |

The wrapper creates a fresh RAM file in `--ram-dir` and removes it after QEMU
exits. It does not truncate/reuse a running VM's RAM file. It leaves your boot
disk and firmware arguments under your control.

## 6. Build and prepare the receiver inside the guest

SSH through the management NIC (or use the serial console):

```sh
ssh -p 2222 YOUR_GUEST_USER@127.0.0.1
uname -r
```

Use your existing method to copy/share **this working tree** into the guest;
the new examples must be included. Build natively in the guest:

```sh
cd /path/to/async-net-stack-rs
cargo build --locked --release --features xdp --example xdp_vm_rx
# Debian/Ubuntu, if missing:
sudo apt-get install iproute2 ethtool
sudo bash scripts/prepare-vhost-guest.sh
```

The script discovers the lab MAC, checks `virtio_net`, sets MTU 1500 and disables
remaining GRO/GSO/TSO. It refuses another MAC or an IPv4-configured interface.
If NetworkManager/systemd-networkd gave the lab NIC an address, mark **that lab
NIC** unmanaged and remove its IP configuration before running the script. Do
not configure an IP address on it; the experiment uses raw Ethernet frames.

Use the interface name printed by the script (shown below as `enp0s2`):

```sh
sudo bash -c 'ulimit -l unlimited; exec ./target/release/examples/xdp_vm_rx \
  --iface enp0s2 --mode zero-copy --packets 10000 --samples 8' \
  > guest-zero-copy.jsonl 2> guest-zero-copy.log
```

The receiver performs the requested chain:

1. Register UMEM and attach the native XDP redirect program.
2. Bind with **XDP_ZEROCOPY**, verify the kernel's XDP_OPTIONS confirmation, and
   supply FILL frames. The driver resets/rebinds its RX/TX virtqueues.
3. Send START through AF_XDP TX after setup. The backend consumes that TX chain.
4. The backend resolves RX IOVAs through its IOTLB and mapped RAM, then writes
   the virtio header, test header and generated payload directly into the chain.
5. Publish used entries with a release store and batch the interrupt notification.
6. Receive redirected frames in guest AF_XDP. Validate sequence and every payload
   byte; sample `/proc/self/pagemap` while the application still owns each buffer.
7. Drop the packet leases and call progress to recycle/refill and drive wakeups.

The generated header records the GPA of the Ethernet frame's first byte. The
guest compares that against the physical address of its `PacketBuf` data pointer,
including its offset within UMEM. Equal addresses plus strict zero-copy mode give
sampled evidence of the same guest buffer, not merely equal payload contents.
Root/CAP_SYS_ADMIN is needed to reveal pagemap PFNs. `--skip-address-check` exists
for restricted guests, but the result checker will not call that full evidence.

## 7. Check evidence, then run the copy control

Copy `guest-zero-copy.jsonl` to the host beside `host.jsonl`, or put both on a
machine with Python. The host log can be read while the backend remains running:

```sh
python3 scripts/check-vhost-run.py --host "$LAB_RUN/host.jsonl" \
  --guest guest-zero-copy.jsonl --mode zero-copy
```

Expected: 10,000 received packets, zero RX drops/invalid descriptors, TX drained,
`zero_copy: true`, and 8 physical-address matches. The host log should show
`access_platform: true`, `ring_reset: true`, queue-stop events during XSK rebinding,
IOTLB activity, and a matching `session_generated` with zero payload staging copies.
The checker matches session IDs and sampled sequence/GPA values across both logs.

Without changing the VM/backend, run the receiver again in **forced copy** mode:

```sh
sudo bash -c 'ulimit -l unlimited; exec ./target/release/examples/xdp_vm_rx \
  --iface enp0s2 --mode copy --packets 10000 --samples 8' \
  > guest-copy.jsonl 2> guest-copy.log
```

```sh
python3 scripts/check-vhost-run.py --host "$LAB_RUN/host.jsonl" \
  --guest guest-copy.jsonl --mode copy
```

Expected: the same valid contents and counts, `zero_copy: false`, and different
sampled physical addresses. The host writes a guest kernel RX buffer; the guest
then copies into UMEM. No automatic fallback is used in either run.

## 8. Troubleshoot by the failed step

| Symptom | Check next |
| --- | --- |
| Mac build fails on `linux/vhost_types.h` from `linux-headers/linux/vhost.h` | Apply `qemu-vhost-net-macos-headers.patch`, then rerun Ninja in the existing build directory |
| Missing HVF or macOS executable cannot run | Rebuild QEMU natively on the intended host; `file`, `uname`, `-accel help` |
| QEMU says backend lacks IOMMU features | Use this backend; its log should negotiate BACKEND_REQ/REPLY_ACK and ACCESS_PLATFORM |
| `XDP_ZEROCOPY` bind returns EINVAL/EOPNOTSUPP | Confirm booted kernel, ACCESS_PLATFORM, receive-pool state, and queue-reset support; inspect guest `dmesg` |
| No queue-stop messages when binding an XSK pool | Check that the patched QEMU binary is running |
| Backend reports unresolved IOTLB miss | Keep `iommu_platform=on,ats=off` and omit virtual SMMU; save control trace and QEMU stderr |
| Backend reports a short RX chain | Keep MTU 1500, size <=1514, mergeable buffers and offloads disabled |
| Guest receives nothing | Check START/session_start, then host RX samples, virtio notifications and FILL wakeups |
| Sequence gap or RX drops | Restart backend/VM with `--pps 1000`, then retry; avoid compiling in the guest during the run |
| PFN unavailable | Run as guest root with CAP_SYS_ADMIN and PROC_PAGE_MONITOR; check kernel lockdown policy |
| Equal payload but GPA mismatch in strict mode | Preserve both logs; do not label the run zero-copy evidence |

Record the environment with `scripts/vm-net-info.sh INTERFACE`, `uname -a`, the
kernel commit/config, QEMU version/patch, generated launch command, and both logs.

## igb comparison, after virtio works

The new Rust backend speaks **virtio queues**; attaching `igb` to it is not a valid
combination. QEMU's emulated igb needs an ordinary packet network backend such as
an isolated TAP setup on Linux or a supported vmnet setup on Mac. Boot this as a
separate experiment with your original boot command and management NIC; do not
use `launch-vhost-vm.py` for igb. Enable `CONFIG_IGB=y` in the guest kernel.

**Linux QEMU host:** create an unbridged TAP with an otherwise unused subnet.
These commands fail if `tap-igb-lab` already exists; choose another name instead
of reusing somebody else's interface.

```sh
sudo ip tuntap add dev tap-igb-lab mode tap user "$(id -un)"
sudo ip addr add 10.9.0.1/24 dev tap-igb-lab
sudo ip link set dev tap-igb-lab up
```

Append these arguments to the original QEMU command, keeping its management NIC:

```sh
-netdev tap,id=igblab,ifname=tap-igb-lab,script=no,downscript=no \
-device igb,netdev=igblab,mac=02:00:00:00:00:03
```

**Native Mac QEMU host:** check that `-netdev help` lists `vmnet-host`. The Linux
binary in the question will not list it. A native build needs vmnet support and
the usual vmnet privilege/entitlement setup. Append this instead of TAP:

```sh
-netdev vmnet-host,id=igblab,start-address=10.9.0.1,end-address=10.9.0.10,subnet-mask=255.255.255.0 \
-device igb,netdev=igblab,mac=02:00:00:00:00:03
```

The addresses must not overlap an existing host network. Keep this lab guest
interface unmanaged, with no DHCP client; the responder below owns `10.9.0.2`.
See QEMU's [vmnet-host options](https://www.qemu.org/docs/master/system/qemu-manpage.html).
This comparison uses vmnet's existing packet path, including its copies.

**Inside the guest**, identify MAC `02:00:00:00:00:03` with `ip -br link` and
substitute its interface name below. Check that `ethtool -i` reports `igb`.
Do not run the virtio preparation script against this NIC.

```sh
export LAB_IGB_IFACE=enp0s2   # replace with the dedicated igb interface
sudo ethtool -i "$LAB_IGB_IFACE"
sudo ethtool -L "$LAB_IGB_IFACE" combined 1
sudo ethtool -K "$LAB_IGB_IFACE" gro off gso off tso off
sudo ip link set dev "$LAB_IGB_IFACE" mtu 1500 up
cargo build --locked --release --features xdp --example device_bench
sudo bash -c 'ulimit -l unlimited; exec "$@"' _ \
  ./target/release/examples/device_bench --backend xdp --action reply \
  --iface "$LAB_IGB_IFACE" --queue 0 --mode zero-copy \
  --mac 02:00:00:00:00:03 --ip 10.9.0.2 --seconds 60 --warmup 2
```

During that 60-second run, generate traffic **on the QEMU host**:

```sh
ping -c 5 10.9.0.2
cargo run --locked --release --example udp_load -- \
  10.9.0.2:9000 --count 10000 --window 32
```

Repeat the guest command with `--mode copy`. Require the benchmark's reported
`zero_copy` value to match the requested mode. An unsupported strict bind is a
useful result to debug, not permission to silently relabel the copy result.
After shutting down this VM, Linux hosts can remove the TAP created above with
`sudo ip link delete tap-igb-lab`. Mac vmnet tears down with its QEMU instance.

Use igb to trace its hardware-shaped descriptors and emulated DMA. Use the
vhost-user example for the exact shared-RAM chain above. PCI PF/VF exploration is
a later configuration experiment; do not assume `igbvf` has the PF driver's XSK
support. See [QEMU's igb documentation](https://www.qemu.org/docs/master/system/devices/igb.html).

## Scope and validation

The backend supports one split RX/TX pair, bounded direct chains, 12-byte modern
virtio headers, ordinary shared RAM, pipe/eventfd notifications, IOTLB permission
checks and invalidation, and synchronous queue stops. Payload bytes are generated
in the mapped destination; only small control/test headers are assembled locally.

Packed/indirect rings, mergeable RX, offloads, general SMMU scatter mappings,
multi-queue, migration, hotplug/reconnection, and external packet forwarding are
not implemented. An RX chain that violates the supported configuration stops the
backend with a diagnostic. This is not a throughput claim about a physical NIC.

Local validation covers native Mac protocol tests with real shared-memory/fd
passing, synthetic IOTLB translations distinct from GPAs, reset/restart, header
splits, page boundaries, ring wraparound and malformed descriptors. Linux and
Rust 1.75 compilation checks are included. Both QEMU patches were checked against
the v11.0.1 source. The native Mac QEMU checkout at `d43c2d5f89` built successfully
with both patches; the resulting ARM64 binary reports HVF, vhost-user and vmnet
support, and exposes the required virtio properties. **The full patched QEMU + current Linux guest chain has not been
run on this development Mac**. The commands and evidence checker are provided
for that next validation on your VM.
