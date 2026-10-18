# Isolating platform dependencies

The workspace now builds the same protocol and simulation sources in two
contexts. The existing `async-net-stack-rs` package remains the host integration,
with its existing features and `std::io` API. `async-net-stack-core` in `portable/`
is a dependency-free `#![no_std]` + `alloc` library. It includes UDP/TCP engines,
wire formats, Ethernet/ARP/routes/neighbors, shared dispatch, packet arenas,
loopback, AP lifecycle and bandwidth/fault simulation.

```sh
cargo test --locked -p async-net-stack-core
cargo build --locked -p async-net-stack-core --target aarch64-unknown-none-softfloat
cargo build --locked -p async-net-stack-core --target riscv64gc-unknown-none-elf
```

Install those Rust targets first if necessary. These commands build libraries;
they do not boot hardware or validate a NIC driver. The portable package requires
Rust 1.77 for `core::net`. The host package retains its Rust 1.75 requirement by
using the identical address types through `std::net`.

## What the platform supplies

| Boundary | Host build | Portable build / integration responsibility |
| --- | --- | --- |
| Time | Caller supplies `Duration` | Same contract; supply a monotonic clock and polling loop |
| TCP initial sequence entropy | `/dev/urandom`, isolated in `src/platform.rs` | `TcpPool::with_sequence_source`; `new` returns `Unsupported` |
| Errors | Existing `std::io` types | Allocation-free error kind + static message in `platform::io` |
| Address types | `std::net` | `core::net`; no sockets or OS address configuration |
| Storage | Global allocator and reference-counted arenas | Provide a global allocator; same `PacketBuf` ownership machinery |
| Packet I/O | Existing Linux/macOS drivers | Implement `Device`; preserve accepted-prefix and no-consumption-on-error rules |
| Capture/configuration/reporting | Existing host tooling | Excluded; pass experiment inputs in from the embedding application |

`device::PacketPool` exposes bounded heap-backed packet allocation to custom
implementations of `Device`, without exposing unsafe raw frame-index ownership.
Outstanding packets keep their arenas alive. The pool is single-core (`Rc`) and
is not automatically suitable for DMA. Arena construction and protocol containers
still allocate; this is not a heap-free, interrupt-safe or allocation-fallible
stack. The application must provide allocation-failure and panic handling.

A platform sequence source must be appropriate for live TCP connections. A fixed
sequence is useful only for reproducible isolated simulations; the portable build
never silently substitutes it for entropy.

## Source and compatibility strategy

The portable package currently compiles shared files by explicit paths into
`src/`; it does not copy the algorithms or wrap the host crate. This is a
repository-local, unpublished build target, requiring the full checkout. Host
and portable types belong to different crates and are not interchangeable.
The duplicated pieces are the small platform boundary and module lists.
This avoids prematurely moving private backend buffer operations across a public
crate boundary while preserving the existing host API.

Portable errors deliberately support only the protocol/device operations in use,
not the full `std::io` API, OS error codes or boxed error sources. Driver-specific
diagnostics can remain on the driver, just as completion counters do today.
The portable manifest recognizes `xdp` solely for cfg compatibility in shared
buffer internals; it does not contain an AF_XDP backend.

The portable tests reuse the host scenarios for ARP/routing, UDP, coordinator
ownership/backpressure, AP teardown, and bandwidth faults, plus UDP discovery and
TCP partition recovery. They also verify explicit entropy and arena lifetime.
The test runner uses the host allocator; ARM and RISC-V library builds independently
verify that the production portable code does not depend on `std` or libc.
CI covers these builds and the portable package's minimum Rust version.

## Remaining work before hardware

The [TX/RX DMA ownership boundary](dma-ownership.md) and its scripted fault backend
cover TX mapping, completion delay/error/duplication, buffer pressure and teardown.
RX now covers posted-buffer ownership, received-length validation, stale completions,
bounded delivery and quiescent shutdown, with shared host/portable tests.
The following remain before hardware:

1. Implement actual DMA allocation/mapping, cache maintenance, device barriers and
   descriptor rings. Run the same ownership scenarios against that adapter
2. Supply a board runtime: allocator, monotonic clock, entropy, panic handling,
   polling/interrupt integration and NIC initialization.

Host DNS helpers, telemetry/JSON, capture ingestion and OS backends are still
outside the portable package. This milestone isolates the packet-processing and
experiment core; it does not make every host tool meaningful on bare metal.
