//! UDP over the userspace Ethernet/ARP/routing adapter. Use an isolated NIC.
#[cfg(all(target_os = "linux", feature = "xdp"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use async_net_stack_rs::{
        api::{Action, Service, UdpPool},
        device::{XdpConfig, XdpDevice, XdpMode},
        net::{EthernetIpv4, InterfaceConfig},
    };
    use clap::Parser;
    use std::{
        net::{Ipv4Addr, SocketAddrV4},
        time::{Duration, Instant},
    };

    #[derive(Parser)]
    struct Args {
        #[arg(long)]
        iface: String,
        #[arg(long)]
        ip: Ipv4Addr,
        #[arg(long, default_value_t = 24)]
        prefix: u8,
        /// The selected interface's Ethernet MAC, as six colon-separated octets.
        #[arg(long, value_parser = parse_mac)]
        mac: [u8; 6],
        #[arg(long)]
        gateway: Option<Ipv4Addr>,
        /// Optional remote endpoint to initiate a UDP request instead of only echoing.
        #[arg(long)]
        peer: Option<SocketAddrV4>,
        #[arg(long, default_value_t = 9000)]
        port: u16,
        #[arg(long, default_value_t = 0)]
        queue: u32,
        #[arg(long)]
        generic: bool,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
    }

    let args = Args::parse();
    let device = XdpDevice::with_config(
        &args.iface,
        args.queue,
        &XdpConfig {
            mode: XdpMode::Copy,
            attach_generic: args.generic,
            ..XdpConfig::default()
        },
    )?;
    let mut config = InterfaceConfig::new(args.ip, args.prefix, args.mac);
    config.event_capacity = 128;
    let mut interface = EthernetIpv4::new(device, config)?;
    if let Some(gateway) = args.gateway {
        interface.set_gateway(gateway)?;
    }
    let mut pool = UdpPool::new(interface, 128, 64)?;
    let local = SocketAddrV4::new(args.ip, args.port);
    pool.bind(Service {
        address: local,
        id: 7,
    })?;
    if let Some(peer) = args.peer {
        pool.send_to(local, peer, b"hello through ARP")?;
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(args.seconds) {
        let now = start.elapsed();
        pool.device_mut().advance(now)?;
        pool.poll(now, Duration::from_secs(5), 64, |datagram| {
            if args.peer.is_some() {
                println!(
                    "{}: {}",
                    datagram.source,
                    String::from_utf8_lossy(datagram.payload)
                );
                Action::Ignore // Do not create an echo loop with the remote responder.
            } else {
                Action::Echo
            }
        })?;
        if pool.pending() == 0 && pool.device_mut().pending() == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    println!(
        "UDP: {:?}; Ethernet/IP: {:?}",
        pool.stats(),
        pool.device_mut().stats()
    );
    while let Some((at, event)) = pool.device_mut().pop_event() {
        println!("{at:?} {event:?}");
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "xdp"))]
fn parse_mac(value: &str) -> Result<[u8; 6], String> {
    let octets = value
        .split(':')
        .map(|p| u8::from_str_radix(p, 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "invalid MAC address".to_owned())?;
    octets
        .try_into()
        .map_err(|_| "MAC needs six octets".to_owned())
}

#[cfg(not(all(target_os = "linux", feature = "xdp")))]
fn main() {
    eprintln!("xdp_pool requires Linux and --features xdp");
    std::process::exit(1);
}
