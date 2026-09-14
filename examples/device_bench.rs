//! Reproducible device benchmark. Run with --help; see docs/benchmarking.md.
use async_net_stack_rs::device::{Device, LoopbackDevice, PacketBuf};
use async_net_stack_rs::net::{LinkLayer, Responder};
use async_net_stack_rs::transport::udp;
use clap::Parser;
use serde_json::{json, Value};
use std::error::Error;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, default_value = "loopback", value_parser = ["loopback", "tun", "xdp", "uring"])]
    backend: String,
    #[arg(long, default_value = "tx", value_parser = ["rx", "reply", "tx"])]
    action: String,
    #[arg(long, default_value = "tun0")]
    iface: String,
    #[arg(long, default_value_t = 0)]
    queue: u32,
    #[arg(long, default_value = "auto", value_parser = ["auto", "copy", "zero-copy"])]
    mode: String,
    #[arg(long)]
    generic: bool,
    #[arg(long, default_value_t = 64)]
    batch: usize,
    #[arg(long, default_value_t = 4096)]
    frames: usize,
    #[arg(long, default_value_t = 1024)]
    ring: usize,
    #[arg(long, default_value_t = 512)]
    tx_reserve: usize,
    #[arg(long, default_value_t = 128)]
    uring_depth: usize,
    #[arg(long, default_value_t = 64)]
    rx_depth: usize,
    #[arg(long, default_value_t = 64)]
    size: usize,
    #[arg(long, default_value_t = 10.0)]
    seconds: f64,
    #[arg(long, default_value_t = 2.0)]
    warmup: f64,
    #[arg(long, default_value_t = 1500)]
    mtu: usize,
    #[arg(long, default_value = "10.9.0.2")]
    ip: Ipv4Addr,
    #[arg(long, default_value = "10.9.0.1")]
    peer_ip: Ipv4Addr,
    #[arg(long, default_value = "02:00:00:00:00:02")]
    mac: String,
    #[arg(long, default_value = "02:00:00:00:00:01")]
    peer_mac: String,
    #[arg(long, default_value_t = 9000)]
    port: u16,
    #[arg(long)]
    cpu: Option<usize>,
    /// Sleep this many microseconds on empty polls; zero spins.
    #[arg(long, default_value_t = 0)]
    idle_us: u64,
    /// Sample batch-service time, NOT packet latency (adds clock overhead).
    #[arg(long)]
    latency: bool,
}
trait BenchDevice: Device {
    fn progress(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn details(&self) -> Value {
        json!({})
    }
    fn pending_tx(&self) -> usize {
        0
    }
}
impl BenchDevice for LoopbackDevice {}
#[cfg(feature = "tun")]
impl BenchDevice for async_net_stack_rs::device::DefaultDevice {
    fn details(&self) -> Value {
        json!({"interface": self.name().ok(), "io": "read/write"})
    }
}
#[cfg(all(feature = "xdp", target_os = "linux"))]
impl BenchDevice for async_net_stack_rs::device::XdpDevice {
    fn progress(&mut self) -> std::io::Result<()> {
        self.progress();
        Ok(())
    }
    fn pending_tx(&self) -> usize {
        self.pending_tx()
    }
    fn details(&self) -> Value {
        let c = self.counters();
        let kernel = self
            .stats()
            .map(|s| {
                json!({"rx_dropped": s.rx_dropped, "rx_invalid": s.rx_invalid_descs,
            "tx_invalid": s.tx_invalid_descs, "rx_ring_full": s.rx_ring_full,
            "fill_empty": s.rx_fill_ring_empty_descs})
            })
            .unwrap_or(Value::Null);
        json!({"zero_copy": self.is_zero_copy(), "attach": format!("{:?}", self.attach_mode()),
            "need_wakeup": self.need_wakeup_enabled(), "tx_submitted": c.tx_submitted,
            "tx_completed": c.tx_completed, "tx_backpressure": c.tx_backpressure,
            "invalid_descriptors": c.invalid_descriptors, "kernel": kernel})
    }
}
#[cfg(all(feature = "io_uring", target_os = "linux"))]
impl BenchDevice for async_net_stack_rs::device::UringTunDevice {
    fn progress(&mut self) -> std::io::Result<()> {
        self.progress()
    }
    fn pending_tx(&self) -> usize {
        self.pending_tx()
    }
    fn details(&self) -> Value {
        let s = self.stats();
        json!({"interface": self.name(), "enter_calls": s.enter_calls, "rx_submitted": s.rx_submitted,
            "rx_completed": s.rx_completed, "tx_submitted": s.tx_submitted,
            "tx_completed": s.tx_completed, "tx_completed_bytes": s.tx_completed_bytes,
            "rx_errors": s.rx_errors, "tx_errors": s.tx_errors, "submit_errors": s.submit_errors,
            "last_errno": s.last_errno})
    }
}
fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    if args.batch == 0
        || args.batch > 4096
        || !args.seconds.is_finite()
        || args.seconds <= 0.0
        || args.seconds > 86400.0
        || !args.warmup.is_finite()
        || args.warmup < 0.0
        || args.warmup > 3600.0
        || args.size < 42
        || args.size > args.mtu
    {
        return Err("invalid batch/duration/size (42 <= size <= MTU)".into());
    }
    if let Some(cpu) = args.cpu {
        pin_cpu(cpu)?;
    }
    match args.backend.as_str() {
        "loopback" => {
            if args.action != "tx" {
                return Err("loopback uses --action tx (includes receive drain)".into());
            }
            run(
                LoopbackDevice::with_capacity(args.batch * 4, args.batch * 4),
                &args,
                LinkLayer::Ip,
            )
        }
        #[cfg(feature = "tun")]
        "tun" => {
            #[cfg(target_os = "macos")]
            let dev = async_net_stack_rs::device::DefaultDevice::new_with_mtu(0, args.mtu)?;
            #[cfg(target_os = "linux")]
            let dev =
                async_net_stack_rs::device::DefaultDevice::new_with_mtu(&args.iface, args.mtu)?;
            run(dev, &args, LinkLayer::Ip)
        }
        #[cfg(all(feature = "xdp", target_os = "linux"))]
        "xdp" => {
            use async_net_stack_rs::device::{XdpConfig, XdpDevice, XdpMode};
            let cfg = XdpConfig {
                frames: args.frames,
                rx_entries: args.ring,
                tx_entries: args.ring,
                fill_entries: args.ring,
                cq_entries: args.ring,
                batch_size: args.batch,
                tx_reserve: args.tx_reserve,
                attach_generic: args.generic,
                mode: match args.mode.as_str() {
                    "copy" => XdpMode::Copy,
                    "zero-copy" => XdpMode::ZeroCopy,
                    _ => XdpMode::Auto,
                },
                ..XdpConfig::default()
            };
            run(
                XdpDevice::with_config(&args.iface, args.queue, &cfg)?,
                &args,
                LinkLayer::Ethernet,
            )
        }
        #[cfg(all(feature = "io_uring", target_os = "linux"))]
        "uring" => {
            use async_net_stack_rs::device::{UringConfig, UringTunDevice};
            let cfg = UringConfig {
                entries: args.uring_depth,
                rx_depth: args.rx_depth,
            };
            run(
                UringTunDevice::new(&args.iface, args.mtu, cfg)?,
                &args,
                LinkLayer::Ip,
            )
        }
        _ => Err("backend unavailable: enable tun/xdp/io_uring on its supported OS".into()),
    }
}
fn mac(s: &str) -> Result<[u8; 6], Box<dyn Error>> {
    let bytes: Vec<u8> = s
        .split(':')
        .map(|n| u8::from_str_radix(n, 16))
        .collect::<Result<_, _>>()?;
    Ok(bytes
        .try_into()
        .map_err(|_| "MAC must contain six octets")?)
}
fn pin_cpu(cpu: usize) -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    {
        if cpu >= libc::CPU_SETSIZE as usize {
            return Err("CPU exceeds CPU_SETSIZE".into());
        }
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::CPU_ZERO(&mut set);
            libc::CPU_SET(cpu, &mut set);
        }
        if unsafe { libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cpu;
        Err("exact CPU affinity is supported only on Linux".into())
    }
}
fn run<D: BenchDevice>(mut dev: D, args: &Args, link: LinkLayer) -> Result<(), Box<dyn Error>> {
    let responder = Responder {
        ipv4: args.ip.octets(),
        mac: mac(&args.mac)?,
        udp_port: Some(args.port),
    };
    let bytes = if args.action == "tx" {
        let mut template = dev.alloc().ok_or("no TX frame")?;
        if args.size > template.tail_capacity() {
            return Err("size exceeds backend payload capacity".into());
        }
        let l2 = if link == LinkLayer::Ethernet { 14 } else { 0 };
        udp::build_ipv4(
            &mut template,
            (args.ip.octets(), 12345),
            (args.peer_ip.octets(), args.port),
            &vec![0xa5; args.size - 28 - l2],
            0,
        )?;
        let mut bytes = Vec::with_capacity(args.size);
        if l2 > 0 {
            bytes.extend(mac(&args.peer_mac)?);
            bytes.extend(responder.mac);
            bytes.extend([8, 0]);
        }
        bytes.extend(template.as_slice());
        drop(template);
        bytes
    } else {
        Vec::new()
    };
    eprintln!(
        "configure the peer now; warmup={}s; device={}",
        args.warmup,
        dev.details()
    );
    let mut tx: Vec<PacketBuf> = Vec::with_capacity(args.batch);
    let mut rx: Vec<PacketBuf> = Vec::with_capacity(args.batch);
    let mut latencies = Vec::with_capacity(65536);
    let mut before = Value::Null;
    let mut measured = Value::Null;
    for (warmup, duration) in [(true, args.warmup), (false, args.seconds)] {
        if !warmup {
            before = dev.details();
        }
        let start = Instant::now();
        let deadline = start + Duration::from_secs_f64(duration);
        let (mut rx_packets, mut rx_bytes, mut tx_packets, mut tx_bytes) = (0u64, 0u64, 0u64, 0u64);
        let (mut short_sends, mut empty_polls, mut ignored) = (0u64, 0u64, 0u64);
        while Instant::now() < deadline {
            let sample = (!warmup && args.latency && latencies.len() < latencies.capacity())
                .then(Instant::now);
            let mut work = 0;
            if args.action == "tx" && tx.is_empty() {
                dev.alloc_batch(args.batch, &mut tx);
                for buf in &mut tx {
                    if bytes.len() > buf.tail_capacity() {
                        return Err("TX template exceeds buffer".into());
                    }
                    buf.set_len(bytes.len());
                    buf.as_mut_packet().copy_from_slice(&bytes);
                }
            }
            if !tx.is_empty() {
                // Count accepted bytes before send empties those handles.
                let lens: usize = tx.iter().map(PacketBuf::len).sum();
                let sent = dev.send(&mut tx)?;
                let unsent: usize = tx[sent..].iter().map(PacketBuf::len).sum();
                tx_packets += sent as u64;
                tx_bytes += (lens - unsent) as u64;
                if sent < tx.len() {
                    short_sends += 1;
                }
                tx.drain(..sent); // keep and retry suffix, bounded by batch
                work += sent;
            }
            if args.action != "tx" || args.backend == "loopback" {
                // Preserve replies under backpressure before receiving more.
                if tx.is_empty() {
                    let n = dev.recv(args.batch, &mut rx)?;
                    work += n;
                    rx_packets += n as u64;
                    rx_bytes += rx.iter().map(|p| p.len() as u64).sum::<u64>();
                    if args.action == "reply" {
                        for mut packet in rx.drain(..) {
                            if responder.respond(&mut packet, link).is_reply() {
                                tx.push(packet);
                            } else {
                                ignored += 1;
                            }
                        }
                    } else {
                        rx.clear();
                    }
                }
            }
            dev.progress()?;
            if let Some(sample) = sample {
                latencies.push(sample.elapsed().as_nanos() as u64);
            }
            if work == 0 {
                empty_polls += 1;
                if args.idle_us > 0 {
                    std::thread::sleep(Duration::from_micros(args.idle_us));
                } else {
                    std::hint::spin_loop();
                }
            }
        }
        if !warmup {
            let elapsed = start.elapsed().as_secs_f64();
            measured = json!({"elapsed_seconds": elapsed, "rx_packets": rx_packets, "rx_bytes": rx_bytes,
                "tx_accepted": tx_packets, "tx_accepted_bytes": tx_bytes, "short_sends": short_sends,
                "rx_mpps": rx_packets as f64 / elapsed / 1e6, "rx_gbps": rx_bytes as f64 * 8.0 / elapsed / 1e9,
                "tx_accepted_mpps": tx_packets as f64 / elapsed / 1e6,
                "empty_polls": empty_polls, "ignored_or_malformed": ignored,
                "unsent_at_end": tx.len(), "pending_tx_at_end": dev.pending_tx() });
        }
    }
    let after = dev.details();
    let drain = Instant::now() + Duration::from_millis(250);
    while dev.pending_tx() > 0 && Instant::now() < drain {
        dev.progress()?;
    }
    latencies.sort_unstable();
    let percentile = |q: usize| {
        latencies
            .get((latencies.len().saturating_sub(1) * q) / 100)
            .copied()
    };
    let kernel = std::process::Command::new("uname")
        .arg("-sr")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    println!(
        "{}",
        json!({"backend": args.backend, "action": args.action, "kernel": kernel,
        "arch": std::env::consts::ARCH, "batch": args.batch, "configured_tx_size": args.size,
        "warmup_seconds": args.warmup, "cpu": args.cpu, "idle_us": args.idle_us,
        "configuration": {"iface": args.iface, "queue": args.queue, "frames": args.frames,
            "ring_entries": args.ring, "tx_reserve": args.tx_reserve, "requested_mode": args.mode,
            "generic": args.generic, "uring_depth": args.uring_depth, "rx_depth": args.rx_depth,
            "mtu": args.mtu, "ip": args.ip.to_string(), "peer_ip": args.peer_ip.to_string(),
            "mac": args.mac, "peer_mac": args.peer_mac, "udp_port": args.port},
        "measurement": measured, "before": before, "after": after, "after_drain": dev.details(),
        "pending_tx_after_drain": dev.pending_tx(),
        "batch_service_ns": {"samples": latencies.len(), "p50": percentile(50), "p99": percentile(99)} })
    );
    Ok(())
}
