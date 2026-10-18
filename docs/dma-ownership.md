# DMA transmit/receive ownership and fault injection

`device::dma` provides portable TX and RX ownership boundaries. It contains no register
access, physical-address assumptions, host clock or OS dependency. It builds in
both the host package and `async-net-stack-core`.

```sh
cargo run --locked --example dma_lab
cargo test --locked --test dma
cargo test --locked -p async-net-stack-core --test scenarios dma::
```

The demo submits a burst into a two-slot queue, delays a completion, rejects its
duplicate, retries the retained suffix, withholds another completion and exercises
both unconfirmed and confirmed shutdown. It checks free-frame counts at each step.
These are ownership events, not simulated network delivery or measured NIC timing.

## Three independently replaceable parts

| Component | Responsibility |
| --- | --- |
| `DmaMemory` | Map a packet payload to an opaque device address, synchronize caches, unmap |
| `TxEngine` | Publish descriptors, acquire completions, prove DMA quiescent at shutdown |
| `TxQueue<M, E>` | Retain packets/mappings, enforce bounded slots and prefix acceptance, process tokens and events |

`DmaAddress` is explicitly distinct from a CPU pointer. Mapping covers the payload
including its current headroom offset, not necessarily the whole allocation.
A real mapper must validate the memory, device-address domain, address width,
alignment and descriptor constraints. It may reject unsuitable packets.
Cache operations and descriptor barriers are separate responsibilities: cache
synchronization does not substitute for device/ring memory ordering.

`TxQueue::new` is unsafe because only the integrator can establish that the
mapper and engine refer to the same device/IOMMU domain, have no outstanding
external work and cannot be accessed behind the queue. The two implementation
traits also have explicit unsafe contracts. The simulated `simulation::dma::tx_queue`
factory pairs its synthetic mapper/engine safely and returns a separate fault
control handle.

## Ownership lifecycle

1. The caller owns a `PacketBuf` and may modify it.
2. Mapping and device synchronization prepare the packet, without publishing it.
3. Successful submission transfers the packet into a private flight slot. The
   caller's accepted slot becomes empty. The packet and mapping remain alive and
   have no CPU payload-access API while hardware owns them.
4. A matching completion proves DMA reads have ended. CPU synchronization runs,
   then unmapping, then packet recycling. Failure completions follow the same
   safe release path but increment the error counter.

A queue-full send returns an accepted prefix (possibly zero). Mapping or descriptor
publication failure preserves the rejected packet and suffix; `Err` consumes
nothing. If earlier packets were accepted, send returns their count and records
the later failure in counters. Publication errors must mean no descriptor became
visible to hardware. An ambiguous submission outcome cannot be reported as an
ordinary error by a real backend.

Tokens are monotonically assigned, never reused within the queue, and checked for
overflow. Duplicate/unknown/retired completions are counted and ignored. A stale
token cannot release a new packet in the same flight slot. This cannot detect a
backend falsely declaring a currently live token complete: truthful completion
and stop guarantees remain part of the unsafe driver contract.

`poll_at` supplies monotonic time and processes at most `completion_budget` events.
`next_deadline` lets a simulated engine integrate with the coordinator. Real
engines may return no deadline and be polled by the board runtime. Completion is
local DMA ownership release, not proof of wire delivery or remote reception.
`recv` is empty: this queue is not a complete NIC or a network loopback.

## Shutdown and observability

`shutdown` permanently stops acceptance. True means the engine has proved all
accesses quiescent and outstanding packets have been cancelled/reclaimed. False
retains all outstanding ownership. The caller may continue polling completions
or retry shutdown after device reset/stop becomes confirmed. A successful shutdown
is sticky and idempotent.

On Drop, failure to confirm stop quarantines the entire resource bundle by
intentionally leaking it: mappings, packets, mapper and engine. A timeout alone
never authorizes reuse. This fail-safe path trades memory for safety and is not a
reset/recovery mechanism. Use explicit shutdown and handle false before dropping.
Backend mapping, synchronization, submission, completion and stop methods must not
panic; panicking during an ownership transition is outside the unsafe contract.

Queue counters distinguish submission, completion errors, stale completions,
mapping/publication failures, cancellation and event overflow. A bounded event FIFO
records submitted/completed/stale/cancelled tokens; overflow drops oldest events.
The synthetic backend also keeps the most recent 256 cache/ownership audit events.

## Fault controls and remaining boundary

`simulation::dma` supports per-submission completion delays, error and duplicate
completions, withheld completions, rejected publication, mapping failure, explicit
stale completion injection and failed stop confirmation. Different completion
delays reproduce reordering. Packet arena size, flight slots and backend capacity
are independent limits for buffer-pressure experiments. Control scripts and pending
completion lists are bounded; all time comes from the scenario.

The mapper uses synthetic addresses and the engine never reads real DMA memory.
The same tests run against host and portable builds. The demo is separate from
packet-fabric propagation: a future NIC model can compose wire delivery and local
completion timing without treating them as the same event.

Real
DMA allocation/IOMMU mapping, cache implementations, descriptor rings, IRQ handling,
scatter/gather, and any migration of existing AF_XDP/io_uring backends remain
platform work. This initial adapter does not assert that ordinary heap memory is
DMA-capable, and it does not expose unsafe raw frame-index transfer to applications.

## RX posted buffers

`RxQueue<M, E>` adds a separate RX-only `Device`, using `RxDmaMemory` for
write-direction mappings and `RxEngine` for descriptors. It is shared unchanged
by the host and `no_std + alloc` package. OS backends remain separate.

Allocate empty packets from the pool or queue, then call `post(&mut frames)`.
Posting accepts a prefix, replaces accepted handles with empty placeholders and
preserves the rejected suffix. Buffers must have zero length and nonzero tail
capacity. Headroom is preserved: mappings start at the current payload offset and
cover exactly `tail_capacity()`. Drivers must obtain writable pointers from the
mutable allocation slice and preserve packet metadata. `RxQueue::new` is unsafe
for the same domain/pairing reasons as the TX constructor.

The queue privately retains each packet and mapping until a matching completion
or confirmed stop. Completion first performs CPU synchronization and unmapping.
Only successful lengths in `1..=posted_capacity` become packets; zero, oversized
(including integer-overflow-sized) and failed receives are discarded and counted.
Validation happens before setting packet length or exposing bytes. A driver must
still prevent actual out-of-range DMA writes: validating completion metadata cannot
undo memory corruption. For a valid successful receive, the driver guarantees
that the reported bytes were written by this receive.

Tokens are never reused, including after failed map/post operations. Unknown,
duplicate and retired tokens cannot release a current buffer. Slots bound the sum
of posted and ready packets, so undelivered traffic cannot grow an unbounded FIFO.
`poll_at` processes at most the completion budget; `recv` also processes one budget
at the last supplied time, then delivers up to its requested maximum. `recv(0)`
only clears its output. The caller explicitly replenishes empty buffers with
`post`; delivered packets remain owned by the caller until dropped. `send` is
unsupported; a NIC adapter can compose the independent TX and RX queues.

`shutdown` permanently prevents posting. Completed packets remain receivable;
unconfirmed stop retains posted mappings and allows completion polling or a later
shutdown retry. Confirmed stop cancels posted buffers. Drop without confirmed stop
quarantines the mapper, engine, mappings and posted packets. Stats distinguish
posting/mapping failures, failed receives, invalid lengths, stale completions,
delivery and cancellation; a failed completion with a bad length increments both
error counters.

```sh
cargo test --locked --test dma_rx
cargo test --locked -p async-net-stack-core --test scenarios dma_rx::
```

The test backend writes recognizable payload bytes only within the posted range,
even when reporting malicious lengths. Tests check headroom preservation, exact
capacity, recycling, cache-hook order, malformed/failed lengths, stale and reordered
completions, queue pressure, publication failures and shutdown quarantine. These
are ownership tests, not a hardware DMA or cache-coherency implementation.
