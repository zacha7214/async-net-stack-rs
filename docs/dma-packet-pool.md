# DMA-backed PacketPool: implementation and rationale

## Decision

Use one physically contiguous, page-aligned DMA arena and let the existing
`PacketPool` divide it into fixed-size frames. `PacketBuf` continues to provide
exclusive packet ownership, headroom, recycling and the same `Device` interface.
The new unsafe `PacketPool::from_owned_region` constructor takes a CPU pointer,
initialized allocation extent, frame dimensions and an `Rc<dyn Any>` storage owner.
Outstanding packets retain the arena owner through the existing pool reference.
It neither copies payloads nor exposes raw frame-index ownership to applications.

The QEMU runtime's safe `OwnedDmaRegion::into_packet_pool` method encapsulates that
unsafe call and returns a pool plus a `PacketDmaMemory` mapper for the same region.
Conversion consumes the region, removing its direct mutable CPU-access API. The
mapper and mappings retain private references to the region; mappings expose no
payload view. The allocator itself is retained by every region. Final region drop
returns the pages once; the underlying `PageRun` never escapes this wrapper.

This is the chosen efficient integration because it preserves the ownership model
already tested by the stack, avoids a bounce buffer and avoids allocating a mapping
object on every packet. Mapping clones an existing non-atomic `Rc`, checks a range
and constructs a small value. Packet recycling continues to reuse the same frames.
The arena is zeroed once per region allocation, not on every packet allocation.
Pool bookkeeping and protocol containers still use the ordinary heap; this design
is not a heap-free stack and does not make metadata allocation fallible.

## What A now supplies

- Independent linker reservations: 2 MiB heap, 4 MiB DMA page pool, separate from
  kernel, stack and the base-of-RAM device-tree reservation.
- A bounded, reclaiming global allocator with 64-byte granules, alignment handling
  and external bitmap metadata. Its first-fit scan is bounded, not constant-time;
  small allocations waste up to 63 bytes, and fragmentation can prevent larger
  contiguous allocations. This deliberately simple bring-up allocator is not a
  claimed high-performance general allocator. Pooling keeps it off the per-packet
  payload path. A serialized wrapper supplies `GlobalAlloc`; IRQs must not allocate.
- Allocation exhaustion returns null to Rust's allocation machinery. Fallible
  `Vec::try_reserve` reports failure. Infallible allocation failure reaches the
  default no_std allocation-error path and our serial panic-and-halt handler;
  the `heap-oom-test` guest verifies this path. Panicking does not reclaim DMA.
- A non-Copy `OwnedDmaRegion`: contiguous pages, requested CPU/device alignment,
  zero initialization and automatic reclamation. Invalid requests leave no live
  allocation; invalid packet-pool dimensions return the owned pages.
- Separate CPU pointers and `DmaAddress` values. The guest explicitly claims an
  identity-mapped, no-IOMMU domain; the code does not cast arbitrary packet pointers
  into device addresses. Host tests intentionally use different CPU/device bases.
- TX maps precisely the current payload; RX obtains writable provenance from the
  mutable whole-frame slice and maps exactly the tail after headroom. Empty ranges,
  nonempty RX packets, foreign heap packets, overflow and out-of-arena ranges fail.
  Descriptor lengths and addresses are bounded to the initial 32-bit device domain.
- DMA packet frames must be multiples of 64 bytes, isolating the Cortex-A53's
  cache-line boundaries at the frame level. Headroom need not be cache-line aligned.
  No cache-enabled safety claim follows from alignment alone.

All allocations and queues remain single-core at the packet layer. The heap's lock
does not make the `Rc` pools or the DMA allocator thread-safe. Device access is
allowed only through the unsafe publication/ownership contracts; obtaining a device
address is not permission to keep using the CPU slice while DMA is active.

## Why not the zerocopy crate here?

The [zerocopy crate documented by the Rust kernel](https://rust.docs.kernel.org/zerocopy/index.html)
provides checked byte/type conversions and layout traits. Its
[upstream documentation](https://github.com/google/zerocopy/blob/main/zerocopy/README.md)
also describes endian-aware integer types. Those tools can help represent network
headers or descriptor snapshots with less handwritten casting.

They do not supply DMA-capable memory, pin an arena's lifetime, translate CPU to
bus addresses, perform cache maintenance or establish device ordering. Those are
the problems this milestone solves. Adding the crate here would not eliminate the
unsafe platform contracts or improve packet storage ownership, so no dependency
was added. We can evaluate it separately when implementing Virtio header layouts.
Even then, a byte-layout trait does not authorize ordinary Rust references to
memory concurrently modified by a device, or replace volatile/ordered ring access.

“Zero-copy payload storage” and “using the zerocopy crate” are independent choices.
This implementation achieves the former at the pool/mapper boundary without the
latter. Actual network zero-copy behavior awaits the descriptor-ring driver.

## Alternatives considered

1. **Put all heap memory in the DMA domain.** Easy initially, but blurs device
   addressability and lifetime, complicates physical-board migration, and risks
   cache-line sharing with unrelated metadata. Rejected in favor of a reserved pool.
2. **Copy every TX/RX packet through separate DMA buffers.** A reasonable compatibility
   fallback, but adds bandwidth cost and another ownership lifecycle. Our existing
   arena model already supports direct payload storage, so copying is unnecessary.
3. **Replace PacketBuf with a new DMA-only packet type.** Would spread platform
   details into protocols and duplicate tested ownership logic. The small constructor
   extension keeps the existing interface and host backends intact.
4. **One physical allocation per packet.** Unnecessary page fragmentation and
   bookkeeping for small frames. Allocate an arena once and recycle its frame slots.

## Lifetime and shutdown guarantees

Pool, packet and mapper references can outlive each other. The arena returns to the
DMA allocator only when the final retaining reference disappears. Each outstanding
mapping also holds a reference, independently of the mapper handle. A retained
packet therefore survives device drop; retained mappings cannot lose their storage.
The queue must still retain the individual packet until DMA ends, to prevent frame
recycling within an otherwise live arena. An arena reference alone does not prevent
that frame-level reuse; the existing TX/RX queues enforce this obligation.

An unconfirmed stop must never reclaim live mappings/packets. Existing queue Drop
quarantine retains the mapper, engine and flights together. The new mapper fits that
contract: leaked resources also retain their arena and allocator. The caller of the
unsafe domain constructor must reserve backing RAM for every surviving reference,
including leaked ones. There is no physical-device reset implementation in A.

## Cache and mapping boundaries

Cache hooks are explicitly empty only because the boot contract keeps caches off
for the lifetime of this domain. This is not a general coherent-DMA implementation
or an IOMMU mapping API. Descriptor publication/acquisition and MMIO ordering still
belong to the B/C transport/ring engine. Before enabling caches, implement
and validate a platform policy covering payloads, rings, rounded cache-line ranges,
CPU/device ownership transitions and adjacent data. QEMU tests do not validate
physical cache behavior.

The public owned-arena constructor remains unsafe because a generic library cannot
verify an owner's allocation, exclusive access, initialization or lifetime claims.
The QEMU wrapper establishes these locally using the reserved pool; ordinary heap
`PacketPool::new` remains valid for host use and is rejected by the DMA mapper.

## Validation and commands

```sh
python3 scripts/qemu-virt-net.py --smoke
python3 scripts/qemu-virt-net.py --smoke --release
cargo test --locked --manifest-path baremetal/Cargo.toml -p async-net-baremetal-runtime
cargo test --locked -p async-net-stack-core
cargo test --locked --lib device::tests
cargo test --locked --test dma --test dma_rx
```

The runtime suite covers alignment, fragmentation, exhaustion, randomized live-byte
preservation/nonoverlap, invalid domains/layouts, zero-on-reallocation, headroom,
exact RX ranges, foreign-memory rejection and arena retention by packets/mappings.
A TX ownership-queue test with an inert engine checks failed stop then confirmed
stop against this actual mapper. Host and portable device tests also cover the
public owned-arena lifetime. The existing scripted DMA scenarios remain regression
coverage; D adds corresponding ownership cases against the actual Virtio rings.

QEMU boots the portable core, exercises heap reuse and DMA-backed packet allocation,
checks mapping without publishing descriptors, and verifies an intentional heap OOM
halts through the panic handler. Modern, absent and legacy device probes remain.
The smoke command restores the normal guest binary after the OOM variant.

B and C build on this allocation layer; see the [split-ring design](virtio-split-rings.md).
D uses these owned arenas in the reusable Virtio Device, with pooled headers,
separate RX/TX reserves, bounded replenishment and direct RX-to-TX reuse. The
current QEMU runner verifies portable-stack ARP/IPv4/UDP traffic. See the
[adapter rationale](virtio-net-adapter.md) for the integration decision and limits.
G now adds [virtio-rng entropy](virtio-rng.md) using an owned request region.
Interrupts and physical cache support remain separate work packages.

