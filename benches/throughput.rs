//! Baseline throughput benchmarks for the device layer.
//!
//! Run with: `cargo bench` (criterion harness).
//!
//! These measure the in-memory [`LoopbackDevice`], which models the copy-based
//! TUN data path (pool → kernel → pool) with no privileges and no peer. It is
//! the software-path baseline a real TUN device — and later an AF_XDP zero-copy
//! device — should be compared against.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use async_net_stack_rs::device::{Device, LoopbackDevice, PacketBuf};

/// Number of frames moved per round-trip iteration.
const BATCH: usize = 64;

criterion_group!(
    benches,
    bench_alloc_recycle,
    bench_loopback_roundtrip,
    bench_batch_api,
    bench_batched_pipeline
);
criterion_main!(benches);

/// Frame-pool allocator throughput: allocate a batch and recycle it by clearing.
fn bench_alloc_recycle(c: &mut Criterion) {
    let batch = 256usize;
    let mut dev = LoopbackDevice::with_capacity(batch, batch);
    let mut bufs: Vec<PacketBuf> = Vec::with_capacity(batch);

    let mut group = c.benchmark_group("alloc_recycle");
    group.throughput(Throughput::Elements(batch as u64));
    group.bench_function("batch_256", |b| {
        b.iter(|| {
            for _ in 0..batch {
                bufs.push(dev.alloc().unwrap());
            }
            black_box(&bufs);
            // Clearing the batch recycles every frame back to the pool.
            bufs.clear();
        });
    });
    group.finish();
}

/// Full copy-based round trip: build TX frames, `send` (copies into the kernel
/// arena), `recv` (copies back into fresh frames). Reported as round-trip bytes
/// (both directions), so it directly reflects total data movement.
fn bench_loopback_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("loopback_roundtrip");

    for &payload in &[64usize, 512, 1500] {
        group.throughput(Throughput::Bytes((payload * BATCH * 2) as u64));
        group.bench_with_input(
            BenchmarkId::new("copy", payload),
            &payload,
            |b, &payload| {
                // Setup (not timed): pool sized so the full batch round-trips with
                // no drops, plus reusable TX/RX scratch vectors.
                let mut dev = LoopbackDevice::with_capacity(BATCH * 4, BATCH * 4);
                let mut tx: Vec<PacketBuf> = Vec::with_capacity(BATCH);
                let mut rx: Vec<PacketBuf> = Vec::with_capacity(BATCH);

                b.iter(|| roundtrip(&mut dev, &mut tx, &mut rx, payload));
            },
        );
    }

    group.finish();
}

#[inline(never)]
fn roundtrip(
    dev: &mut LoopbackDevice,
    tx: &mut Vec<PacketBuf>,
    rx: &mut Vec<PacketBuf>,
    payload: usize,
) {
    // TX: allocate frames, write payload, hand them to the device.
    tx.clear();
    for _ in 0..BATCH {
        let mut buf = dev.alloc().unwrap();
        let off = buf.data_offset();
        buf.as_mut_slice()[off..off + payload].fill(0xAB);
        buf.set_len(payload);
        tx.push(buf);
    }
    black_box(dev.send(tx).unwrap());

    // RX: receive the echoes back and touch the bytes so the copies are live.
    rx.clear();
    black_box(dev.recv(BATCH, rx).unwrap());
    let mut sum = 0usize;
    for buf in rx.iter() {
        for &b in buf.as_slice() {
            sum += b as usize;
        }
    }
    black_box(sum);
}

/// Compare per-frame allocation against the public batch API. Each operation
/// reports frames/s; payload size has no effect on this metadata-only test.
fn bench_batch_api(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_batch_api");
    for &batch in &[1usize, 8, 32, 64, 256] {
        group.throughput(Throughput::Elements(batch as u64));
        for batched in [false, true] {
            group.bench_with_input(
                BenchmarkId::new(if batched { "batch" } else { "single" }, batch),
                &batch,
                |b, &batch| {
                    let mut dev = LoopbackDevice::with_capacity(batch, batch);
                    let mut bufs = Vec::with_capacity(batch);
                    b.iter(|| {
                        if batched {
                            assert_eq!(dev.alloc_batch(batch, &mut bufs), batch);
                        } else {
                            for _ in 0..batch {
                                bufs.push(dev.alloc().unwrap());
                            }
                        }
                        black_box(&bufs);
                        bufs.clear();
                    });
                },
            );
        }
    }
    group.finish();
}

/// Application-delivered bytes (count each payload once). No full-payload
/// checksum in the timed loop: the legacy roundtrip test above includes it.
fn bench_batched_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("batched_copy_pipeline");
    for &payload in &[64usize, 512, 1500] {
        for &batch in &[1usize, 8, 32, 64, 256] {
            group.throughput(Throughput::Bytes((payload * batch) as u64));
            group.bench_function(BenchmarkId::new(format!("bytes_{payload}"), batch), |b| {
                let mut dev = LoopbackDevice::with_capacity(batch * 2, batch * 2);
                let mut tx = Vec::with_capacity(batch);
                let mut rx = Vec::with_capacity(batch);
                b.iter(|| {
                    tx.clear();
                    rx.clear();
                    assert_eq!(dev.alloc_batch(batch, &mut tx), batch);
                    for buf in &mut tx {
                        buf.set_len(payload);
                        buf.as_mut_packet().fill(0xab);
                    }
                    assert_eq!(dev.send(&mut tx).unwrap(), batch);
                    assert_eq!(dev.recv(batch, &mut rx).unwrap(), batch);
                    for buf in &rx {
                        black_box(buf.as_slice());
                    }
                });
            });
        }
    }
    group.finish();
}
