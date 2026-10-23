# Virtio PacketPool/Device adapter and portable UDP (D)

**Follow-on status:** [G now implements virtio-rng entropy](virtio-rng.md), sharing
this transport/ring machinery and supplying the explicit TCP sequence callback.

D is implemented in `baremetal/runtime/src/virtio/net.rs`. `VirtioNet<R>` is a
reusable polling Ethernet `Device` over the MMIO transport and split rings from
B/C. The boot binary now runs `UdpPool<EthernetIpv4<VirtioNet<Mmio>>>`: ARP,
IPv4 validation, UDP checksums, endpoint dispatch and echo all use the existing
portable core. The temporary `DemoFlight`/handwritten ARP fixture is removed.

The supported runtime remains one AArch64 CPU, caches/MMU off, no IOMMU and no
interrupt servicing. The adapter is reusable within that DMA domain; moving it
to physical hardware still requires a different platform mapping/cache contract.
No Linux driver code or new dependency was added.

## Ownership decision: one flight owner

C's `NetTransport<R, T>` already owns every submitted `T`, returns it on a valid
completion, and releases it only after confirmed device-wide reset. D uses that
mechanism directly. A private `Flight` holds two private `Lease` values: one for
the network header and one for the packet. Each lease retains its exclusive
`PacketBuf`, mapper and non-cloneable mapping.

The earlier plan proposed wrapping the portable `TxQueue`/`RxQueue` with new
`TxEngine`/`RxEngine` implementations. Those queues also retain packets and
mappings, and independently call their engine's stop method. Layering them over
C's retained owners would require another shared registry and coordinated stop
handles, or a change to their existing contracts. Keeping ownership in the ring
avoids that duplication and gives RX/TX exactly one reset authority. The portable
queues remain unchanged and useful for other engines. D reuses their `Device`
semantics, `PacketBuf`/`PacketPool` and TX/RX mapping contracts, and exercises the
same ownership cases against this concrete adapter.

The lifecycle is:

1. Allocate a private pooled header; encode/clear its twelve bytes.
2. Map the packet and header in their DMA direction, then synchronize for device
   access. The private leases retain both handles before publication.
3. Publish the two-descriptor chain through `NetTransport::submit`.
4. On rejection, restore the exact caller packet and release mappings/header.
5. On valid completion or confirmed reset, synchronize for CPU access and unmap
   **before** either packet can be inspected or returned to a pool.

A lease destructor is valid only before publication or after quiescence. That
invariant is enforced by keeping leases private and transferring the entire
flight to the ring. An active ring whose reset fails quarantines its flights,
ring memory and metadata together; their lease destructors never run during DMA.
The allocator and backing regions remain retained by those owners.

`shutdown()` is terminal and device-wide. A timeout retains all in-flight work;
retrying is allowed. Confirmed reset counts outstanding TX/RX as cancelled,
clears ready packets and releases the rings. Pool arenas stay allocated until
the adapter and all external packet handles drop. A received packet remains
valid even after the adapter is gone. Drop uses the transport's reset/quarantine
path. Restart by constructing a new adapter after successful shutdown/drop.

## Packet pools and hot-path costs

Default DMA allocation, assuming QEMU accepts 64 descriptors in each queue:

| Storage | Capacity | DMA pages (4 KiB) |
| --- | --- | --- |
| TX packet arena | 64 × 2048 bytes | 32 |
| RX packet arena | 64 × 2048 bytes | 32 |
| Header arena | 64 × 64 bytes | 1 |
| RX/TX split rings | 64 descriptors each | 2 |
| Total | 268 KiB | 67 |

Header storage is pooled rather than allocating a page for each twelve-byte
header. Separate TX and RX packet reserves prevent posted RX work from consuming
all application/control TX storage. One shared header pool is sized for the
maximum simultaneous RX and TX chains, so neither direction can exhaust the
other's share. Actual RX capacity is the smaller of configured RX buffers and
half the negotiated RX descriptor count.

TX maps exactly the current payload; arbitrary valid headroom is preserved. RX
maps the empty packet's tail starting at configured headroom (128 bytes by
default, leaving 1920 writable bytes). Its header mapping covers an isolated
64-byte frame, while the descriptor exposes only twelve bytes. No live device
buffer is accessed through a Rust slice until completion/reset and unmapping.

The send path accepts packets from either private packet arena. UDP echo can
therefore rewrite the received packet in place and submit that same storage as
TX. It is not copied through a staging buffer. Foreign heap pools and handles
from a previous adapter instance are rejected without consuming them. No
`zerocopy` parsing crate is needed: explicit header bytes and linear owners solve
different problems from deriving byte-layout traits; see the
[A rationale](dma-packet-pool.md).

Ring bookkeeping, header/packet pools and the RX-ready deque are preallocated.
Adapter publication/completion/refill does not allocate per packet. The caller's
output `Vec` can grow if it lacks capacity, and upper protocol layers still own
their normal queue/neighbor allocations. Heap metadata allocation failure uses
the existing panic path; this does not make the entire stack fallible-allocation
or allocation-free.

## Device semantics and replenishment

`send` consumes only an accepted prefix, leaving valid empty placeholders.
Descriptor/header pressure returns a short `Ok(count)` without waiting. An error
before any acceptance consumes nothing; an error after a prefix returns that
count and retains the suffix. Transport faults are latched in `fault()` and
reported by subsequent progress calls. A fault during notification cannot turn
an already published packet into a rejection. TX completion means local DMA
ownership ended, not remote delivery; its used length is not a byte count.

`poll_at` rejects backward time and performs bounded TX and RX completion work.
`recv` clears/recycles its previous output and also drives progress. Each progress
call handles at most `Config::budget` completions per direction and attempts at
most that many RX posts. After delivering packets, `recv` makes one additional
bounded refill pass. Call progress even while idle; there is no background task.
`next_deadline` remains the default `None`, not a promise that it is safe to sleep.

For RX, `posted + ready <= rx_target` always holds. Ready packets retain that
budget until delivered. Application-held packets consume their RX-pool frames,
but not ready slots; spare frames can replace them. If the application retains
all RX frames, replenishment pauses without spinning or stealing TX frames.
Dropping them or completing an echoed TX makes them available on the next poll.

`recv` never returns more than its requested limit. If a transport fault occurs
during the refill after delivery, it returns the already delivered count and
latches the fault; the next progress call reports it. Calling `recv(0, out)` still
recycles old output and drives bounded progress.

Polling reads/acknowledges advisory interrupt status. Configuration-change status
refreshes the cached MAC/link fields exposed by `net_config()`. This is not GIC
interrupt integration or automatic upper-layer address/link reconfiguration.

## Header and frame policy

This adapter uses the existing feature allowlist: modern VERSION_1 plus MAC,
with optional link status. Offloads, mergeable buffers, packed/indirect rings,
multiqueue and event-index optimization remain declined.

The network header is twelve bytes. TX initializes it to zero. RX subtracts that
size with checked arithmetic and accepts Ethernet lengths from 14 through 1514
bytes. The upper Ethernet adapter handles protocols and minimum-frame padding;
the NIC adapter supplies no FCS. VLAN/jumbo operation is outside this milestone.

RX rejects recognized checksum/GSO/RSC semantics that were not negotiated and
rejects nonzero GSO type. Unknown flag bits and inactive checksum/GSO fields are
ignored. `num_buffers` accepts zero (QEMU compatibility) or one, and rejects
larger values without attempting to merge packets. Zero never means “wait for
another buffer.” QEMU can leave this field zero in non-mergeable mode; modern
header size remains twelve. The [Virtio 1.2 network section](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html)
and [QEMU receive implementation](https://gitlab.com/qemu-project/qemu/-/blob/v9.2.2/hw/net/virtio-net.c)
provide the protocol/compatibility references.

Malformed header or short/oversized Ethernet length within the published DMA
capacity drops that completed packet, increments `rx_invalid` and permits refill.
A used length beyond the published writable capacity, invalid descriptor ID or
impossible ring progress faults the transport and retains the suspicious owner
until reset. Length checks cannot contain a device that writes outside its
advertised range; the conforming-device trust boundary from C still applies.

## Validation and how to run it

```sh
python3 scripts/qemu-virt-net.py                  # show serial and verify UDP
python3 scripts/qemu-virt-net.py --smoke          # full debug suite
python3 scripts/qemu-virt-net.py --smoke --release
cargo test --locked --manifest-path baremetal/Cargo.toml -p async-net-baremetal-runtime
cargo test --locked -p async-net-stack-core
```

The runtime suite has 45 tests, including nine adapter tests using a synthetic
MMIO peer that consumes the actual split rings and accesses only their published
ranges. They cover accepted-prefix/full/retry behavior, foreign-buffer rollback,
reordered/delayed TX completion, exact headroom/address mapping, same-buffer RX
→ TX echo, held RX pool exhaustion/replenishment, bounded completion/ready work,
malformed headers/lengths, setup allocation failure, token exhaustion, faults
before/after publication, shared reset retry, live-handle survival and failed-Drop
quarantine. Existing C tests retain index-wrap and descriptor-corruption coverage.

The Python peer uses the host OS UDP stack, separate from the guest code. It
compares every returned payload byte, sends 128 datagrams in each of two rounds,
and verifies sizes from zero through 1472 bytes (the 1500-byte IPv4 MTU).
After the first exchange resolves ARP it keeps up to eight datagrams outstanding.
There are no retransmissions to hide loss. Missing, duplicate, unexpected or
modified echoes fail the run, as do timeout, guest panic and early QEMU exit.

Guest readiness is `NET READY: round=N UDP=10.0.2.15:9000`. Once a round's echoes
are verified, the peer sends `ANS-DONE` followed by the single round byte. The
guest echoes that terminator, drains pending TX, asserts replenishment and
completion counters, resets with RX still posted, and checks that **all** DMA
pages return after owners drop. It then repeats with fresh pools/rings and tokens.
The guest watchdog is 60 seconds; the host's complete UDP run is bounded to 45.
Absent-device, legacy-device and heap-allocation-failure checks remain in `--smoke`.

The runner binds an ephemeral UDP forwarding port only on `127.0.0.1`, using
[QEMU user-mode host forwarding](https://www.qemu.org/docs/master/system/invocation.html).
It needs neither TAP nor administrator privileges. **It now uses `restrict=off`.**
On the tested QEMU 11.1.1/macOS build, `restrict=on` accepted host-forwarded UDP but
discarded the replies before they reached the host socket. An Ethernet capture
showed correct checksummed guest replies, and disabling restriction made the
same traffic pass. Ordinary user networking permits guest outbound traffic;
loopback binding limits the forwarding listener, not guest egress. This is an
observed backend limitation, not a claim that every QEMU/libslirp version behaves
that way. A restricted socket/TAP packet peer remains an E enhancement.

## Remaining work and useful human work packages

D completes the reusable polling adapter and portable UDP path. The original
bare-metal objectives still need these platform and validation pieces:

- **E — expand the independent peer.** The runner now supplies a baseline. Add
  capture artifacts and an independent Ethernet peer for invalid IPv4/UDP
  checksums, truncation, fragmentation, wrong endpoints, loss/reordering and
  sustained burst/backpressure tests. Add guest-originated UDP initiation and
  reset-under-load scenarios. Keep readiness/termination separate from the DMA
  implementation. Real-QEMU pressure is not inferred from the eight-packet window;
  deterministic descriptor/pool exhaustion is currently covered by host tests.
- **F — interrupt runtime.** Implement GIC setup and resumable vectors, signal
  bounded work to the main loop, and define a race-free sleep/wakeup sequence.
  Keep pool/transport mutation in the single main-loop owner. Extend adapter
  notification policy only when the IRQ scheduling contract is ready; requesting
  suppressed completion interrupts is still the polling baseline.
- **G — implemented.** Bounded virtio-rng buffering, shared typed transport and
  the explicit TCP sequence source are covered in the [entropy design](virtio-rng.md).
  The extended runner verifies short entropy responses and UDP without an RNG.
- **H — hardware runtime.** Device-tree discovery/reservations, MMU attributes,
  cache maintenance/coherency and a selected physical board/NIC remain separate.
  Implement platform-specific DMA mapping/synchronization and validate ownership
  under physical cache and interrupt conditions before enabling caches here.

Suggested division: human E; assistant F next. Keep E in the runner/peer,
F in GIC/vector/runtime modules, and review changes to the adapter's polling/reset
interfaces together. The [work plan](virtio-net-plan.md) has the broader breakdown.

