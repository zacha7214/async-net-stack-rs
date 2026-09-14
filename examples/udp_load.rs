//! Windowed UDP echo client: sequence verification, loss, duplicates and RTT.
//! The socket stack and this generator can limit load; see docs/benchmarking.md.
use clap::Parser;
use serde_json::json;
use std::collections::VecDeque;
use std::error::Error;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};
#[derive(Parser)]
struct Args {
    #[arg(default_value = "10.9.0.2:9000")]
    target: SocketAddr,
    #[arg(long, default_value_t = 100000)]
    count: usize,
    #[arg(long, default_value_t = 64)]
    window: usize,
    /// UDP payload bytes (excluding IP/UDP/Ethernet).
    #[arg(long, default_value_t = 64)]
    payload: usize,
    #[arg(long, default_value_t = 100)]
    timeout_ms: u64,
    /// Overall bound, including when every send returns EAGAIN.
    #[arg(long, default_value_t = 30)]
    seconds: u64,
}
fn main() -> Result<(), Box<dyn Error>> {
    let a = Args::parse();
    if a.count == 0
        || a.count > 10_000_000
        || a.window == 0
        || a.window > 65536
        || !(8..=65507).contains(&a.payload)
        || a.timeout_ms == 0
        || a.seconds == 0
        || !a.target.is_ipv4()
    {
        return Err("invalid count/window/payload/timeout (IPv4 only)".into());
    }
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect(a.target)?;
    socket.set_nonblocking(true)?;
    let mut payload = vec![0xa5; a.payload];
    let mut rx = vec![0u8; 65536];
    let mut pending = VecDeque::<(usize, Instant)>::with_capacity(a.window);
    let mut sent_at = vec![None; a.count];
    let mut seen = vec![false; a.count];
    let mut rtts = Vec::with_capacity(a.count);
    let mut sent = 0;
    let mut received = 0;
    let mut expired = 0;
    let mut duplicates = 0;
    let mut invalid = 0;
    let start = Instant::now();
    let deadline = start + Duration::from_secs(a.seconds);
    while (sent < a.count || !pending.is_empty()) && Instant::now() < deadline {
        while let Some(&(id, when)) = pending.front() {
            if seen[id] {
                pending.pop_front();
            } else if when.elapsed() >= Duration::from_millis(a.timeout_ms) {
                pending.pop_front();
                sent_at[id] = None;
                expired += 1;
            } else {
                break;
            }
        }
        while pending.len() < a.window && sent < a.count {
            payload[..8].copy_from_slice(&(sent as u64).to_be_bytes());
            let when = Instant::now();
            match socket.send(&payload) {
                Ok(n) if n == payload.len() => {
                    pending.push_back((sent, when));
                    sent_at[sent] = Some(when);
                    sent += 1;
                }
                Ok(_) => return Err("short UDP send".into()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        // Bound receive work so a busy peer cannot defeat the deadline.
        for _ in 0..a.window {
            match socket.recv(&mut rx) {
                Ok(n) => {
                    if n != a.payload || rx[8..n].iter().any(|&v| v != 0xa5) {
                        invalid += 1;
                        continue;
                    }
                    let id = u64::from_be_bytes(rx[..8].try_into().unwrap());
                    if id >= sent as u64 {
                        invalid += 1;
                        continue;
                    }
                    let id = id as usize;
                    if seen[id] {
                        duplicates += 1;
                        continue;
                    }
                    seen[id] = true;
                    received += 1;
                    if let Some(when) = sent_at[id] {
                        rtts.push(when.elapsed().as_nanos() as u64);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        std::hint::spin_loop();
    }
    let elapsed = start.elapsed().as_secs_f64();
    rtts.sort_unstable();
    let percentile = |p| rtts.get((rtts.len().saturating_sub(1) * p) / 100).copied();
    println!(
        "{}",
        json!({"target": a.target.to_string(), "sent": sent, "received": received,
        "lost_at_end": sent - received, "expired_requests": expired, "duplicates": duplicates,
        "invalid": invalid, "unsent": a.count - sent, "elapsed_seconds": elapsed,
        "received_mpps": received as f64 / elapsed / 1e6,
        "rtt_ns": {"samples": rtts.len(), "p50": percentile(50), "p99": percentile(99)}})
    );
    if received == 0 {
        return Err("no valid echo replies received".into());
    }
    Ok(())
}
