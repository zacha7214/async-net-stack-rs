//! Minimal TUN echo server: a smoke test and manual baseline for the real
//! device backends.
//!
//! Requires elevated privileges and an externally configured interface:
//!
//! macOS (after starting, use the printed name, e.g. `utun1`):
//!   sudo ifconfig utun1 10.0.0.1 10.0.0.2 up
//!
//! Linux (after starting, use the printed name, e.g. `tun0`):
//!   sudo ip addr add 10.0.0.1/24 dev tun0 && sudo ip link set tun0 up
//!
//! Every IP datagram received is sent straight back; throughput is printed once
//! per second.

use std::error::Error as StdError;
use std::time::{Duration, Instant};

use async_net_stack_rs::device::{DefaultDevice, Device, PacketBuf};

fn main() -> Result<(), Box<dyn StdError>> {
    // For jumbo frames, use `DefaultDevice::new_with_mtu(_, 9000)` and set the
    // interface MTU to match (e.g. `ifconfig utun1 mtu 9000` / `ip link set mtu 9000`).
    #[cfg(target_os = "macos")]
    let mut dev = DefaultDevice::new(0)?; // unit 0 = next available utun
    #[cfg(target_os = "linux")]
    let mut dev = DefaultDevice::new("tun0")?;

    println!(
        "echoing on {} (mtu {} bytes, frame capacity {} bytes)",
        dev.name()?,
        dev.mtu()?,
        dev.frame_size()
    );
    println!("configure the interface, then send it IP traffic (e.g. ping)");

    let mut frames: Vec<PacketBuf> = Vec::with_capacity(64);
    let mut packets = 0u64;
    let mut bytes = 0u64;
    let mut last = Instant::now();

    loop {
        let n = dev.recv(frames.capacity(), &mut frames)?;
        if n == 0 {
            // Non-blocking fd with nothing queued: avoid spinning while idle.
            std::thread::sleep(Duration::from_micros(100));
            continue;
        }

        for buf in frames.iter() {
            bytes += buf.len() as u64;
        }
        dev.send(&mut frames)?;
        packets += n as u64;

        let dt = last.elapsed().as_secs_f64();
        if dt >= 1.0 {
            println!(
                "echoed {packets} packets, {bytes} bytes in {dt:.2}/s: \
                 {:.2} Mpp/s, {:.2} Gbp/s (one-way)",
                packets as f64 / dt / 1e6,
                bytes as f64 * 8.0 / dt / 1e9
            );
            packets = 0;
            bytes = 0;
            last = Instant::now();
        }
    }
}
