# Mac host, Linux guest: a zero-copy learning lab

The most useful design is an ARM64 Linux guest under QEMU/HVF, with two different
test NICs used separately: QEMU's **igb** for studying a hardware driver's DMA and
descriptor protocol, and **virtio-net with shared guest RAM** for optimizing the
host/guest data path. Keep a separate management NIC. Use native ARM64 execution;
an x86 guest under instruction emulation introduces a large unrelated cost.
QEMU supports [HVF on Apple Silicon for AArch64 guests](https://www.qemu.org/2021/12/14/qemu-6-2-0/).

macOS lacking AF_XDP does not prevent shared-memory zero-copy between processes
or between a host backend and guest. The missing facility is a comparable public
path for directing the **physical Mac NIC's DMA into your chosen userspace pool**.
Emulated DMA uses host CPU accesses to guest RAM; it does not need that facility.

## What can be realistic about igb?

QEMU already emulates the Intel **82576**: PCI registers, descriptor processing,
guest-memory accesses and interrupts. It is intended to support SR-IOV testing,
although many hardware features are incomplete. See the
[igb device documentation](https://www.qemu.org/docs/master/system/devices/igb.html).
The Linux driver need not be x86 code to drive an Intel PCI device.

In your QEMU checkout, follow `igb_start_xmit`, descriptor reads and
`igb_txdesc_writeback` in
[hw/net/igb_core.c](https://github.com/qemu/qemu/blob/master/hw/net/igb_core.c).
The guest supplies DMA addresses; QEMU resolves accesses through its device
memory APIs. You do not need to invent a second fake DMA arena. **Ordinary guest
RAM, including guest UMEM pages, is already backed by host memory.**

If the guest driver posts UMEM-backed RX buffers, emulated device writes can land
in those buffers. That can exercise a real guest driver zero-copy path while the
Mac performs CPU copies to emulate the NIC. It says nothing about real PCIe DMA
bandwidth, cache pollution by a physical NIC, or hardware interrupt latency.
Current [Linux igb](https://github.com/torvalds/linux/blob/master/drivers/net/ethernet/intel/igb/igb_main.c)
handles `XDP_SETUP_XSK_POOL`; verify the **installed guest kernel**, its queue
configuration and its bind result. Do not infer that an emulated VF driven by
`igbvf` has the same AF_XDP support as the PF driven by `igb`.

With an existing bootable QEMU ARM64 configuration:

For complete configuration commands, see the
[igb comparison steps](vhost-user-lab.md#igb-comparison-after-virtio-works).

```sh
qemu-system-aarch64 -device help | rg 'igb|virtio-net'
qemu-system-aarch64 -netdev help
```

Use `-device igb,netdev=lab,mac=02:00:00:00:00:02` with an isolated `lab` network
backend available in that build. Those are device fragments, not a complete VM
launcher. Start with the PF and one queue; add VF/mailbox/reset experiments later.
Record the QEMU build, guest kernel commit, DMA descriptor address/length, UMEM
chunk/offset and completion sequence. Compare forced copy and strict zero-copy
with the existing `device_bench`; require its reported `zero_copy` value to agree.

## Designs that may work when optimizing for speed

This path now has experimental code and a
[step-by-step QEMU 11.0.1 lab](vhost-user-lab.md): the `vhost_user_net` host backend,
`xdp_vm_rx` guest receiver, launch/configuration scripts, and a queue-reset patch.
Use that guide to implement and verify the following design on your VM.

Use a Mac userspace **vhost-user-net backend** that maps QEMU's shared guest RAM.
QEMU remains responsible for running the guest and exposing virtio PCI registers;
the backend processes the virtqueues. Current
[vhost-user](https://www.qemu.org/docs/master/interop/vhost-user.html#support-for-platforms-other-than-linux)
supports non-Linux platforms with shared file mappings, SCM_RIGHTS and pipe-based
notifications when eventfd is unavailable. Linux `vhost-net` is a different backend
and is not available directly on macOS.

Target synthetic RX path:

```text
guest AF_XDP fill ring supplies a UMEM frame
    -> Linux virtio-net posts that buffer to its RX virtqueue
    -> Mac backend resolves its address into mapped guest RAM
    -> generator writes packet bytes directly into that buffer
    -> backend publishes used descriptors and batches the notification
    -> guest XDP redirects the same packet buffer to the AF_XDP RX ring
    -> application processes it, then returns its lease to FILL
```

There is no intermediate packet allocation or payload memcpy required between
generation and guest userspace in this target path. Generating the bytes still
writes memory and consumes CPU. A TX sink can read guest buffers in place and
release them only after it finishes. Forwarding an arbitrary TX buffer into a
different preposted RX buffer still normally requires copying: merely changing
a descriptor index cannot transfer ownership of unrelated pages between VMs.

Build a single RX/TX queue pair first, with checked split-descriptor chains,
bounded batches, explicit completions and small packets without offloads. Then
measure notification suppression, polling and larger batches. Pre-map guest RAM,
cache translations with proper invalidation, and retain mappings through device
reset and outstanding operations. Validate direction, lengths, chained ranges,
overlaps and indices before using any guest-supplied address. A private benchmark
with a trusted peer is substantially simpler than this device boundary.

### Virtio DMA Integration

The [current virtio-net driver](https://github.com/torvalds/linux/blob/master/drivers/net/virtio_net.c)
checks XSK headroom, receive-pool state and matching non-null RX/TX DMA devices
in `virtnet_xsk_pool_enable`. It also resets queues while rebinding the pool.
These capabilities have to work, not just the ordinary virtio packet path.

In [virtio_ring.c](https://github.com/torvalds/linux/blob/master/drivers/virtio/virtio_ring.c),
`virtqueue_dma_dev` can return NULL when the DMA mapping API is bypassed. The
[DMA quirk](https://github.com/torvalds/linux/blob/master/include/linux/virtio_config.h)
depends on `VIRTIO_F_ACCESS_PLATFORM`. Therefore a minimal backend that only
handles guest physical addresses is not enough to promise guest AF_XDP zero-copy.
Record the negotiated features and the exact failed enable check.

When negotiating the platform/IOMMU feature, implement the corresponding
[vhost-user IOTLB translation, permissions and invalidation protocol](https://www.qemu.org/docs/master/interop/vhost-user.html#iommu-support).
The protocol uses the older name `VIRTIO_F_IOMMU_PLATFORM` for that feature. Do not
advertise it and treat IOVAs as GPAs, or remove the driver's checks to make a bind
succeed. Begin with ordinary virtio/copy operation, then establish the DMA mapping
and queue-reset requirements before calling the result zero-copy.

### Mac extension: vmnet scatter/gather

[vmnet_read](https://developer.apple.com/documentation/vmnet/vmnet_read(_:_:_:))
accepts an array of packet descriptions with caller-provided iovecs. The proposed
optimization is to point those iovecs at **already leased guest RX buffers**:

```text
baseline:  vmnet -> host staging packets -> memcpy into guest buffers
candidate: vmnet -> iovecs referring directly to guest buffers
```

This removes the backend's extra staging copy; it does not establish physical NIC
DMA into guest memory. Use batches for vmnet reads/writes, respect partial counts,
and keep buffers alive through the documented completion of each operation.
For RX, ensure each packet's buffer capacity meets the interface's reported
maximum; a guest posting small buffers may need a counted staging fallback.
Also handle the virtio header separately and negotiate offloads consistently.

The installed Apple SDK exposes `vmnet_read_max_packets_key` and
`vmnet_write_max_packets_key` (macOS 15+) and `vmnet_enable_virtio_header_key`
(15.4+). Query supported limits instead of hard-coding a batch cap. Compare
host-only traffic first, then an external peer using a bridged interface. This
would be a new vmnet/VM integration module; the example below does not call vmnet.
See Apple's [vmnet API and entitlement requirements](https://developer.apple.com/documentation/vmnet).

An `ivshmem` PCI BAR is useful for a custom guest driver exercise, but is not my
first choice for AF_XDP: PCI BAR memory is not automatically ordinary, pinnable
UMEM, and ARM cache attributes and atomic accesses need deliberate handling.
Also check platform support: QEMU's
[ivshmem guide](https://www.qemu.org/docs/master/system/devices/ivshmem.html)
describes Linux hosts. Sharing a file through virtiofs/9p does not by itself share
the guest's mapped physical pages coherently with the host.

## Runnable example: `shm_nic`

This repository now includes a **two-native-process memory experiment**, runnable
on Mac or Linux without root or a VM. It is a model of RX ownership and completion,
not an igb emulator, virtio device, AF_XDP backend or vhost-user server. Its JSON
states `real_vm: false` and `physical_dma: false`.

```sh
cargo build --locked --release --example shm_nic
./target/release/examples/shm_nic --mode direct --wait spin \
  --size 1500 --batch 64 --ring 1024 --packets 20000000
./target/release/examples/shm_nic --mode both-copy --wait pipe \
  --size 1500 --batch 64 --ring 1024 --packets 20000000
python3 scripts/shm-nic-smoke.py
```

For a controlled comparison change **one** variable per pair of runs:

| Mode | Host producer | Consumer modeling guest userspace | Extra copies/frame |
| --- | --- | --- | --- |
| `direct` | Generates into shared frame | Reads shared frame | 0 |
| `host-copy` | Generates in staging, copies to shared frame | Reads shared frame | 1 |
| `guest-copy` | Generates into shared frame | Copies to local staging, reads it | 1 |
| `both-copy` | Host staging copy | Consumer staging copy | 2 |

Both processes map the same private temporary file. The producer initializes and
pre-faults shared RAM before spawning the consumer; the pathname is removed once
both map it. Offsets, not host pointers, identify frames. All slots start leased
to the producer; publishing hands them to the consumer, and the completion index
returns the leases. Unlike a full NIC there is no independent FILL allocator,
out-of-order completion, TX path or driver in between. Drop reaps the child and
unmaps local resources; a stalled peer produces a bounded timeout.

Payload writes precede the release publication; the consumer acquires before
reading. Completion releases ownership only after validation, and the producer
acquires before reuse. Published and completed counters have 128-byte alignment;
frame stride is configurable. This relies on coherent ordinary shared RAM and
native lock-free 64-bit atomics. It is not an ABI for MMIO, PCI BARs, untrusted
peers or arbitrary host/guest architecture combinations.

`--wait spin` polls continuously. `--wait pipe` sends an 8-byte hint per published
batch in each direction and uses poll/read when idle. Full nonblocking pipes
coalesce hints safely because the counters determine ownership. It is a simple
notification baseline, not virtio event-index suppression. JSON exposes writes,
EAGAIN, poll/read calls, actual batch sizes and each process's CPU time.

The default consumer verifies a 32-byte Ethernet test header, sequence and length;
`--verify full` also checks every payload byte. The producer always initializes
the complete frame. Packets use local experimental EtherType 0x88b5; they are not
IPv4/UDP load. Neither `frame_gbps` nor packets/s is physical network throughput.

Warmup is drained, staging pages are touched, and both workers reach a start
barrier before measurement. The host timer ends at the final receive completion.
Startup, teardown and warmup are excluded; no per-packet clocks are used. Total
CPU ns/packet includes both processes. These are throughput/CPU measurements, not
packet latency. The default ring fits in cache; sweep `--ring` and `--stride` to
test larger working sets. Full verification adds a memory scan, not a copy.

```sh
python3 scripts/shm-nic-sweep.py --output results.json \
  --packets 20000000 --warmup 1000000 --repeats 5 \
  --sizes 64 1500 --batches 1 8 64 256 \
  --modes direct host-copy guest-copy both-copy --waits spin pipe
```

The script shuffles run order, saves raw results and reports median/min/max. It
refuses to overwrite previous results. Keep the Mac plugged in, avoid concurrent
builds, and record power mode and thermal conditions; the OS may schedule the two
workers on different core types. Longer runs and the real VM experiment are
needed before predicting application or network throughput.

## Local measurements (2026-09-13)

On the development Mac (ARM64, Darwin 25.6.0), the focused sweep used 20 million
1500-byte frames per run, one million warmup frames, ring 1024, batch 64, stride
1536, header verification and five randomized repetitions. No core affinity or
power-mode control was applied. The release binary was built with Rust 1.98.0.

| Mode | Spin median Mframes/s (min–max) | Pipe median Mframes/s (min–max) | Spin total CPU ns/frame |
| --- | --- | --- | --- |
| Direct | 46.81 (46.07–49.25) | 33.48 (32.22–34.09) | 42.7 |
| Host copy | 22.05 (21.71–22.17) | 16.99 (16.71–17.10) | 90.7 |
| Guest copy | 31.91 (31.64–31.95) | 26.84 (26.60–27.01) | 62.7 |
| Both copies | 21.66 (21.57–21.99) | 18.20 (18.14–18.54) | 92.3 |

In this workload, removing host staging improved median spin throughput about
2.12×. Removing both staging copies improved it about 2.16×. That makes direct
placement into leased receive buffers worth testing in a real vmnet backend.
It does **not** predict a 2× improvement to a VM: driver execution, vmnet, guest
interrupts, parsing and physical I/O are absent. The small working set and
header-only consumer make these rates much higher than a complete network stack.
The non-monotonic pipe results also show that copy count alone does not explain
performance; work distribution and notification scheduling matter.

Raw runs and CPU/syscall counters are in
[shm-nic-sustained-results.json](shm-nic-sustained-results.json).
[shm-nic-local-results.json](shm-nic-local-results.json) contains the initial
48-run batch/size sweep (two million frames, three repetitions); some 64-byte
runs last only milliseconds, so treat those as exploratory measurements.

Validation: 17 native Mac two-process cases passed with full payload checks,
including tiny-ring wraparound and all copy/wait modes. Five invalid configurations
were rejected. Linux cross-compilation and Rust 1.75 compatibility were checked;
native Linux execution and QEMU/guest integration have not been run locally.
