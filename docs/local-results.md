# Local validation: 2026-09-13

Environment: aarch64 macOS, Darwin 25.6.0. Performance runs used Rust
1.100.0-nightly (fd7ed57df 2026-08-29), Criterion 0.5.1, release optimization.
No CPU affinity or hardware performance counters were available in these runs.
These are short exploratory measurements, not publishable line-rate results.
The machine had no running Linux VM or Docker daemon, so no Linux kernel data
path was measured. CI jobs are supplied but have not been run from this checkout.

## Completed checks

* macOS `cargo test --all-targets --all-features`: 33 unit tests, 6 public API
  integration tests, and every Criterion benchmark smoke case passed.
* `cargo test --no-default-features`: passed (28 unit + 6 integration tests).
* Linux aarch64: all targets compiled with all features and XDP without the
  default TUN feature. Clippy passed with warnings denied on Linux and macOS.
* Formatting, whitespace checks, release examples, and shell syntax checks passed.
* Rust 1.75 compiled all targets/features for both macOS and Linux aarch64, using the exact locked dependency sources
  vendored temporarily outside the repository (the old Cargo could not read the
  current local registry index cache).
* The UDP load client exchanged 1,000 packets through a temporary localhost echo
  socket: 1,000 valid replies, zero loss, duplicates, or invalid payloads. This
  checks the client; it does not exercise TUN/XDP or establish network latency.
* `device_bench` completed a loopback TX/RX run, emitted parseable JSON, and
  reported no short sends or unsent frames. Batch-service sampling was exercised.

Privileged veth tests, real utun traffic, io_uring runtime/cancellation, native
virtio XDP, and NIC zero-copy remain **unmeasured locally**. The Linux io_uring
socket-pair test and isolated veth script are explicit runtime checks to run in
CI/your VM, not silently skipped “passing” hardware tests.

## Allocation batching

Same current implementation, single allocations versus `alloc_batch`, including
recycling the full batch. Twenty samples, 0.3 s warmup, 1 s measurement per case.
Point estimates in ns per complete batch:

| Frames | Individual calls | Batch API | Speed ratio |
| ---: | ---: | ---: | ---: |
| 1 | 3.94 | 8.40 | 0.47× |
| 8 | 28.93 | 18.40 | 1.57× |
| 32 | 128.01 | 58.89 | 2.17× |
| 64 | 260.79 | 109.94 | 2.37× |
| 256 | 1047.56 | 428.40 | 2.45× |

The batch API benefits multi-packet workloads. For one packet, plain `alloc()`
is faster. This is metadata allocation/recycling speed, not NIC throughput.

## Original workload before and after

The original benchmark code and identifiers were kept for this comparison.
Twenty samples, 1 s warmup, 2 s measurement per case. Point-estimate changes are
computed directly from saved Criterion estimates; they differ slightly from
Criterion's resampled change-distribution estimate.

| Workload | Before | After | Time change |
| --- | ---: | ---: | ---: |
| Allocate/recycle 256 individually | 1.037 µs | 1.046 µs | +0.84% |
| Copy roundtrip, 64 × 64 B | 1.268 µs | 1.310 µs | +3.31% |
| Copy roundtrip, 64 × 512 B | 3.589 µs | 3.620 µs | +0.86% |
| Copy roundtrip, 64 × 1500 B | 9.630 µs | 9.595 µs | −0.36% |

Safe arena lifetime and valid consumed slots add a small cost to the original
small-packet workload. Batching the loopback receive allocations reduced an
intermediate ~15% regression to ~3%. The 1500-byte benchmark showed no statistically
significant change. The old implementation contained unsafe ownership/ring
behavior, so its timing is not a correctness-equivalent target to preserve at
all costs. No Linux/XDP/io_uring speedup is claimed without runtime measurements.

The machine-readable estimates are in [local-benchmarks.json](local-benchmarks.json).
Full Criterion reports remain under the ignored `target/criterion` directory.

## Vhost-user guest-buffer lab

The new examples compile with all features on native macOS and for Linux ARM64,
including Rust 1.75; Clippy passes with warnings denied. Native Rust checks now
include two vhost memory/permission tests in addition to the existing 42 tests.

`scripts/vhost-user-smoke.py` passed four Mac pipe-notification configurations
(GPA/IOTLB addressing × contiguous/chained buffers) plus descriptor-loop rejection.
It uses real Unix sockets, SCM_RIGHTS, file mappings and C11 atomic ring indices.
The runs exercise fragmented control messages, unaligned file offsets, IOVAs
distinct from GPAs, access permissions, invalidation/refill, page-crossing rings,
16-bit index wraparound, disabled TX, and queue stop/restart at new addresses.
The Linux eventfd variants are supplied for native Linux/CI execution.

The evidence checker passed three tests with fabricated logs, including rejection
of contradictory addresses, missing sessions, drops and incomplete TX. The
launcher was exercised with a QEMU argument-checking stub. The QEMU patch applied
and reverse-checked against the downloaded upstream v11.0.1 source. These are
protocol/configuration checks: **no real QEMU guest AF_XDP run or throughput
measurement has been performed locally**. Use [the VM lab](vhost-user-lab.md) to
collect that evidence on the configured guest.

Subsequent Mac QEMU build check: reproduced the `linux/vhost_types.h` error in
`hw/net/vhost_net.c`, then verified the header portability patch with the native
compiler. Both lab patches apply to v11.0.1 and checkout `d43c2d5f89`. Applied them
to `/Users/zach/qemu` and completed `ninja -C build-mac-vhost -j4 qemu-system-aarch64`.
The resulting native ARM64 executable reports version 11.1.50 and lists HVF,
vhost-user, and vmnet backends; the required virtio properties are present. This
updates the build validation only; the Linux guest datapath remains untested here.
