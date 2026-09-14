# VM and virtio-net lab

For a Mac host, see the [shared-memory NIC lab and QEMU design](mac-shared-memory.md):
igb emulated DMA, virtio DMA-feature requirements, macOS vhost-user notifications,
and a runnable host copy/batching experiment.

## What “zero-copy” establishes

A userspace buffer passing through this Rust API without a clone does not prove
that the kernel avoided copying it. AF_XDP's XDP_OPTIONS_ZEROCOPY is the runtime
check for the socket's driver-facing mode. The benchmark reports this explicitly.
AF_XDP copy mode still batches descriptors through shared rings; wrapping its
wakeups in io_uring does not remove its kernel copy. See the
[AF_XDP ABI](https://docs.kernel.org/networking/af_xdp.html).

Think about each boundary separately:

```text
physical NIC <-> host driver <-> TAP/vhost/QEMU <-> guest virtqueue
                                                    |
                                          guest virtio-net
                                          /             \
                                XSK pool buffer     kernel RX buffer
                                      |                 | copy
                                guest UMEM <-------------+
                                      |
                                Rust PacketBuf
```

Guest AF_XDP zero-copy concerns the guest driver/UMEM boundary. It does not prove
that host TAP, vhost-net, QEMU, or another virtual switch avoids copies. Treat
host and guest CPU cost as separate measurements. QEMU supports several network
backends with different paths; see its [network emulation documentation](https://www.qemu.org/docs/master/system/devices/net.html).

## A useful progression

1. **Two namespaces inside one Linux VM, veth + generic copy.** Run
   `scripts/linux-veth-smoke.sh`. This exercises ring ownership, offsets, ARP,
   ICMP, UDP, TX completion and teardown without involving virtio on the data
   path. Passing it says nothing about virtio zero-copy.
2. **The VM's dedicated virtio NIC, native XDP forced copy.** Use a second
   management NIC. Put a peer VM or Linux namespace on the same isolated L2
   network. Configure one combined RX/TX queue initially. This adds the guest
   virtqueue and host virtualization path while retaining a known copy mode.
3. **The same topology, strict zero-copy.** Change only `--mode zero-copy`.
   Record success or the exact bind/attach error. Never infer unsupported
   hardware from a permission error or report an auto fallback as zero-copy.
4. **Change the host backend.** Compare QEMU TAP handling with vhost-net on/off,
   keeping guest image, CPU assignment, offered load and packet sizes constant.
   Then compare vhost-user or passt if useful; avoid treating NAT/user networking
   as an equivalent L2 line-rate setup.
5. **Queue and scheduling experiments.** Compare worker sharing a CPU with NAPI
   against separate CPUs, then vary queue size and batch size independently.
   Multi-queue requires a shared program/XSKMAP manager that is not implemented
   here; do not start independent default XdpDevices on the same interface and
   expect both to receive.

For the dedicated test interface, collect its state before changing anything:

```sh
./scripts/vm-net-info.sh enp0s1 > guest-before.txt
sudo ethtool -L enp0s1 combined 1
sudo ethtool -K enp0s1 gro off gso off tso off
# Disable peer TX checksum offload too when testing software-checksummed packets.
# Unsupported ethtool settings are evidence to record, not silently ignore.
sudo ./target/release/examples/device_bench --backend xdp --iface enp0s1 \
  --action reply --mode copy --ip 10.9.0.2 --mac 02:00:00:00:00:02 \
  --batch 64 --queue 0 --cpu 2 --warmup 5 --seconds 30 > native-copy.json
```

Use the actual MAC of the guest NIC. Leave `10.9.0.2` owned by the userspace
responder, not also configured on the guest kernel interface. The peer owns
`10.9.0.1/24` on that L2 segment and runs `ping 10.9.0.2` and `udp_load`.
Stop each run before changing mode. The program detaches on normal Rust drop;
force-killing a netlink-attached process may require manual cleanup. Avoid
SIGKILL when collecting completion/teardown results.

For QEMU/KVM on a Linux host, an example network-device fragment is:

```text
-netdev tap,id=lab,ifname=tap-lab,script=no,downscript=no,vhost=on
-device virtio-net-pci,netdev=lab,mac=02:00:00:00:00:02
```

This assumes a TAP/bridge lab you already configured, plus your usual VM boot,
CPU, memory and disk arguments. Change only `vhost=on` to `vhost=off` for that
comparison. On a Mac host, Linux vhost-net is not available; use a Linux host or
nested Linux lab for that comparison, and record the extra virtualization layer.
QEMU's [command reference](https://www.qemu.org/docs/master/system/qemu-manpage.html)
also describes AF_XDP as a host network backend: testing host-side AF_XDP with
normal guest virtio is a separate experiment from guest-side AF_XDP.

## Read the driver beside the experiment

Do not assume virtio-net categorically lacks AF_XDP zero-copy. The
[Linux v6.11 source](https://github.com/torvalds/linux/blob/v6.11/drivers/net/virtio_net.c)
already handles XDP_SETUP_XSK_POOL. Distro backports and driver/virtio feature
combinations vary; inspect the exact guest source corresponding to `uname -r`.

In that checkout:

```sh
rg -n 'XDP_SETUP_XSK_POOL|virtnet_xsk|xsk_buff|xsk_tx_completed|xsk_pool_dma' \
  drivers/net/virtio_net.c
rg -n 'xsk_rcv|xsk_generic_rcv|xsk_generic_xmit|copy' net/xdp/xsk.c
rg -n 'xp_assign_dev|force_zc|XDP_SETUP_XSK_POOL' net/xdp/xsk_buff_pool.c
```

Trace these questions in order:

* During pool enable, which checks reject the queue, headroom, DMA device or
  receive mode? In the [current driver](https://github.com/torvalds/linux/blob/master/drivers/net/virtio_net.c),
  `virtnet_xsk_pool_enable` checks DMA-device compatibility, and receive setup
  uses XSK buffer addresses when adding virtqueue buffers. Match this to your
  source version; function details change over time.
* Follow fill addresses into RX buffers, through the virtqueue and XDP redirect,
  then into the RX descriptor's address/length. Observe the offset within the
  UMEM chunk, not just its base address.
* Follow a TX descriptor into the driver's transmit path, then its completion
  back to XSK/CQ. A virtqueue descriptor being available does not automatically
  mean its packet storage can be reused.
* Force XDP_COPY and follow the generic XSK receive/transmit path. Identify where
  payload bytes are copied and how completion ownership differs. Compare CPU
  profiles and packets/s at 64 versus 1500 bytes to separate per-packet cost from
  memory bandwidth.

For learning, instrument counts or use sampling profiles before tracing every
packet. If adding ftrace/BPF instrumentation, rerun without it for throughput.
Use deliberately tiny rings and constrained TX reserve to make buffer starvation
and completion lag observable; do not “fix” them by rewinding ring indices.

## io_uring as a separate fallback study

Compare plain TUN and `--backend uring` in the same guest with the same peer load.
Sweep RX depth 1/8/32/64 and total request depth 32/128/256, then TX-only with
RX depth zero. The prediction to test is fewer syscalls per packet at sufficient
batch size, traded against submission/completion bookkeeping and additional
outstanding buffers. No speedup is assumed until measured.

Modern [io_uring ZC RX](https://docs.kernel.org/networking/iou-zcrx.html) is another
kernel facility with its own setup and NIC requirements. It is not implemented
by this TUN backend, and ordinary registered buffers would not turn TUN into it.
