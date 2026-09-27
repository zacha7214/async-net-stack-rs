//! Native UDP DNS lookup over TUN. See docs/dns.md for an isolated Linux lab.
#[cfg(all(feature = "tun", any(target_os = "linux", target_os = "macos")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use async_net_stack_rs::{
        device::DefaultDevice,
        dns::{NetworkConfig, Resolver, ResolverConfig, UdpResolver},
    };
    use clap::Parser;
    use std::{
        net::{Ipv4Addr, SocketAddrV4},
        time::{Duration, Instant},
    };

    #[derive(Parser)]
    struct Args {
        #[arg(default_value = "example.test")]
        name: String,
        #[cfg(target_os = "linux")]
        #[arg(long, default_value = "labtun")]
        iface: String,
        #[cfg(target_os = "macos")]
        #[arg(long, default_value_t = 0)]
        utun_unit: u32,
        #[arg(long, default_value = "10.77.0.2")]
        local: Ipv4Addr,
        #[arg(long, default_value = "10.80.2.1:53")]
        server: Vec<SocketAddrV4>,
    }

    let args = Args::parse();
    #[cfg(target_os = "linux")]
    let device = DefaultDevice::new(&args.iface)?;
    #[cfg(target_os = "macos")]
    let device = DefaultDevice::new(args.utun_unit)?;
    eprintln!(
        "DNS TUN interface: {} (routes/addresses must already be configured)",
        device.name()?
    );

    // Applications depend on Resolver, not the concrete UDP engine. Another
    // implementation can replace this constructor without changing the loop.
    let mut resolver: Box<dyn Resolver> = Box::new(UdpResolver::new(
        device,
        NetworkConfig {
            local_address: args.local,
            servers: args.server,
        },
        ResolverConfig::default(),
    )?);

    let start = Instant::now();
    resolver.poll(start.elapsed())?;

    let id = resolver
        .resolve(&args.name)
        .map_err(|e| std::io::Error::other(format!("DNS submission: {e:?}")))?;
    loop {
        resolver.poll(start.elapsed())?;
        if let Some(completion) = resolver.pop_result() {
            if completion.id == id {
                match completion.result {
                    Ok(answer) => {
                        println!(
                            "{}: {:?}; expires at {:?}",
                            answer.canonical_name, answer.addresses, answer.expires_at
                        );
                        return Ok(());
                    }
                    Err(error) => {
                        return Err(std::io::Error::other(format!("DNS lookup: {error:?}")).into())
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(not(all(feature = "tun", any(target_os = "linux", target_os = "macos"))))]
fn main() {
    eprintln!("dns_lookup requires the tun feature on Linux or macOS");
    std::process::exit(1);
}
