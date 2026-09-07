//! Tests that do not need privileges run unconditionally; the integration
//! tests against a real interface are gated on the `XDP_TEST_IFACE` env var
//! (socket/UMEM creation and bind are unprivileged).

use super::*;
use crate::device::{Device, PacketBuf};

/// End-to-end socket setup on a real interface, without touching XDP
/// program attach (so it can run unprivileged). Run with e.g.
/// `XDP_TEST_IFACE=enp0s1 cargo test --features xdp`.
#[test]
fn umem_and_socket_on_test_iface() {
    let Ok(ifname) = std::env::var("XDP_TEST_IFACE") else {
        eprintln!("skipping: set XDP_TEST_IFACE to a network interface to run this test");
        return;
    };
    let cifname = std::ffi::CString::new(ifname.as_str()).unwrap();
    let ifindex = unsafe { libc::if_nametoindex(cifname.as_ptr()) };
    assert_ne!(ifindex, 0, "no such interface: {ifname}");

    let umem = UMem::new(128, 4096, 0, 128, 128, 0).expect("UMem::new");
    let sock = XskSocket::new(
        &umem,
        ifindex,
        0,
        64,
        64,
        sys::XDP_ZEROCOPY | sys::XDP_USE_NEED_WAKEUP,
    )
    .expect("XskSocket::new (zero-copy or copy fallback)");

    let flags = sock.bind_flags();
    eprintln!(
        "ifindex {ifindex}: bound with {} ({})",
        if flags & sys::XDP_ZEROCOPY != 0 {
            "zero-copy"
        } else {
            "copy mode"
        },
        if sock.need_wakeup_enabled() {
            "need-wakeup granted"
        } else {
            "need-wakeup not granted"
        }
    );
    assert!(flags & (sys::XDP_ZEROCOPY | sys::XDP_COPY) != 0);

    let stats = sock.stats().expect("XDP_STATISTICS");
    eprintln!("stats: {stats:?}");
    drop(sock);
    drop(umem);
}

/// Full data path on a real interface: loads + attaches the default
/// redirect program (needs root / CAP_BPF+CAP_NET_ADMIN) and counts RX.
/// Generate traffic against the interface while it runs, e.g. on a veth:
///
/// ```sh
/// sudo ip link add xdpt0 type veth peer name xdpt1
/// sudo ip link set xdpt0 up && sudo ip link set xdpt1 up
/// sudo ip addr add 10.9.9.1/24 dev xdpt0 && sudo ip addr add 10.9.9.2/24 dev xdpt1
/// ping -c 5 -I xdpt1 10.9.9.1 &
/// sudo XDP_TEST_IFACE=xdpt0 cargo test --features xdp -- --nocapture xdp_device_rx_on_test_iface
/// ```
///
/// Never point this at a production NIC: the redirect-all program takes
/// every packet on the interface.
#[test]
fn xdp_device_rx_on_test_iface() {
    let Ok(ifname) = std::env::var("XDP_TEST_IFACE") else {
        eprintln!("skipping: set XDP_TEST_IFACE to a network interface to run this test");
        return;
    };

    let mut cfg = XdpConfig::default();
    cfg.attach_generic = true; // veth has native XDP but no XSK redirect path
    let mut dev = XdpDevice::with_config(&ifname, 0, &cfg).expect("XdpDevice::with_config");
    eprintln!(
        "attach: {:?}, bind flags: 0x{:x}, need-wakeup: {}",
        dev.attach_mode(),
        dev.bind_flags(),
        dev.need_wakeup_enabled()
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut total = 0usize;
    let mut bufs: Vec<PacketBuf> = Vec::with_capacity(64);
    while std::time::Instant::now() < deadline {
        let n = dev.recv(64, &mut bufs).expect("recv");
        total += n;
        if n > 0 {
            bufs.clear();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let (cfp, fp, fc, d0, d1, d2) = dev.debug_umem().debug_fill_state();
    let (rxc, rxp, rxn) = dev.debug_socket().debug_rx_state();
    eprintln!(
        "fill: cached_prod={cfp} prod={fp} cons={fc} descs=[{d0:#x}, {d1:#x}, {d2:#x}]; \
         rx: cached_cons={rxc} prod={rxp} cons={rxn}"
    );

    let stats = dev.stats().expect("XDP_STATISTICS");
    eprintln!("received {total} packets; stats: {stats:?}");
    assert!(total > 0, "no packets were redirected to the socket");
    drop(dev);
}

/// Isolation test: attach is performed EXTERNALLY (bpftool net attach by
/// prog id, which exercises the classic IFLA_XDP attach path), while this
/// device's own program/map/socket handle the data path.
///
/// ```sh
/// # while the test runs (the program only exists for its duration):
/// sudo bpftool net attach xdpgeneric id $(sudo bpftool prog list \
///     | grep xdp_def_prog | awk '{print $1}' | tr -d :) dev xdpt0
/// ```
#[test]
fn xdp_device_rx_with_c_prog() {
    let Ok(ifname) = std::env::var("XDP_TEST_IFACE") else {
        eprintln!("skipping: set XDP_TEST_IFACE to a network interface to run this test");
        return;
    };

    let mut cfg = XdpConfig::default();
    cfg.attach = false; // attached externally via bpftool
    let mut dev = XdpDevice::with_config(&ifname, 0, &cfg).expect("XdpDevice::with_config");

    // Inject the socket into this device's own XSKMAP (the attached program
    // redirects there).
    let sock_fd = dev.debug_socket().fd();
    let map_fd = dev.debug_prog_map_fd();
    sys::xskmap_set(map_fd, 0, sock_fd).expect("xskmap_set");
    eprintln!("installed socket fd {sock_fd} into own xsks_map");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut total = 0usize;
    let mut bufs: Vec<PacketBuf> = Vec::with_capacity(64);
    while std::time::Instant::now() < deadline {
        let n = dev.recv(64, &mut bufs).expect("recv");
        total += n;
        if n > 0 {
            bufs.clear();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let stats = dev.stats().expect("XDP_STATISTICS");
    eprintln!("received {total} packets; stats: {stats:?}");
    assert!(total > 0, "no packets were redirected to the socket");
    drop(dev);
}
