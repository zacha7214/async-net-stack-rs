# Modern virtio MMIO initialization (B)

G factors the lifecycle implementation into `Transport<R, T, QUEUES>`. The existing
`NetTransport` keeps its network API and two queues; `RngTransport` uses one queue,
device ID 4 and VERSION_1 only. Reset and retained-owner behavior remain common,
with separate ownership for each device. See [virtio-rng](virtio-rng.md).

This document records the B milestone's initialization design. B initialized
QEMU's modern virtio-net device, negotiated a small feature set, configured two
empty split queues, reached DRIVER_OK, read MAC and optional link state, and
performed a confirmed device-wide reset. Repeating that cycle tested
reinitialization and DMA-page reclamation before packet publication was added.

C now supplies [split-ring publication and completion](virtio-split-rings.md)
under the same reset owner. D connects those operations to the
[packet pool and portable stack](virtio-net-adapter.md). The current smoke command
exchanges ARP and UDP traffic and tests the independent entropy device from G.

Implementation: `baremetal/runtime/src/virtio/mmio.rs`. The existing probe selects
the device; `NetTransport` owns the register backend, DMA allocator and queue memory.
The register backend and allocator are paired through one unsafe constructor, which
requires exclusive ownership and the same caches-off, no-IOMMU device domain.
The transport never exposes its register backend or mutable ring storage afterward.

## Initialization and supported features

The sequence follows the [Virtio specification](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html),
sections 3.1 and 4.2:

1. Validate magic, modern MMIO version 2 and network device ID before status writes.
2. Reset and read status zero with a bounded poll count.
3. Set ACKNOWLEDGE, then DRIVER.
4. Read both device-feature words and write both selected-feature words.
5. Set FEATURES_OK and verify that the device accepted it.
6. Configure both queues with owned, zeroed storage.
7. Set DRIVER_OK only after both queues are configured.

Required: VERSION_1 and NET_MAC. Optional: NET_STATUS. Requiring a device-supplied
MAC avoids introducing a generated address and persistence policy in this milestone.
The driver declines offloads, mergeable RX buffers, packed/indirect rings, event
index, multiqueue, control queues, notification data and independent queue reset.
There is no PCI or legacy transport, IOMMU, interrupt routing or cache maintenance.

Missing required features and rejected negotiation put the transport into a failed
state. Device NEEDS_RESET/FAILED status stops further setup or notifications; an
explicit reset/reinitialization is required. Configuration reads bracket byte-wide
MAC and optional aligned 16-bit status reads with ConfigGeneration checks and retry
only up to the configured limit. An unstable configuration is reported to the caller.
The bootstrap limit is 1024 register reads/attempts, not a wall-clock timeout.

## Queue storage and ownership

`configure_queue(index, preferred)` accepts RX index 0 and TX index 1. It selects a
power-of-two size bounded by the preference, hardware maximum and the split-ring
limit. Zero capacity, already-ready queues and invalid indices are rejected. Each
queue currently has a single page-aligned allocation containing descriptor,
available and used areas with their required alignment. Trailing event fields are
reserved but event-index behavior is not negotiated. All indexes initially equal
zero, so the NIC sees no available buffers.

The transport stores the region owner before publishing any queue addresses. If
queue readiness is rejected or device status changes, that storage remains owned
until confirmed reset. Allocation failure occurs before queue addresses/readiness
are published. One transport controls both queues; there are no independently
resettable TX/RX handles. `queue_size` reports configured capacity without exposing
ring memory. C extends this owner with retained flights; D uses it for packet/header leases
rather than creating a second register/reset owner.

`reset` reads back status zero before releasing either queue arena. Success is
sticky until initialization restarts. On timeout it retains all queue memory and
blocks ordinary operations; a later reset can be retried. Drop attempts a bounded
reset and intentionally leaks queue owners if confirmation fails. Those owners also
retain the allocator. Timeout never means DMA memory is safe to reuse.

## Ordering and raw access

`Registers` is an unsafe implementation contract with unsafe raw access methods.
The state machine privately supplies valid access directions, field widths and DMA
addresses. The real AArch64 backend uses volatile accesses with `dsb sy` before and
after each access, without the compiler's `nomem` exemption. This conservatively
orders register operations against ordinary queue-memory initialization as well.
These full-system barriers can be optimized later with an explicit ordering proof.
They do not implement cache cleaning/invalidation or prove physical-board coherence.

The fake backend records every register operation and rejects unexpected access
widths/directions during tests. It tests state-machine sequencing, not CPU memory
ordering: architecture instructions and cache behavior remain separate obligations.

`notify` checks DRIVER_OK and writes the queue index. Interrupt status and ack APIs
mask undefined bits. Callers must finish handling observed events before acking;
the bootstrap can acknowledge empty-queue/configuration events after reading
configuration. IRQ delivery itself remains disabled and unimplemented.

## Validation and commands

```sh
python3 scripts/qemu-virt-net.py --smoke
python3 scripts/qemu-virt-net.py --smoke --release
cargo test --locked --manifest-path baremetal/Cargo.toml -p async-net-baremetal-runtime
cargo clippy --locked --manifest-path baremetal/Cargo.toml --workspace \
  --target aarch64-unknown-none-softfloat -- -D warnings
```

The host fake-device suite covers exact reset/negotiation ordering and feature
masking, invalid identity, missing/rejected features, device errors, configuration
width/retry behavior, queue alignment and publication order, queue-capacity errors,
allocation failure, rejected readiness, empty notification, bounded reset failure,
retry/reclamation and quarantine on Drop. The quarantine test deliberately leaks
its backing allocation because the failed device stop cannot authorize release.

QEMU debug/release smoke checks now require two successful initialization/reset
cycles, two 64-entry queues, DRIVER_OK, the configured MAC, and restored DMA-page
counts. Absent and legacy devices must not enter this modern initialization path.
Heap/DMA tests and the deliberate OOM panic variant remain in the smoke suite.
The current runner also verifies portable UDP traffic through D; see the
[adapter design](virtio-net-adapter.md). Fake-register tests isolate lifecycle
behavior, and none of these tests establish physical NIC cache correctness.

