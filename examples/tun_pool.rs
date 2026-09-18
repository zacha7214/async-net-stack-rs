//! Linux kernel-facing virtual UDP worker pool; see docs/network-lab.md.
#[cfg(all(target_os = "linux", feature = "tun"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use async_net_stack_rs::{
        api::{Action, Service, UdpPool},
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
        #[arg(long, default_value_t = 30)]
        seconds: u64,
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
    let mut pool = UdpPool::new(device, 128, 256)?;
    for i in 0..args.workers {
        pool.bind(Service {
            address: SocketAddrV4::new(Ipv4Addr::new(10, 77, 0, i + 2), 9000),
            id: 7,
        })?;
    }
    let start = Instant::now();
    while !STOP.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(args.seconds) {
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
    Ok(())
}
#[cfg(not(all(target_os = "linux", feature = "tun")))]
fn main() {
    eprintln!("tun_pool requires Linux and the tun feature");
    std::process::exit(1);
}
