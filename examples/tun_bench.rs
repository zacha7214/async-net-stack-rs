//! A/B harness: the plain single-fd TUN loop vs the kqueue-driven sharded
//! reactor (macOS only for the reactor; `--mode plain` works on both).
//!
//! Requires elevated privileges and externally configured interfaces (names are
//! printed at startup; the reactor prints one name per shard).
//!
//! ```text
//! cargo run --release --example tun_bench -- --mode plain --duration 10
//! cargo run --release --example tun_bench -- --mode reactor --shards 4 --duration 10
//! ```
//!
//! While it runs, send IP traffic at the printed interface(s) — e.g. from
//! another terminal: `sudo ping -f -s 1400 10.0.0.2` (after `ifconfig`/`ip`),
//! or iperf — and compare the reported Mpps/Gbps between the two modes.

use std::env;
use std::error::Error as StdError;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use async_net_stack_rs::device::{DefaultDevice, Device, PacketBuf};

/// Frames read per batch (matches the device pool size).
const BATCH: usize = 256;

struct Stats {
    packets: AtomicU64,
    bytes: AtomicU64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            packets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }
}

fn main() -> Result<(), Box<dyn StdError>> {
    let args: Vec<String> = env::args().collect();

    let mode = flag(&args, "--mode").unwrap_or_else(|| "reactor".to_string());
    let shards: usize = flag(&args, "--shards")
        .map(|s| s.parse().expect("--shards must be an integer"))
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let duration = Duration::from_secs(
        flag(&args, "--duration")
            .map(|s| s.parse().expect("--duration must be an integer (seconds)"))
            .unwrap_or(10),
    );

    match mode.as_str() {
        "plain" => run_plain(duration)?,
        "reactor" => run_reactor(shards, duration)?,
        other => {
            eprintln!(
                "unknown --mode {other:?}\n\n\
                 usage: tun_bench [--mode plain|reactor] [--shards N] [--duration S]\n\
                 \n\
                 plain   — one utun device, direct recv/send loop (baseline)\n\
                 reactor — kqueue + N pinned shard threads (macOS)"
            );
            std::process::exit(2);
        }
    }
    Ok(())
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Baseline: one device, direct `recv`/`send`, `sleep` when idle — the plain
/// backend exactly as shipped, with no kqueue and no sharding.
fn run_plain(duration: Duration) -> Result<(), Box<dyn StdError>> {
    #[cfg(target_os = "macos")]
    let mut dev = DefaultDevice::new(0)?; // unit 0 = next available utun
    #[cfg(target_os = "linux")]
    let mut dev = DefaultDevice::new("tun0")?;

    println!("plain: echo on {} (mtu {} bytes)", dev.name()?, dev.mtu()?);
    println!("configure the interface, then send it IP traffic");

    let mut frames: Vec<PacketBuf> = Vec::with_capacity(BATCH);
    let mut packets = 0u64;
    let mut bytes = 0u64;
    let start = Instant::now();
    let mut last = start;

    loop {
        let n = dev.recv(BATCH, &mut frames)?;
        if n == 0 {
            // No kqueue here by design: this is the polling baseline.
            thread::sleep(Duration::from_micros(100));
            if !duration.is_zero() && start.elapsed() >= duration {
                break;
            }
            continue;
        }

        for buf in frames.iter() {
            bytes += buf.len() as u64;
        }
        dev.send(&mut frames)?; // echo: all frames back-to-back in one call
        packets += n as u64;

        let dt = last.elapsed().as_secs_f64();
        if dt >= 1.0 {
            print_rate("plain", dt, packets, bytes);
            packets = 0;
            bytes = 0;
            last = Instant::now();
        }

        if !duration.is_zero() && start.elapsed() >= duration {
            break;
        }
    }
    Ok(())
}

/// kqueue + sharded workers: one pinned thread per utun device, batch reads on
/// every `EVFILT_READ` wake, coalesced batch writes in the handler.
#[cfg(target_os = "macos")]
fn run_reactor(shards: usize, duration: Duration) -> Result<(), Box<dyn StdError>> {
    use async_net_stack_rs::device::UtunReactor;

    let mut reactor = UtunReactor::sharded(shards, 1500)?;
    println!(
        "reactor: {} shard(s) on [{}]",
        reactor.names().len(),
        reactor.names().join(", ")
    );
    println!("configure each interface, then send it IP traffic");

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::default());

    // Stop after `duration`, unless it is zero (run until killed).
    let stopper = if duration.is_zero() {
        None
    } else {
        let stop = stop.clone();
        Some(thread::spawn(move || {
            thread::sleep(duration);
            stop.store(true, Ordering::Relaxed);
        }))
    };

    let worker_stats = stats.clone();
    let worker_stop = stop.clone();
    let runner = thread::spawn(move || {
        reactor.run(worker_stop, move |_shard, dev, rx| {
            for buf in rx.iter() {
                worker_stats
                    .bytes
                    .fetch_add(buf.len() as u64, Ordering::Relaxed);
            }
            worker_stats
                .packets
                .fetch_add(rx.len() as u64, Ordering::Relaxed);
            // Echo: coalesced batch write of the whole received batch.
            let _ = dev.send(rx);
        })
    });

    let start = Instant::now();
    let mut last = start;
    let mut prev_packets = 0u64;
    let mut prev_bytes = 0u64;

    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(200));
        let dt = last.elapsed().as_secs_f64();
        if dt >= 1.0 {
            let p = stats.packets.load(Ordering::Relaxed);
            let b = stats.bytes.load(Ordering::Relaxed);
            print_rate("reactor", dt, p - prev_packets, b - prev_bytes);
            prev_packets = p;
            prev_bytes = b;
            last = Instant::now();
        }
    }

    runner
        .join()
        .map_err(|_| io::Error::other("reactor worker thread panicked"))??;
    if let Some(handle) = stopper {
        handle.join().unwrap();
    }

    let total = start.elapsed().as_secs_f64().max(1e-9);
    let p = stats.packets.load(Ordering::Relaxed);
    let b = stats.bytes.load(Ordering::Relaxed);
    println!(
        "summary: {:.2} Mpps, {:.2} Gbps over {total:.1}s",
        p as f64 / total / 1e6,
        b as f64 * 8.0 / total / 1e9
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn run_reactor(_shards: usize, _duration: Duration) -> Result<(), Box<dyn StdError>> {
    Err(io::Error::other("reactor mode is macOS-only (kqueue)").into())
}

fn print_rate(mode: &str, dt: f64, packets: u64, bytes: u64) {
    println!(
        "{mode}: {:>9.2} Mpps  {:>8.2} Gbps (one-way)",
        packets as f64 / dt / 1e6,
        bytes as f64 * 8.0 / dt / 1e9
    );
}
