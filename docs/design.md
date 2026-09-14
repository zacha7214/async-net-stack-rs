# Implementation notes

## Frame ownership

The pool owns a single zero-initialized arena and a bounded index free list.
`PacketBuf` holds a non-atomic `Rc` to the pool; XDP pools additionally retain the
UMEM mapping. Safe slice access includes the entire initialized frame. Handles
are exclusive and remain `!Send`/`!Sync`. Pool metadata stays valid if the device
moves, and packets remain usable after the device drops.

A successful send replaces accepted handles with empty, zero-capacity values.
There are no manually dropped-but-still-live Rust values, cloned raw owners, or
accessible stale packet pointers. The copying backends consume only successfully
written packets; XDP and io_uring transfer buffers into completion-owned state.

## AF_XDP changes

* A configurable free-frame TX reserve replaces fill-ring reclamation. Once a
  descriptor is published, the kernel can cache it before advancing its shared
  consumer. Moving FILL's producer backwards could put one frame in RX and TX
  simultaneously. Refill now respects both a target of outstanding RX frames
  and the free TX reserve.
* RX packet offsets come from descriptors, including kernel XDP headroom.
  Completion addresses map back to the containing aligned frame. Invalid
  descriptors are counted/quarantined instead of creating aliased handles.
* TX and FILL cache consumer positions and refresh them only when cached space
  is insufficient. Index arithmetic wraps across u32 rollover. Descriptor
  writes happen before release publication; consumption follows acquire loads.
* RX wakeup uses `poll(POLLIN, timeout=0)`; TX uses the sendto kick. Flags are
  checked at batch boundaries and while driving idle/in-flight work.
* Reused scratch vectors remove per-send allocation. Sends are bounded by
  batch/ring space and never wait indefinitely for completions.
* Auto bind omits COPY/ZEROCOPY flags so the kernel chooses the mode. Copy and
  ZeroCopy requests are strict. The accepted mode is confirmed with XDP_OPTIONS.
* The redirect program is attached before binding, allowing drivers to prepare
  native XDP resources first. Redirect remains disabled until the socket and
  fill ring are ready. An unrelated existing program does not imply our new
  XSKMAP is wired into it.

These ownership and wakeup rules follow the kernel's
[AF_XDP documentation](https://docs.kernel.org/networking/af_xdp.html).
The implementation remains single queue, aligned chunks, and single-buffer RX;
it never enables XDP_USE_SG. A small fill ring no longer loses excess pool frames.
For RX-only work, `tx_reserve=0` is allowed. For TX-only work, use a larger reserve
and a smaller fill ring; published RX buffers cannot be taken back on demand.

`attach=false` still creates a private program/map. Use `program_fd()`/`map_fd()`
to integrate that exact program/map externally. Attaching an unrelated redirect
program without updating its map will not deliver traffic here.

## io_uring experiment

A separate Linux TUN backend queues one READ or WRITE SQE per datagram and
publishes many SQEs in one `io_uring_enter`. It registers the TUN fd once. READs
are kept outstanding up to `rx_depth`; TX uses the remaining request capacity.
Packet ownership lives in a reusable request table, keyed by CQE user_data.
Ready RX packets count against the RX budget, preventing unbounded buffering.

No READV/WRITEV operation is misused to represent several datagrams: a vector
operation on TUN is still a single packet. No SQPOLL thread is enabled. Both
mappings and all request buffers remain owned until completion.

The backend probes Linux 6.0+'s synchronous cancellation API before submitting
anything. Shutdown cancels submitted work before dropping buffers; if cancellation
fails or exceeds its one-second timeout, outstanding handles are deliberately
quarantined (leaked) rather than freeing memory the kernel might still access.
Unsubmitted SQEs cannot start during shutdown because this is not an SQPOLL ring.
See [io_uring cancellation](https://github.com/axboe/liburing/blob/master/man/io_uring_register_sync_cancel.3)
and [io_uring UAPI](https://github.com/torvalds/linux/blob/master/include/uapi/linux/io_uring.h).

`send` reports accepted requests. `progress`, the next `recv`, or the next `send`
reports deferred completion/submission failures. Stats distinguish submitted
requests, completed packets/bytes, errors, and enter calls. Successful writes
can complete out of order; these APIs do not promise ordered UDP delivery.

Batching reduces potential syscall cost, **not TUN's payload copy**. At low load,
extra ring bookkeeping can cost more than direct read/write. Registered buffers,
SQPOLL, multishot receives, and io_uring ZC RX are separate future experiments;
none is silently enabled or claimed here.

## macOS

The address-family prefix is network byte order, as required by
[XNU's utun ABI](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/net/if_utun.c).
Two iovecs send that prefix plus the existing packet without changing headroom.
The kqueue benchmark updates statistics once per batch. macOS affinity tags are
advisory and do not pin a worker to an exact CPU.

## Next modules worth implementing

1. A shared XSKMAP/program manager and one worker per RX queue, with explicit RSS
   steering. Separate devices cannot each attach their own program to one NIC.
2. A bounded ARP/neighbor cache and routing table, with timers and packet queues
   that retain handles safely under backpressure.
3. UDP socket demultiplexing and an executor-facing readiness API. Keep timeout
   ownership separate from device buffers before introducing TCP connection state.
4. Multi-buffer XDP: capability negotiation, descriptor continuation chains,
   bounded scatter/gather packet views, and completion tests before enabling SG.
5. An AF_PACKET baseline using sendmmsg/recvmmsg or TPACKET_V3, then compare it to
   generic XDP copy without conflating syscall and copy costs.
