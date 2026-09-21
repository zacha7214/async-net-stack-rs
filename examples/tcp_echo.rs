//! Initial TCP echo server over TUN or AF_XDP + userspace Ethernet/ARP/routing.
//!
//! Linux TUN: cargo run --features tun --example tcp_echo -- --iface labtun
//! Configure the printed TUN interface from another shell, as for echo_server.
//! Isolated XDP NIC: add --backend xdp --iface eth1 --mac 02:00:00:00:00:20
//! --ip 192.168.1.20 --prefix 24 [--gateway 192.168.1.1] [--generic].
//! Connect with a kernel TCP client (e.g. nc ADDRESS 9000). This example is not
//! a throughput benchmark; the initial TCP sender has one segment in flight.
use async_net_stack_rs::{
    api::tcp::{ConnectionId, TcpConfig, TcpPool},
    device::Device,
};
use clap::Parser;
use std::{
    collections::BTreeMap,
    io,
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "tun")]
    backend: String,
    #[arg(long, default_value = "labtun")]
    iface: String,
    #[arg(long, default_value = "10.9.0.2")]
    ip: Ipv4Addr,
    #[arg(long, default_value_t = 24)]
    prefix: u8,
    #[arg(long)]
    mac: Option<String>,
    #[arg(long)]
    gateway: Option<Ipv4Addr>,
    #[arg(long, default_value_t = 0)]
    queue: u32,
    #[arg(long)]
    generic: bool,
    #[arg(long, default_value_t = 9000)]
    port: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    match args.backend.as_str() {
        #[cfg(all(feature = "tun", any(target_os = "linux", target_os = "macos")))]
        "tun" => {
            let device = async_net_stack_rs::device::DefaultDevice::new(&args.iface)?;
            eprintln!(
                "TUN interface: {}; configure its address/link before connecting",
                device.name()?
            );
            run(device, &args)?;
        }
        #[cfg(all(feature = "xdp", target_os = "linux"))]
        "xdp" => {
            use async_net_stack_rs::{
                device::{XdpConfig, XdpDevice, XdpMode},
                net::{EthernetIpv4, InterfaceConfig},
            };
            let mac = parse_mac(args.mac.as_deref().ok_or("--mac is required with xdp")?)?;
            let device = XdpDevice::with_config(
                &args.iface,
                args.queue,
                &XdpConfig {
                    attach_generic: args.generic,
                    mode: XdpMode::Copy,
                    ..XdpConfig::default()
                },
            )?;
            let mut config = InterfaceConfig::new(args.ip, args.prefix, mac);
            config.event_capacity = 128;
            let mut interface = EthernetIpv4::new(device, config)?;
            if let Some(gateway) = args.gateway {
                interface.set_gateway(gateway)?;
            }
            run(interface, &args)?;
        }
        _ => {
            return Err(
                "backend unavailable: select tun, or xdp on Linux with its Cargo feature".into(),
            )
        }
    }
    Ok(())
}

struct Echo {
    pending: Vec<u8>,
    offset: usize,
    eof: bool,
}

fn run<D: Device>(device: D, args: &Args) -> io::Result<()> {
    let mut tcp = TcpPool::new(device, TcpConfig::default())?;
    let local = SocketAddrV4::new(args.ip, args.port);
    tcp.listen(local)?;
    let mut echoes: BTreeMap<ConnectionId, Echo> = BTreeMap::new();
    let start = Instant::now();
    let mut bytes = [0; 4096];
    eprintln!("TCP echo listening on {local}");
    loop {
        tcp.poll(start.elapsed(), 64)?;
        while let Some(id) = tcp.accept(local)? {
            eprintln!("accepted {:?}", tcp.status(id)?);
            echoes.insert(
                id,
                Echo {
                    pending: Vec::with_capacity(bytes.len()),
                    offset: 0,
                    eof: false,
                },
            );
        }
        let mut finished = Vec::new();
        for (&id, echo) in &mut echoes {
            if tcp.status(id)?.state.is_terminal() {
                finished.push(id);
                continue;
            }
            if echo.offset == echo.pending.len() && !echo.eof {
                match tcp.read(id, &mut bytes) {
                    Ok(0) => echo.eof = true,
                    Ok(n) => {
                        echo.pending.clear();
                        echo.pending.extend_from_slice(&bytes[..n]);
                        echo.offset = 0;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
            if echo.offset < echo.pending.len() {
                match tcp.write(id, &echo.pending[echo.offset..]) {
                    Ok(n) => echo.offset += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
            if echo.eof && echo.offset == echo.pending.len() {
                tcp.close(id)?;
            }
        }
        for id in finished {
            eprintln!("finished {:?}", tcp.status(id)?);
            tcp.remove(id)?;
            echoes.remove(&id);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(all(feature = "xdp", target_os = "linux"))]
fn parse_mac(value: &str) -> Result<[u8; 6], Box<dyn std::error::Error>> {
    let bytes = value
        .split(':')
        .map(|part| u8::from_str_radix(part, 16))
        .collect::<Result<Vec<_>, _>>()?;
    bytes
        .try_into()
        .map_err(|_| "MAC needs six hexadecimal octets".into())
}
