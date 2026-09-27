//! Linux kernel-facing virtual UDP worker pool; see docs/network-lab.md.

#[cfg(all(target_os = "linux", feature = "tun"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use async_net_stack_rs::{
        api::{Action, Service, UdpConfig, UdpPool},
        device::DefaultDevice,
    };

    use clap::Parser;
    use std::{
        net::{Ipv4Addr, SocketAddrV4},
        time::{Duration, Instant},
    };

    #[derive(Parser)]
    struct Args {
        #[arg(long, default_value = "labtun")]
        interface: String,
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=200))]
        workers: u8,
        /// Zero runs until SIGINT/SIGTERM, suitable for a managed service.
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        /// Round robin across remote IP:port pairs.
        #[arg(long)]
        fair_queue: bool,
        #[arg(long, default_value_t = 128)]
        queue_capacity: usize,
        #[arg(long, default_value_t = 128)]
        per_peer_capacity: usize,
        /// Optional global IPv4 byte rate (headers included).
        #[arg(long)]
        bytes_per_second: Option<u64>,
        #[arg(long)]
        per_peer_bytes_per_second: Option<u64>,
        /// Expiry of UDP-owned queue entries; device queues are a separate stage.
        #[arg(long)]
        queue_deadline_ms: Option<u64>,
        /// Collect bounded outcome events; print on shutdown, outside the loop.
        #[arg(long, default_value_t = 0)]
        event_capacity: usize,
    }

    // The launcher terminates us after the client finishes. Preserve counters
    // on that path without performing I/O inside the signal handler.
    use std::sync::atomic::{AtomicBool, Ordering};
    static STOP: AtomicBool = AtomicBool::new(false);
    extern "C" fn stop(_: libc::c_int) {
        STOP.store(true, Ordering::Relaxed);
    }

    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: zeroed sigaction is initialized below, the handler only uses
        // a lock-free atomic, and its code/static storage live for the process.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = stop as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }

    let args = Args::parse();
    let device = DefaultDevice::new(&args.interface)?;
    eprintln!(
        "{}: virtual workers 10.77.0.2..{}:9000, discovery service 7",
        device.name()?,
        args.workers + 1
    );

    let mut pool = UdpPool::with_config(
        device,
        UdpConfig {
            queue_capacity: args.queue_capacity,
            peer_capacity: 256,
            per_peer_capacity: args.per_peer_capacity,
            max_tx_peers: args.queue_capacity,
            tx_budget: args.queue_capacity,
            fair_queue: args.fair_queue,
            bytes_per_second: args.bytes_per_second,
            per_peer_bytes_per_second: args.per_peer_bytes_per_second,
            queue_lifetime: args.queue_deadline_ms.map(Duration::from_millis),
            event_capacity: args.event_capacity,
            ..UdpConfig::default()
        },
    )?;

    for i in 0..args.workers {
        pool.bind(Service {
            address: SocketAddrV4::new(Ipv4Addr::new(10, 77, 0, i + 2), 9000),
            id: 7,
        })?;
    }

    let start = Instant::now();
    while !STOP.load(Ordering::Relaxed)
        && (args.seconds == 0 || start.elapsed() < Duration::from_secs(args.seconds))
    {
        let n = pool.poll(start.elapsed(), Duration::from_secs(5), 64, |_| {
            Action::Echo
        })?;

        if n == 0 && pool.pending() == 0 {
            std::thread::sleep(Duration::from_micros(50));
        } else {
            std::thread::yield_now();
        }
    }

    println!("{:?}; pending={}", pool.stats(), pool.pending());
    while let Some(event) = pool.pop_event() {
        println!("{event:?}");
    }

    Ok(())
}

#[cfg(not(all(target_os = "linux", feature = "tun")))]
fn main() {
    eprintln!("tun_pool requires Linux and the tun feature");
    std::process::exit(1);
}
