//! ICMP/UDP echo over Linux TUN or macOS utun. Configure the printed interface:
//! Linux: sudo ip addr add 10.9.0.1/24 dev tun0; sudo ip link set tun0 up
//! macOS: sudo ifconfig utunN 10.9.0.1 10.9.0.2 up
//! Then ping 10.9.0.2 or run udp_load against 10.9.0.2:9000.
use async_net_stack_rs::device::{DefaultDevice, Device, PacketBuf};
use async_net_stack_rs::net::{LinkLayer, Responder};
use std::error::Error;
use std::os::fd::AsRawFd;
fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "macos")]
    let mut dev = DefaultDevice::new(0)?;
    #[cfg(target_os = "linux")]
    let mut dev = DefaultDevice::new("tun0")?;
    eprintln!(
        "{}: stack IP 10.9.0.2, ICMP echo and UDP port 9000",
        dev.name()?
    );
    let responder = Responder {
        ipv4: [10, 9, 0, 2],
        mac: [0; 6],
        udp_port: Some(9000),
    };
    let mut rx: Vec<PacketBuf> = Vec::with_capacity(64);
    let mut tx = Vec::with_capacity(64);
    loop {
        if !tx.is_empty() {
            let sent = dev.send(&mut tx)?;
            tx.drain(..sent);
        }
        if tx.is_empty() {
            dev.recv(64, &mut rx)?;
            for mut packet in rx.drain(..) {
                if responder.respond(&mut packet, LinkLayer::Ip).is_reply() {
                    tx.push(packet);
                }
            }
        }
        if tx.is_empty() {
            let mut pfd = libc::pollfd {
                fd: dev.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, 100) } < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    return Err(err.into());
                }
            }
        } else {
            std::thread::yield_now();
        }
    }
}
