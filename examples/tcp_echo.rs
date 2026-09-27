//! Initial TCP echo server over TUN or AF_XDP + userspace Ethernet/ARP/routing.
//!
//! Linux TUN: cargo run --features tun --example tcp_echo -- --iface labtun
//! Configure the printed TUN interface from another shell, as for echo_server.
//! Isolated XDP NIC: add --backend xdp --iface eth1 --mac 02:00:00:00:00:20
//! --ip 192.168.1.20 --prefix 24 [--gateway 192.168.1.1] [--generic].
//! Connect with a kernel TCP client (e.g. nc ADDRESS 9000). This example is not
//! a throughput benchmark (the loop sleeps 1 ms). Use --help for window limits.
//! --verbose-state collects bounded transition events and prints batches every
//! --state-report-ms milliseconds. Console output still affects measurements;
//! leave it disabled for timing runs and use TcpPool::status for sampled metrics.
use async_net_stack_rs::{
    api::tcp::{ConnectionId, TcpConfig, TcpPool},
    device::Device,
    telemetry::{Telemetry, TelemetryConfig},
};

use clap::Parser;
use std::{
    collections::BTreeMap,
    io::{self, Write},
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

    /// Client PC UDP collector address, e.g. 192.168.1.10:9900.
    #[arg(long)]
    telemetry_client: Option<std::net::SocketAddr>,
    /// Optional host management-interface source address and port.
    #[arg(long)]
    telemetry_bind: Option<std::net::SocketAddr>,
    #[arg(long, default_value_t = 16384)]
    send_capacity: usize,
    #[arg(long, default_value_t = 16384)]
    receive_capacity: usize,

    /// Unscaled advertised receive-window ceiling (1..=65535 bytes).
    #[arg(long, default_value_t = 65535)]
    receive_window: u16,
    #[arg(long, default_value_t = 536)]
    mss: u16,

    /// Initial cwnd in MSS units (1..=4, further bounded for large MSS).
    #[arg(long, default_value_t = 2)]
    initial_cwnd_segments: u16,
    #[arg(long, default_value_t = 16384)]
    max_cwnd_bytes: usize,
    #[arg(long, default_value_t = 65535)]
    initial_ssthresh_bytes: usize,
    #[arg(long, default_value_t = 128)]
    max_inflight_segments: usize,
    #[arg(long, default_value_t = 32)]
    tx_burst: usize,

    #[arg(long)]
    verbose_state: bool,
    #[arg(long, default_value_t = 256)]
    event_capacity: usize,
    #[arg(long, default_value_t = 250)]
    state_report_ms: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    match args.backend.as_str() {
        #[cfg(all(feature = "tun", any(target_os = "linux", target_os = "macos")))]
        "tun" => {
            let device =
                async_net_stack_rs::device::DefaultDevice::new(args.iface.parse::<u32>()?)?;
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
    if (args.verbose_state || args.telemetry_client.is_some()) && args.state_report_ms == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state-report-ms must be positive",
        ));
    }

    let mut tcp = TcpPool::new(
        device,
        TcpConfig {
            send_capacity: args.send_capacity,
            receive_capacity: args.receive_capacity,
            receive_window_limit: args.receive_window,
            mss: args.mss,
            initial_cwnd_segments: args.initial_cwnd_segments,
            max_cwnd_bytes: args.max_cwnd_bytes,
            initial_ssthresh_bytes: args.initial_ssthresh_bytes,
            max_inflight_segments: args.max_inflight_segments,
            tx_burst: args.tx_burst,
            verbose_state: args.verbose_state || args.telemetry_client.is_some(),
            event_capacity: args.event_capacity,
            state_report_interval: Duration::from_millis(args.state_report_ms),
            ..TcpConfig::default()
        },
    )?;

    let telemetry = args
        .telemetry_client
        .map(|client| {
            let mut config = TelemetryConfig::new(client);
            if let Some(bind) = args.telemetry_bind {
                config.bind = bind;
            }
            Telemetry::start(config)
        })
        .transpose()?;

    let mut reports = io::BufWriter::new(io::stderr());
    let report_interval = Duration::from_millis(args.state_report_ms);
    let mut next_report = report_interval;
    let mut last_overwritten = 0;
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

        let now = start.elapsed();
        if let Some(exporter) = &telemetry {
            exporter.progress(tcp.stats().events_overwritten);

            // Bound work per loop; keep all formatting and transmission elsewhere.
            for _ in 0..64 {
                let Some(event) = tcp.pop_event() else {
                    break;
                };
                exporter.record(event);
            }
        }

        // Remote mode takes precedence over synchronous console diagnostics.
        if telemetry.is_none() && args.verbose_state && now >= next_report {
            while let Some(event) = tcp.pop_event() {
                writeln!(
                    reports,
                    "TCP {:?} {:?}: {:?}",
                    event.at, event.connection, event.status
                )?;
            }

            let overwritten = tcp.stats().events_overwritten;
            if overwritten != last_overwritten {
                writeln!(
                    reports,
                    "TCP state events overwritten: {}",
                    overwritten - last_overwritten
                )?;
                last_overwritten = overwritten;
            }

            reports.flush()?;
            next_report = now.saturating_add(report_interval);
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
