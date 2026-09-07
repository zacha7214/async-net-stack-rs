//! Probe AF_XDP feature support on an interface, the Rust counterpart of
//! `xdp-c-ref/src/xdp_caps.c`.
//!
//! ```sh
//! cargo run --features xdp --example xdp_probe -- enp0s1
//! ```
//!
//! Everything exercised here is unprivileged (no BPF load, no program
//! attach): kernel version, driver, queue count, unaligned-chunk support,
//! and the zero-copy vs copy-mode bind probe.

#[cfg(all(target_os = "linux", feature = "xdp"))]
fn main() {
    use std::os::raw::c_char;

    use async_net_stack_rs::device::{
        UMem, XskSocket, XDP_COPY, XDP_UMEM_UNALIGNED_CHUNK_FLAG, XDP_USE_NEED_WAKEUP,
        XDP_ZEROCOPY,
    };

    let ifname = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "enp0s1".to_string());

    // Kernel release.
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    let release = if unsafe { libc::uname(&mut uts) } == 0 {
        unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr() as *const c_char) }
            .to_string_lossy()
            .into_owned()
    } else {
        "unknown".to_string()
    };
    println!("kernel:         {release}");

    // ifindex.
    let cifname = std::ffi::CString::new(ifname.as_str()).expect("interface name");
    let ifindex = unsafe { libc::if_nametoindex(cifname.as_ptr()) };
    if ifindex == 0 {
        eprintln!("ERROR: no such interface \"{ifname}\"");
        std::process::exit(1);
    }
    println!("interface:      {ifname} (ifindex {ifindex})");

    // Driver (via sysfs).
    let driver_path = format!("/sys/class/net/{ifname}/device/driver");
    match std::fs::read_link(&driver_path) {
        Ok(p) => println!(
            "driver:         {}",
            p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
        ),
        Err(_) => println!("driver:         virtual (no driver)"),
    }

    // RX queue count (via sysfs).
    let queues = format!("/sys/class/net/{ifname}/queues");
    match std::fs::read_dir(&queues) {
        Ok(entries) => {
            let count = entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|n| n.starts_with("rx-"))
                        .unwrap_or(false)
                })
                .count();
            println!("rx queues:      {count}");
        }
        Err(_) => println!("rx queues:      unknown (no /sys/class/net entry)"),
    }

    // Unaligned-chunk probe: a non-power-of-two chunk size is only legal
    // with XDP_UMEM_UNALIGNED_CHUNK_FLAG (kernel ~5.16+).
    match UMem::new(64, 3000, 0, 64, 64, XDP_UMEM_UNALIGNED_CHUNK_FLAG) {
        Ok(umem) => {
            println!("unaligned umem: SUPPORTED (XDP_UMEM_UNALIGNED_CHUNK_FLAG accepted)");
            drop(umem);
        }
        Err(e) => println!("unaligned umem: not supported ({e})"),
    }

    // Zero-copy vs copy probe.
    let umem = match UMem::new(128, 4096, 0, 128, 128, 0) {
        Ok(umem) => umem,
        Err(e) => {
            eprintln!("ERROR: cannot create UMEM: {e}");
            std::process::exit(1);
        }
    };

    match XskSocket::new(&umem, ifindex, 0, 64, 64, XDP_ZEROCOPY | XDP_USE_NEED_WAKEUP) {
        Ok(sock) => {
            let flags = sock.bind_flags();
            if flags & XDP_ZEROCOPY != 0 {
                print!("zero-copy:      SUPPORTED (bind with XDP_ZEROCOPY ok)");
                match sock.options() {
                    Ok(opts) if opts & (1 << 0) != 0 => {
                        println!("; kernel confirms zero-copy active (XDP_OPTIONS_ZEROCOPY)")
                    }
                    _ => println!(),
                }
            } else {
                assert!(flags & XDP_COPY != 0);
                println!("zero-copy:      NOT SUPPORTED on this device/queue (fell back to XDP_COPY)");
                println!("copy mode:      WORKS (bind ok)");
            }
            println!(
                "need-wakeup:    {}",
                if sock.need_wakeup_enabled() {
                    "granted (XDP_USE_NEED_WAKEUP)"
                } else {
                    "not granted (always kick)"
                }
            );
            match sock.stats() {
                Ok(stats) => println!("stats:          {stats:?}"),
                Err(e) => println!("stats:          unavailable ({e})"),
            }
            drop(sock);
        }
        Err(e) => {
            println!("zero-copy:      bind failed unexpectedly ({e})");
            println!("copy mode:      failed (see error above)");
        }
    }
    drop(umem);

    println!(
        "\n(no XDP program was attached; run `xdp-c-ref` or attach a \
         redirect program to exercise the data path)"
    );
}

#[cfg(not(all(target_os = "linux", feature = "xdp")))]
fn main() {
    eprintln!("xdp_probe requires Linux and `--features xdp`");
}
