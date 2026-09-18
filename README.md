# async-net-stack-rs

A small, single-core Rust networking stack for learning and measuring Linux
AF_XDP and copying backends. The long-term target is 10 Gb/s on one core;
**that is a goal, not a measured hardware result**.

The device backends use handwritten libc/kernel interfaces. Packet handles
own their arena through a non-atomic reference count, so they can safely outlive
a moved or dropped device without cloning payloads.

## Implemented

| Backend | Platform | Data path |
| --- | --- | --- |
| AF_XDP (`xdp`) | Linux | RX/TX/FILL/CQ rings; strict zero-copy, forced copy, or kernel auto selection |
| TUN (`tun`, default) | Linux | Nonblocking read/write into a reusable packet pool |
| utun (`tun`) | macOS | Nonblocking read and scatter/gather write; optional kqueue shards |
| io_uring TUN (`io_uring`) | Linux 6.0+ | Experimental batched READ/WRITE, fixed file registration, owned buffers through CQ completion |
| Loopback | Both | In-memory copy model for tests and microbenchmarks |

`net::Responder` answers Ethernet ARP, IPv4 ICMP echo, and UDP echo in place.
The Ethernet parser handles up to two VLAN tags. Fragmented IP, IPv4 options,
IPv6/NDP, TCP, routing, multi-buffer XDP and shared-UMEM/multi-queue dispatch are
not implemented. The device API is poll-based; it is not yet an async TCP socket
API. `af_packet` remains an empty compatibility feature.

## Start testing

```sh
cargo test --locked --all-targets --all-features
cargo bench --locked --bench throughput
cargo run --release --example device_bench -- \
  --backend loopback --action tx --batch 64 --size 1500 --warmup 2 --seconds 10
```

Linux: build once, then run a test isolated from the host's interfaces:

```sh
cargo build --locked --release --features xdp,io_uring --examples
sudo ./scripts/linux-veth-smoke.sh
```

The script creates two temporary network namespaces, tests ARP/ping/UDP through
generic AF_XDP copy mode, checks TX completions, saves JSON logs in `/tmp`, and
removes its interfaces/namespaces. It needs `ip`, `ping`, `ethtool`, and Python 3.
It **does not test driver zero-copy**.

For TUN/utun ICMP and UDP echo:

```sh
cargo build --release --example echo_server --example udp_load
sudo ./target/release/examples/echo_server
# Configure the printed interface in another terminal (instructions in example).
./target/release/examples/udp_load 10.9.0.2:9000 --count 10000 --window 32
```

See [benchmark methodology](docs/benchmarking.md), [VM/virtio-net experiments](docs/vm-xdp.md),
and [implementation notes](docs/design.md) before comparing results.

For a Mac host zero-copy experiment, try the new
[shared-memory NIC lab](docs/mac-shared-memory.md). `shm_nic` compares direct
shared-frame access against host/consumer staging copies across two processes,
with batched descriptors, completions and optional pipe notifications. The guide
also develops the QEMU igb, virtio/vhost-user and vmnet integration designs.

To run the actual guest-buffer experiment, follow the
[QEMU 11.0.1 vhost-user lab](docs/vhost-user-lab.md). It includes a QEMU queue-reset
patch, host launcher, guest configuration, `vhost_user_net` generator, and
`xdp_vm_rx` receiver with sampled physical-address verification. The host protocol
tests run locally; end-to-end AF_XDP validation requires your Linux guest.

For a reproducible guest kernel, the [QEMU kernel lab](docs/kernel-lab.md) builds a
small ARM64 kernel and initramfs inside a Multipass VM and exports a checksummed
bundle you can boot here. `scripts/kernel-lab-doctor.py` checks the prerequisites
first. The bundle is built for debugging: full DWARF, no KASLR and no modules, so
`scripts/run-kernel-lab.py --no-lab-nic --debug` plus `scripts/debug-kernel-lab.py`
can single-step the early ARM64 boot path from `primary_entry` through the MMU
handoff into `start_kernel`, using stock QEMU.

`scripts/qemu-lab.py` is the entry point for both labs on a fresh checkout: it
clones QEMU at the known-good tag, applies `patches/`, builds and code-signs it,
then offers the tests your kernel bundle supports and launches the one you pick.
Run it with no arguments. The kernel itself is built by `scripts/kernel-lab-build.py`
on whichever builder you choose — `--builder local`, `ssh`, `docker` or
`multipass` — and only the finished bundle is copied back.

```sh
cargo run --release --example shm_nic -- --mode direct --batch 64 --size 1500
```

## Ownership and backpressure

`recv(max, out)` clears/recycles the previous `out` and returns at most `max`
packets. `alloc_batch(max, out)` appends fresh TX handles. `send(frames)` accepts
a prefix and returns its length. Accepted slots become valid empty buffers;
**the unsent suffix stays yours to retry**. An error consumes no input frames.
This replaces the original drop-everything-on-send contract.

```rust
use async_net_stack_rs::device::{Device, PacketBuf};
fn flush<D: Device>(dev: &mut D, tx: &mut Vec<PacketBuf>) -> std::io::Result<()> {
    let accepted = dev.send(tx)?;
    tx.drain(..accepted);
    // Retry the remaining suffix after progress/readiness, rather than spinning.
    Ok(())
}
```

XDP and io_uring return submission acceptance, not wire delivery. Call their
`progress()` methods to drive/reap outstanding work, and inspect completions,
errors, drops, and peer counts. `PacketBuf::capacity()` includes headroom;
`tail_capacity()` is the writable packet capacity at the current data offset.

An XDP program redirects all traffic arriving on the selected queue to this
stack. Use an isolated interface, not the NIC carrying your management session.
Existing attachments produce an error; netlink cleanup checks program ownership.
Raw AF_XDP sockets need CAP_NET_RAW, and program/map setup also needs the relevant
BPF/network administration capabilities. TUN creation needs CAP_NET_ADMIN.

## Validation status

macOS tests, benchmark smoke runs, and Linux cross-compilation are recorded in
[local results](docs/local-results.md). Privileged Linux datapath tests are
provided but were not run on the development Mac. CI includes native Linux and
macOS tests plus an isolated veth integration job. Zero-copy NIC/virtio results
must be collected on your chosen guest kernel and host configuration.
