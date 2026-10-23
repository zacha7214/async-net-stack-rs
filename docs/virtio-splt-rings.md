# Split descriptor rings, publication and ownership (C)

**Current status:** D replaces the historical ARP fixture described below with
pooled packet/header flights and portable-stack UDP. See the
[adapter design](virtio-net-adapter.md) for current boot checks and remaining work.

C is implemented in `baremetal/runtime/src/virtio/split.rs` and integrated into
`NetTransport` in `virtio/mmio.rs`. The guest now submits actual descriptor chains
to QEMU, acquires TX and RX completions, exercises descriptor pressure/reuse and
validates an ARP reply from QEMU's user-network gateway. This is a ring-level smoke
fixture from the C milestone; D now supplies the reusable PacketBuf/Device adapter.

## Components and ownership

| Component | Responsibility |
| --- | --- |
| `RingMemory` | Volatile little-endian field access and device-ordering barriers |
| `DmaRingMemory` | Retain the DMA region and a raw CPU pointer captured before publication |
| `SplitRing<M, T>` | Descriptor free list, available/used indexes, token mapping and retained submission owners |
| `NetTransport<R, T>` | Shared RX/TX lifecycle, notification, corruption handling and confirmed reset |
| `DemoFlight` in the boot binary | Temporary owned header/payload buffers for the ARP smoke fixture |

The storage interface allows the exact ring algorithm to run against an instrumented
host image. The device-backed implementation never recreates a mutable slice over
live ring memory. It accesses individual fields through aligned volatile operations.
Payload synchronization and device-domain validity remain explicit unsafe contracts.

`T` is the submission's owner. Submission takes it by value; an error returns
`(error, owner)` without publishing anything. Success retains it in a private flight
entry until completion returns it or confirmed reset drops it. This supports owned
header/payload regions now and an adapter-specific owner type later. No generic
owner can prove that a numeric descriptor address belongs to it: `submit` is unsafe
and requires the caller to retain the actual backing allocations, preserve exclusive
packet ownership and finish payload synchronization before publishing.

## Descriptor and metadata layout

One contiguous DMA region contains the descriptor table, available ring and used
ring. Layout calculation validates a power-of-two queue size up to 32768, includes
space for the unnegotiated event fields, and preserves descriptor/used-area
alignment. Construction zeroes the region before exposure. Polling mode requests
suppressed completion interrupts; spurious interrupts remain possible.

CPU-only metadata is allocated once at ring construction:

- One optional flight per possible head, holding token, descriptor count, total
  writable capacity and the owner.
- One link per descriptor, shared by the free-list and active-chain bookkeeping.
- Bounded counters for free descriptors and outstanding chains.

Submission and completion perform no heap allocation of their own. Owners may be
allocated by callers before submission. The original C fixture allocated separate pages
for clarity; D now uses pooled packet/header storage.
Completion follows CPU-maintained links, never device-visible descriptor links.

Direct chains support device-readable buffers followed by device-writable buffers.
Zero lengths, overflowing address ranges, more descriptors than the queue, reversed
access direction, and combined lengths above `u32::MAX` are rejected. Descriptor
exhaustion is a normal `Full` result returning ownership. Indirect descriptors,
packed rings, event-index optimization, IN_ORDER and multiqueue remain disabled.

## Publication and notification

The ordering rules come from [Virtio split-queue operation](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html),
section 2.7. The implementation separates the commit point from notification:

1. Validate everything that can reject the request.
2. Fill descriptors and install the retained owner/token bookkeeping.
3. Write the head into the available ring.
4. Apply the publication barrier, then publish the wrapping available index.
5. Apply the notification barrier, then read the device's suppression flag.
6. The transport kicks the queue if requested.

Once the available index changes, ownership has transferred even if notification
is suppressed. There is deliberately no fallible device-status read after this
point: a new device error must not turn accepted work into an ordinary rejection.
A dedicated test makes the fake device request reset during notification and checks
that submission still reports acceptance and retains the owner.

The AArch64 backend conservatively uses `dsb sy` with a compiler memory clobber for
all three barrier classes. This applies only to the existing caches-off QEMU DMA
domain. It is not a cache-maintenance implementation or a physical-board proof.
A non-AArch64 fallback fence exists for inert host tests, not as another hardware port.

## Completion processing and faults

Each poll handles at most one completion, allowing the caller to impose a budget.
It reads the device's used index, validates the wrapping progress against the number
of outstanding chains, and acquires memory before reading the entry and returning
payload ownership. Completion validates that the ID is an active chain head.
Writable chains also bound the reported byte count by their total writable capacity.
For readable-only chains, used length is returned as raw metadata and deliberately
ignored for validation: it is not a TX payload-length or delivery report.

Only a valid completion removes its flight, returns the exact chain to the free
list and advances the consumed index. Reordered completions are supported. Invalid
IDs, completion entries naming descriptor tails, impossible index progress and
oversized writable lengths latch a fault without returning the suspicious owner.
The transport faults both queues and requires reset before further operation.
This is stricter than ignoring arbitrary corrupt entries because we cannot safely
infer which descriptor ownership the device actually relinquished.

Address validation cannot prevent a broken device from writing outside a descriptor,
and length checks cannot undo such a write. Truthful DMA bounds and completion
remain part of the unsafe device contract.

## Tokens, wraparound and the trust boundary

Accepted tokens strictly increase within a ring lifetime. Rejected requests do not
reserve a token, so a retained request may retry with the same token. The portable
queue adapter may keep its stronger existing rule of allocating a new token for
every attempt. Ring indexes wrap at 65536 independently from the 64-bit tokens.
After confirmed reset, a new ring begins with zeroed indexes and a fresh token map.

Virtio returns a reusable descriptor-head ID, not our token. The map rejects an
inactive head or tail, and duplicate completions are caught while the ID is inactive.
It **cannot distinguish a malicious late duplicate from a valid completion after
that same head has been reassigned**. Nor can it detect a device falsely completing
a currently active head. This is an explicit conforming-device trust boundary, not
a claimed generation-counter protection. Reset must end all old access before ring
storage/IDs are reused across device lifetimes.

## Shutdown and quarantine

The transport marks a ring active before publishing its queue addresses. Active
ring Drop quarantines the entire resource bundle, including ring memory, metadata
and outstanding owners, unless quiescence has explicitly been established. This
also protects empty rings: the device may still inspect their indexes.

A successful device-wide reset proves both queues quiescent. The transport then
marks each ring quiescent and releases ring/payload owners. A timeout retains them
and disables ordinary work; a later reset can be retried. Dropping after failed
reset intentionally leaks the retained bundle. A partial setup or ring corruption
uses the same rule. These paths do not need per-owner cancellation callbacks yet;
D reports cancellation in its adapter counters after confirmed shared reset.

## Validation

```sh
python3 scripts/qemu-virt-net.py --smoke
python3 scripts/qemu-virt-net.py --smoke --release
cargo test --locked --manifest-path baremetal/Cargo.toml -p async-net-baremetal-runtime
```

The 36 runtime tests now include instrumented publication/acquisition ordering,
descriptor layout, full/retry/reuse, reordered completions, invalid chains and tokens,
malformed used IDs/lengths/progress, suppression, quarantine, and mixed chain sizes
checked against an independent live-set model. A 65,540-submission test crosses
16-bit index wraparound. Transport tests cover a device error after publication,
corruption stopping the peer queue, owner retention on reset timeout and release
only after reset confirmation. Fake-device tests are synchronous and do not claim
to model physical cache behavior.

Each QEMU initialization round uses 64 descriptors per queue and two descriptors
per submission. It posts 32 RX chains, submits 32 ARP requests, confirms that a
33rd TX request returns `Full`, then retries that retained owner after a completion.
It verifies all 33 TX completions and at least one matching gateway ARP reply,
then resets with any remaining RX work retained and verifies all DMA pages return.
The entire sequence repeats after reinitialization. The absent/legacy-device and
heap-OOM checks remain. No TAP device, root access or external network is required.

A compatibility detail for D: modern headers are 12 bytes here, but QEMU can leave
`num_buffers` zero when mergeable RX buffers are disabled. The fixture ignores that
field in this mode and validates the single-chain completion bound. QEMU's
[receive implementation](https://gitlab.com/qemu-project/qemu/-/blob/v9.2.2/hw/net/virtio-net.c)
illustrates conditional population of this field. The spec's modern field convention
and this observed behavior should remain documented when defining strict versus
compatible header validation; never use the field to enable unnegotiated merging.

## Remaining work

The following C handoff checklist is now implemented by D (with direct flight
ownership replacing the proposed portable-queue wrappers):

1. Replace the smoke fixture with reusable Virtio network-header encoding/validation
   for the exact negotiated features, including the compatibility policy above.
2. Integrate the DMA-backed `PacketPool`, TX/RX mapping hooks and portable ownership
   queues. Define one flight-owner structure and shared reset authority; avoid
   duplicating packet ownership or freeing header storage separately from a chain.
3. Preserve batch accepted-prefix and no-consumption-on-error semantics at `Device`.
   Ring submissions currently accept one whole chain at a time.
4. Convert RX total written bytes into payload length with checked header subtraction;
   apply all packet/header validations before exposing bytes. Replenish RX buffers.
5. Run the existing portable ownership scenarios against the composed driver, including
   failed publication, delayed/error completion, malformed RX and shutdown behavior.
6. Exercise ARP/IPv4/UDP through the portable stack, rather than the boot fixture,
   with an independent packet peer and sustained queue-pressure/reset tests.

Interrupt routing and race-free wakeups (F), virtio-rng/TCP entropy (G), device-tree
and cache-enabled/physical hardware work (H) remain separate. C supplies the ring
mechanics they can build on, not those platform or protocol integrations.

